//! The sandbox manager provides the actual functionality of the sandbox
//! process. It allows the replica controller process to manage
//! everything required in order to execute code. It holds three
//! kinds of resources that it manages on behalf of the replica
//! controller process:
//!
//! - CanisterWasm: The (wasm) code corresponding to one canister
//! - State: The heap and other (mutable) user state associated with a canister
//! - Execution: An ongoing execution of a canister, using one wasm and state
//!   object
//!
//! All of the above objects as well as the functionality provided
//! towards the controller are found in this module.
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};

use ic_canister_sandbox_common::protocol::id::{ExecId, MemoryId, WasmId};
use ic_canister_sandbox_common::protocol::sbxsvc::{
    CreateExecutionStateSuccessReply, OpenMemoryRequest,
};
use ic_canister_sandbox_common::protocol::structs::{
    MemoryModifications, SandboxExecInput, SandboxExecOutput, StateModifications,
};
use ic_canister_sandbox_common::{controller_service::ControllerService, protocol};
use ic_config::embedders::{Config, PersistenceType};
use ic_embedders::wasm_executor::DirtyPageIndices;
use ic_embedders::WasmExecutionOutput;
use ic_embedders::{
    wasm_utils::{
        instrumentation::{instrument, InstructionCostTable},
        validation::validate_wasm_binary,
    },
    WasmtimeEmbedder,
};
use ic_interfaces::execution_environment::{HypervisorError, HypervisorResult};
use ic_logger::replica_logger::no_op_logger;
use ic_replicated_state::page_map::PageMapSerialization;
use ic_replicated_state::{EmbedderCache, Memory, PageMap};
use ic_types::CanisterId;
use ic_wasm_types::BinaryEncodedWasm;

use crate::system_state_accessor_rpc::SystemStateAccessorRPC;

struct ExecutionInstantiateError;

impl Debug for ExecutionInstantiateError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Failed to instantatiate execution.")
    }
}

/// A canister execution currently in progress.
struct Execution {
    /// Id of the execution. This is used in communicating back to
    /// the replica (e.g. for syscalls) such that replica can associate
    /// events with the correct execution.
    exec_id: ExecId,

    /// The canister wasm used in this execution.
    canister_wasm: Arc<CanisterWasm>,

    /// The sandbox manager that is responsible for
    /// 1) Providing the controller to talk to the replica process.
    /// 2) Creating a new execution state.
    sandbox_manager: Arc<SandboxManager>,
}

impl Execution {
    /// Creates new execution based on canister wasm and state. In order
    /// to start the execution, the given state object will be "locked" --
    /// if that cannot be done, then creation of execution will fail.
    /// The actual code to be run will be scheduled to the given
    /// thread pool.
    ///
    /// This will *actually* schedule and initiate a new execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_on_worker_thread(
        exec_id: ExecId,
        canister_wasm: Arc<CanisterWasm>,
        wasm_memory: Arc<Memory>,
        stable_memory: Arc<Memory>,
        sandbox_manager: Arc<SandboxManager>,
        workers: &mut threadpool::ThreadPool,
        exec_input: SandboxExecInput,
        total_timer: std::time::Instant,
    ) {
        let wasm_memory = (*wasm_memory).clone();
        let stable_memory = (*stable_memory).clone();

        let execution = Arc::new(Self {
            exec_id,
            canister_wasm,
            sandbox_manager,
        });

        workers.execute(move || execution.run(exec_input, wasm_memory, stable_memory, total_timer));
    }

    // Actual wasm code execution -- this is run on the target thread
    // in the thread pool.
    fn run(
        &self,
        exec_input: SandboxExecInput,
        mut wasm_memory: Memory,
        mut stable_memory: Memory,
        total_timer: std::time::Instant,
    ) {
        let run_timer = std::time::Instant::now();

        let system_state_accessor =
            SystemStateAccessorRPC::new(self.exec_id, self.sandbox_manager.controller.clone());

        let subnet_available_memory = exec_input
            .execution_parameters
            .subnet_available_memory
            .clone();
        let (
            WasmExecutionOutput {
                wasm_result,
                num_instructions_left,
                instance_stats,
            },
            deltas,
            // This field isn't used, but we want to ensure that it is not
            // dropped until the execution result is send back to the replica
            // because the drop can be expensive.
            _instance_or_system_api,
        ) = ic_embedders::wasm_executor::process(
            exec_input.func_ref,
            exec_input.api_type,
            exec_input.canister_current_memory_usage,
            exec_input.execution_parameters,
            exec_input.static_system_state,
            system_state_accessor,
            &self.canister_wasm.compilate,
            &self.canister_wasm.embedder,
            &mut wasm_memory,
            &mut stable_memory,
            &exec_input.globals,
            no_op_logger(),
        );

        match wasm_result {
            Ok(_) => {
                let state_modifications = deltas.map(
                    |(
                        DirtyPageIndices {
                            wasm_memory_delta,
                            stable_memory_delta,
                        },
                        globals,
                    )| {
                        StateModifications::new(
                            globals,
                            &wasm_memory,
                            &stable_memory,
                            &wasm_memory_delta,
                            &stable_memory_delta,
                            subnet_available_memory.get(),
                        )
                    },
                );
                if state_modifications.is_some() {
                    self.sandbox_manager
                        .add_memory(exec_input.next_wasm_memory_id, wasm_memory);
                    self.sandbox_manager
                        .add_memory(exec_input.next_stable_memory_id, stable_memory);
                }
                let wasm_output = WasmExecutionOutput {
                    wasm_result,
                    num_instructions_left,
                    instance_stats,
                };
                self.sandbox_manager.controller.execution_finished(
                    protocol::ctlsvc::ExecutionFinishedRequest {
                        exec_id: self.exec_id,
                        exec_output: SandboxExecOutput {
                            wasm: wasm_output,
                            state: state_modifications,
                            execute_total_duration: total_timer.elapsed(),
                            execute_run_duration: run_timer.elapsed(),
                        },
                    },
                );
            }
            Err(err) => {
                let wasm_output = WasmExecutionOutput {
                    wasm_result: Err(err),
                    num_instructions_left,
                    instance_stats,
                };

                self.sandbox_manager.controller.execution_finished(
                    protocol::ctlsvc::ExecutionFinishedRequest {
                        exec_id: self.exec_id,
                        exec_output: SandboxExecOutput {
                            wasm: wasm_output,
                            state: None,
                            execute_total_duration: total_timer.elapsed(),
                            execute_run_duration: run_timer.elapsed(),
                        },
                    },
                );
            }
        }
    }
}

/// Represents a wasm object of a canister. This is the executable code
/// of the canister.
struct CanisterWasm {
    embedder: Arc<WasmtimeEmbedder>,
    compilate: Arc<EmbedderCache>,
}

impl CanisterWasm {
    /// Validates and compiles the given Wasm binary.
    pub fn compile(wasm_src: Vec<u8>) -> HypervisorResult<Self> {
        let wasm = BinaryEncodedWasm::new(wasm_src);
        let log = ic_logger::replica_logger::no_op_logger();
        // TODO(EXC-755): Use the proper embedder config.
        let mut config = Config::new();
        config.persistence_type = PersistenceType::Sigsegv;

        // TODO(EXC-756): Cache WasmtimeEmbedder instance.
        let embedder = Arc::new(WasmtimeEmbedder::new(config.clone(), log));
        let instrumentation_output = validate_wasm_binary(&wasm, &config)
            .map_err(HypervisorError::from)
            .and_then(|_| {
                instrument(&wasm, &InstructionCostTable::new()).map_err(HypervisorError::from)
            })?;
        let compilate =
            embedder.compile(PersistenceType::Sigsegv, &instrumentation_output.binary)?;
        let compilate = Arc::new(compilate);

        Ok(Self {
            embedder,
            compilate,
        })
    }
}

/// Manages the entirety of the sandbox process. It provides the methods
/// through which the controller process (the replica) manages the
/// sandboxed execution.
pub struct SandboxManager {
    repr: Mutex<SandboxManagerInt>,
    controller: Arc<dyn ControllerService>,
}
struct SandboxManagerInt {
    canister_wasms: std::collections::HashMap<WasmId, Arc<CanisterWasm>>,
    memories: std::collections::HashMap<MemoryId, Arc<Memory>>,
    workers: threadpool::ThreadPool,
}

impl SandboxManager {
    /// Creates new sandbox manager. In order to operate, it needs
    /// an established backward RPC channel to the controller process
    /// to relay e.g. syscalls and completions.
    pub fn new(controller: Arc<dyn ControllerService>) -> Self {
        SandboxManager {
            repr: Mutex::new(SandboxManagerInt {
                canister_wasms: HashMap::new(),
                memories: HashMap::new(),
                workers: threadpool::ThreadPool::new(4),
            }),
            controller,
        }
    }

    /// Compiles the given Wasm binary and registers it under the given id.
    /// The function may fail if the Wasm binary is invalid.
    pub fn open_wasm(&self, wasm_id: WasmId, wasm_src: Vec<u8>) -> HypervisorResult<()> {
        let mut guard = self.repr.lock().unwrap();
        assert!(
            !guard.canister_wasms.contains_key(&wasm_id),
            "Failed to open wasm session {}: id is already in use",
            wasm_id,
        );
        let wasm = CanisterWasm::compile(wasm_src)?;
        guard.canister_wasms.insert(wasm_id, Arc::new(wasm));
        Ok(())
    }

    /// Closes previously opened wasm instance, by id.
    pub fn close_wasm(&self, wasm_id: WasmId) {
        let mut guard = self.repr.lock().unwrap();
        let removed = guard.canister_wasms.remove(&wasm_id);
        assert!(
            removed.is_some(),
            "Failed to close wasm session {}: id not found",
            wasm_id
        );
    }

    /// Opens a new memory requested by the replica process.
    pub fn open_memory(&self, request: OpenMemoryRequest) {
        let mut guard = self.repr.lock().unwrap();
        guard.open_memory(request);
    }

    /// Adds a new memory after sandboxed execution.
    fn add_memory(&self, memory_id: MemoryId, memory: Memory) {
        let mut guard = self.repr.lock().unwrap();
        guard.add_memory(memory_id, memory);
    }

    /// Closes previously opened memory instance, by id.
    pub fn close_memory(&self, memory_id: MemoryId) {
        let mut guard = self.repr.lock().unwrap();
        let removed = guard.memories.remove(&memory_id);
        assert!(
            removed.is_some(),
            "Failed to close state {}: id not found",
            memory_id
        );
        // Dropping memory may be expensive. Do it on a worker thread to avoid
        // blocking the main thread of the sandbox process.
        guard.workers.execute(move || drop(removed));
    }

    /// Starts Wasm execution using specific code and state, passing
    /// execution input.
    ///
    /// Note that inside here we start a transaction and the state of
    /// execution can not and does not change while we are processing
    /// this particular session.
    pub fn start_execution(
        sandbox_manager: &Arc<SandboxManager>,
        exec_id: ExecId,
        wasm_id: WasmId,
        wasm_memory_id: MemoryId,
        stable_memory_id: MemoryId,
        exec_input: SandboxExecInput,
    ) {
        let total_timer = std::time::Instant::now();
        let mut guard = sandbox_manager.repr.lock().unwrap();
        let wasm_runner = guard.canister_wasms.get(&wasm_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: wasm {} not found",
                exec_id, wasm_id
            )
        });
        let wasm_memory = guard.memories.get(&wasm_memory_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: wasm memory {} not found",
                exec_id, wasm_memory_id,
            )
        });
        let stable_memory = guard.memories.get(&stable_memory_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: stable memory {} not found",
                exec_id, stable_memory_id,
            )
        });
        Execution::start_on_worker_thread(
            exec_id,
            Arc::clone(wasm_runner),
            Arc::clone(wasm_memory),
            Arc::clone(stable_memory),
            Arc::clone(sandbox_manager),
            &mut guard.workers,
            exec_input,
            total_timer,
        );
    }

    pub fn create_execution_state(
        &self,
        wasm_id: WasmId,
        wasm_source: Vec<u8>,
        wasm_page_map: PageMapSerialization,
        canister_id: CanisterId,
    ) -> HypervisorResult<CreateExecutionStateSuccessReply> {
        // Get the compiled binary from the cache.
        let binary_encoded_wasm = BinaryEncodedWasm::new(wasm_source);
        let (embedder_cache, embedder) = {
            let guard = self.repr.lock().unwrap();
            let canister_wasm = guard.canister_wasms.get(&wasm_id).unwrap_or_else(|| {
                unreachable!(
                    "Failed to create execution state for {}: wasm {} not found",
                    canister_id, wasm_id
                )
            });
            (
                Arc::clone(&canister_wasm.compilate),
                Arc::clone(&canister_wasm.embedder),
            )
        };

        let mut wasm_page_map = PageMap::deserialize(wasm_page_map).unwrap();

        let (exported_functions, exported_globals, wasm_memory_delta, wasm_memory_size) =
            ic_embedders::wasm_executor::get_initial_globals_and_memory(
                &binary_encoded_wasm,
                &embedder_cache,
                &embedder,
                // TODO(EXC-755): Use the proper embedder config.
                &Config::default(),
                &mut wasm_page_map,
                canister_id,
            )?;

        // Send all necessary data for creating the execution state to replica.
        let wasm_memory = MemoryModifications {
            page_delta: wasm_page_map.serialize_delta(&wasm_memory_delta),
            size: wasm_memory_size,
        };

        Ok(CreateExecutionStateSuccessReply {
            wasm_memory,
            exported_globals,
            exported_functions,
        })
    }
}

impl SandboxManagerInt {
    fn open_memory(&mut self, request: OpenMemoryRequest) {
        let page_map = PageMap::deserialize(request.memory.page_map).unwrap();
        let memory = Memory::new(page_map, request.memory.num_wasm_pages);
        self.add_memory(request.memory_id, memory);
    }

    fn add_memory(&mut self, memory_id: MemoryId, memory: Memory) {
        assert!(
            !self.memories.contains_key(&memory_id),
            "Failed to open memory {}: id is already in use",
            memory_id
        );
        let memory = Arc::new(memory);
        self.memories.insert(memory_id, memory);
    }
}
