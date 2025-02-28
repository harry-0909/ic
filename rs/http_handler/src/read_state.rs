//! Module that deals with requests to /api/v2/canister/.../read_state

use crate::{
    common::{cbor_response, into_cbor, make_response, make_response_on_validation_error},
    types::{ApiReqType, RequestType},
    HttpHandlerMetrics, ReplicaHealthStatus, UNKNOWN_LABEL,
};
use hyper::{Body, Response};
use ic_crypto_tree_hash::{sparse_labeled_tree_from_paths, Label, Path};
use ic_interfaces::{
    crypto::IngressSigVerifier, registry::RegistryClient, state_manager::StateReader,
};
use ic_logger::{trace, ReplicaLogger};
use ic_replicated_state::{canister_state::execution_state::CustomSectionType, ReplicatedState};
use ic_types::{
    canonical_error::{
        invalid_argument_error, not_found_error, permission_denied_error, resource_exhausted_error,
        unavailable_error, CanonicalError,
    },
    malicious_flags::MaliciousFlags,
    messages::{
        Blob, Certificate, CertificateDelegation, HttpReadStateContent, HttpReadStateResponse,
        HttpRequest, HttpRequestEnvelope, MessageId, ReadState, SignedRequestBytes,
        EXPECTED_MESSAGE_ID_LENGTH,
    },
    time::current_time,
    CanisterId, UserId,
};
use ic_validator::{get_authorized_canisters, CanisterIdSet};
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tower::{BoxError, Service};

const MAX_READ_STATE_REQUEST_IDS: u8 = 100;

#[derive(Clone)]
pub(crate) struct ReadStateService {
    log: ReplicaLogger,
    metrics: HttpHandlerMetrics,
    health_status: Arc<RwLock<ReplicaHealthStatus>>,
    delegation_from_nns: Arc<RwLock<Option<CertificateDelegation>>>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    validator: Arc<dyn IngressSigVerifier + Send + Sync>,
    registry_client: Arc<dyn RegistryClient>,
    malicious_flags: MaliciousFlags,
}

impl ReadStateService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        log: ReplicaLogger,
        metrics: HttpHandlerMetrics,
        health_status: Arc<RwLock<ReplicaHealthStatus>>,
        delegation_from_nns: Arc<RwLock<Option<CertificateDelegation>>>,
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        validator: Arc<dyn IngressSigVerifier + Send + Sync>,
        registry_client: Arc<dyn RegistryClient>,
        malicious_flags: MaliciousFlags,
    ) -> Self {
        Self {
            log,
            metrics,
            health_status,
            delegation_from_nns,
            state_reader,
            validator,
            registry_client,
            malicious_flags,
        }
    }
}

impl Service<Vec<u8>> for ReadStateService {
    type Response = Response<Body>;
    type Error = BoxError;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, body: Vec<u8>) -> Self::Future {
        trace!(self.log, "in handle read_state");
        self.metrics
            .requests_body_size_bytes
            .with_label_values(&[
                RequestType::ReadState.as_str(),
                ApiReqType::ReadState.as_str(),
                UNKNOWN_LABEL,
            ])
            .observe(body.len() as f64);
        if *self.health_status.read().unwrap() != ReplicaHealthStatus::Healthy {
            let res = make_response(unavailable_error(
                "Replica is starting. Check the /api/v2/status for more information.",
            ));
            return Box::pin(async move { Ok(res) });
        }
        let delegation_from_nns = self.delegation_from_nns.read().unwrap().clone();

        let request = match <HttpRequestEnvelope<HttpReadStateContent>>::try_from(
            &SignedRequestBytes::from(body),
        ) {
            Ok(request) => request,
            Err(e) => {
                let res = make_response(invalid_argument_error(
                    format!("Could not parse body as read request: {}", e).as_str(),
                ));
                return Box::pin(async move { Ok(res) });
            }
        };

        // Convert the message to a strongly-typed struct, making structural validations
        // on the way.
        let request = match HttpRequest::<ReadState>::try_from(request) {
            Ok(request) => request,
            Err(e) => {
                let res = make_response(invalid_argument_error(
                    format!("Malformed request: {:?}", e).as_str(),
                ));
                return Box::pin(async move { Ok(res) });
            }
        };
        let read_state = request.content();

        match get_authorized_canisters(
            &request,
            self.validator.as_ref(),
            current_time(),
            self.registry_client.get_latest_version(),
            &self.malicious_flags,
        ) {
            Ok(targets) => {
                if let Err(err) = verify_paths(
                    self.state_reader.as_ref(),
                    &read_state.source,
                    &read_state.paths,
                    &targets,
                ) {
                    return Box::pin(async move { Ok(make_response(err)) });
                }
            }
            Err(err) => {
                let res = make_response_on_validation_error(request.id(), err, &self.log);
                return Box::pin(async move { Ok(res) });
            }
        }

        // Verify that the sender has authorization to the paths requested.

        let mut paths: Vec<Path> = read_state.paths.clone();

        // Always add "time" to the paths even if not explicitly requested.
        paths.push(Path::from(Label::from("time")));

        let labeled_tree = sparse_labeled_tree_from_paths(&mut paths);

        let res = match self.state_reader.read_certified_state(&labeled_tree) {
            Some((_state, tree, certification)) => {
                let signature = certification.signed.signature.signature.get().0;
                let res = HttpReadStateResponse {
                    certificate: Blob(into_cbor(&Certificate {
                        tree,
                        signature: Blob(signature),
                        delegation: delegation_from_nns,
                    })),
                };
                cbor_response(&res)
            }
            None => make_response(unavailable_error(
                "Certified state is not available yet. Please try again...",
            )),
        };
        Box::pin(async move { Ok(res) })
    }
}

// Verifies that the `user` is authorized to retrieve the `paths` requested.
fn verify_paths(
    state_reader: &dyn StateReader<State = ReplicatedState>,
    user: &UserId,
    paths: &[Path],
    targets: &CanisterIdSet,
) -> Result<(), CanonicalError> {
    let state = state_reader.get_latest_state().take();
    let mut num_request_ids = 0;

    // Convert the paths to slices to make it easier to match below.
    let paths: Vec<Vec<&[u8]>> = paths
        .iter()
        .map(|path| path.iter().map(|label| label.as_bytes()).collect())
        .collect();

    for path in paths {
        match path.as_slice() {
            [b"time"] => {}
            [b"canister", _canister_id, b"controller"] => {}
            [b"canister", _canister_id, b"controllers"] => {}
            [b"canister", _canister_id, b"module_hash"] => {}
            [b"canister", canister_id, b"metadata", name] => {
                let name = String::from_utf8(Vec::from(*name)).map_err(|err| {
                    invalid_argument_error(&format!(
                        "Could not parse the custom section name: {}.",
                        err
                    ))
                })?;

                match CanisterId::try_from(*canister_id) {
                    Ok(canister_id) => {
                        can_read_canister_metadata(user, &canister_id, &name, &state)?
                    }
                    Err(err) => {
                        return Err(invalid_argument_error(&format!(
                            "Could not parse Canister ID: {}.",
                            err
                        )))
                    }
                }
            }
            [b"subnet", _subnet_id, b"public_key"] => {}
            [b"subnet", _subnet_id, b"canister_ranges"] => {}
            [b"request_status", request_id] | [b"request_status", request_id, ..] => {
                num_request_ids += 1;

                if num_request_ids > MAX_READ_STATE_REQUEST_IDS {
                    return Err(resource_exhausted_error(&format!(
                        "Can only request up to {} request IDs.",
                        MAX_READ_STATE_REQUEST_IDS
                    )));
                }

                // Verify that the request was signed by the same user.
                if let Ok(message_id) = MessageId::try_from(*request_id) {
                    let ingress_status = state.get_ingress_status(&message_id);

                    if let Some(ingress_user_id) = ingress_status.user_id() {
                        if let Some(receiver) = ingress_status.receiver() {
                            if ingress_user_id != *user || !targets.contains(&receiver) {
                                return Err(permission_denied_error(
                                    "Request IDs must be for requests signed by the caller.",
                                ));
                            }
                        }
                    }
                } else {
                    return Err(invalid_argument_error(&format!(
                        "Request IDs must be {} bytes in length.",
                        EXPECTED_MESSAGE_ID_LENGTH
                    )));
                }
            }
            _ => {
                // All other paths are unsupported.
                return Err(not_found_error("Invalid path requested."));
            }
        }
    }

    Ok(())
}

fn can_read_canister_metadata(
    user: &UserId,
    canister_id: &CanisterId,
    custom_section_name: &str,
    state: &ReplicatedState,
) -> Result<(), CanonicalError> {
    let canister = state
        .canister_states
        .get(canister_id)
        .ok_or_else(|| not_found_error("Invalid path requested."))?;

    match &canister.execution_state {
        Some(execution_state) => {
            let custom_section = execution_state
                .metadata
                .get_custom_section(custom_section_name)
                .ok_or_else(|| not_found_error("Invalid path requested."))?;

            // Only the controller can request this custom section.
            if custom_section.visibility == CustomSectionType::Private
                && !canister.system_state.controllers.contains(&user.get())
            {
                return Err(permission_denied_error(&format!(
                    "Custom section {} can only be requested by the controllers of the canister.",
                    custom_section_name
                )));
            }
        }
        None => return Err(not_found_error("Invalid path requested.")),
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use crate::common::test::{array, assert_cbor_ser_equal, bytes, int};
    use crate::read_state::can_read_canister_metadata;
    use ic_crypto_tree_hash::{Digest, Label, MixedHashTree};
    use ic_registry_subnet_type::SubnetType;
    use ic_replicated_state::ReplicatedState;
    use ic_test_utilities::state::insert_dummy_canister;
    use ic_test_utilities::types::ids::{canister_test_id, subnet_test_id, user_test_id};
    use ic_types::canonical_error::{not_found_error, permission_denied_error};

    #[test]
    fn encoding_read_state_tree_empty() {
        let tree = MixedHashTree::Empty;
        assert_cbor_ser_equal(&tree, array(vec![int(0)]));
    }

    #[test]
    fn encoding_read_state_tree_leaf() {
        let tree = MixedHashTree::Leaf(vec![1, 2, 3]);
        assert_cbor_ser_equal(&tree, array(vec![int(3), bytes(&[1, 2, 3])]));
    }

    #[test]
    fn encoding_read_state_tree_pruned() {
        let tree = MixedHashTree::Pruned(Digest([1; 32]));
        assert_cbor_ser_equal(&tree, array(vec![int(4), bytes(&[1; 32])]));
    }

    #[test]
    fn encoding_read_state_tree_fork() {
        let tree = MixedHashTree::Fork(Box::new((
            MixedHashTree::Leaf(vec![1, 2, 3]),
            MixedHashTree::Leaf(vec![4, 5, 6]),
        )));
        assert_cbor_ser_equal(
            &tree,
            array(vec![
                int(1),
                array(vec![int(3), bytes(&[1, 2, 3])]),
                array(vec![int(3), bytes(&[4, 5, 6])]),
            ]),
        );
    }

    #[test]
    fn encoding_read_state_tree_mixed() {
        let tree = MixedHashTree::Fork(Box::new((
            MixedHashTree::Labeled(
                Label::from(vec![1, 2, 3]),
                Box::new(MixedHashTree::Pruned(Digest([2; 32]))),
            ),
            MixedHashTree::Leaf(vec![4, 5, 6]),
        )));
        assert_cbor_ser_equal(
            &tree,
            array(vec![
                int(1),
                array(vec![
                    int(2),
                    bytes(&[1, 2, 3]),
                    array(vec![int(4), bytes(&[2; 32])]),
                ]),
                array(vec![int(3), bytes(&[4, 5, 6])]),
            ]),
        );
    }

    #[test]
    fn user_can_read_canister_metadata() {
        let canister_id = canister_test_id(100);
        let controller = user_test_id(24);
        let non_controller = user_test_id(20);

        let mut state = ReplicatedState::new_rooted_at(
            subnet_test_id(1),
            SubnetType::Application,
            "Initial".into(),
        );
        insert_dummy_canister(&mut state, canister_id, controller.get());

        let public_name = "dummy";
        // Controller can read the public custom section
        assert!(can_read_canister_metadata(&controller, &canister_id, public_name, &state).is_ok());

        // Non-controller can read public custom section
        assert!(
            can_read_canister_metadata(&non_controller, &canister_id, public_name, &state).is_ok()
        );

        let private_name = "candid";
        // Controller can read private custom section
        assert!(
            can_read_canister_metadata(&controller, &canister_id, private_name, &state).is_ok()
        );
    }

    #[test]
    fn user_cannot_read_canister_metadata() {
        let canister_id = canister_test_id(100);
        let controller = user_test_id(24);
        let non_controller = user_test_id(20);

        let mut state = ReplicatedState::new_rooted_at(
            subnet_test_id(1),
            SubnetType::Application,
            "Initial".into(),
        );
        insert_dummy_canister(&mut state, canister_id, controller.get());

        // Non-controller cannot read private custom section named `candid`.
        assert_eq!(
            can_read_canister_metadata(&non_controller, &canister_id, "candid", &state),
            Err(permission_denied_error(
                "Custom section candid can only be requested by the controllers of the canister."
            ))
        );

        // Non existent public custom section.
        assert_eq!(
            can_read_canister_metadata(&non_controller, &canister_id, "unknown-name", &state),
            Err(not_found_error("Invalid path requested."))
        );
    }
}
