use fondue;
use ic_fondue::{
    ic_manager::IcHandle,
    internet_computer::{InternetComputer, Subnet},
};

use crate::nns::NnsExt;
use crate::util;

use canister_test::Canister;
use ic_fondue::ic_manager::IcControl;
use ic_nns_constants::{LEDGER_CANISTER_ID, LIFELINE_CANISTER_ID};
use ic_registry_subnet_type::SubnetType;
use ic_types::CanisterId;
use ledger_canister::TRANSACTION_FEE;
use std::convert::TryFrom;

pub fn config() -> InternetComputer {
    InternetComputer::new()
        .add_subnet(Subnet::fast_single_node(SubnetType::System).add_nodes(3))
        .add_subnet(Subnet::fast_single_node(SubnetType::Application))
}

pub fn test(handle: IcHandle, ctx: &fondue::pot::Context) {
    // Install NNS canisters
    ctx.install_nns_canisters(&handle, true);

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    let mut rng = ctx.rng.clone();

    let nns_endpoints: Vec<_> = handle
        .as_permutation(&mut rng)
        .filter(|e| e.subnet.as_ref().map(|s| s.type_of) == Some(SubnetType::System))
        .collect();
    assert_eq!(nns_endpoints.len(), 4);
    rt.block_on(util::assert_all_ready(nns_endpoints.as_slice(), ctx));

    let nns_endpoint = nns_endpoints.first().expect("no NNS nodes");
    let nns = util::runtime_from_url(nns_endpoint.url.clone());
    let ledger = Canister::new(&nns, LEDGER_CANISTER_ID);
    let nns_agent = rt.block_on(util::assert_create_agent(nns_endpoint.url.as_str()));
    let lifeline = util::block_on(util::UniversalCanister::upgrade(
        &nns,
        &nns_agent,
        &LIFELINE_CANISTER_ID,
    ));

    let app_endpoint = util::get_random_application_node_endpoint(&handle, &mut rng);
    rt.block_on(app_endpoint.assert_ready(ctx));
    let agent = rt.block_on(util::assert_create_agent(app_endpoint.url.as_str()));
    let can1 = util::block_on(util::UniversalCanister::new(&agent));
    let can2 = util::block_on(util::UniversalCanister::new(&agent));

    // Top up canisters with amounts of ICP needed for subsequent operations to
    // succeed.
    let fee = TRANSACTION_FEE.get_e8s();
    transfer(
        ctx,
        &rt,
        &ledger.clone(),
        &lifeline,
        &can1.clone(),
        1000 + 2 * fee,
    );
    transfer(
        ctx,
        &rt,
        &ledger.clone(),
        &lifeline,
        &can2.clone(),
        1000 + 2 * fee,
    );

    // Kill one NNS node. Three out of four nodes are still operational which is
    // enough for the subnet to make progress and thus complete the transfer
    // successfully.
    nns_endpoints[1].kill_node(ctx.logger.clone());
    transfer(ctx, &rt, &ledger.clone(), &can1.clone(), &can2.clone(), 100);

    // Kill another NNS node. With two malfunctioned nodes, the network is stuck,
    // i.e. all update requests will be rejected.
    nns_endpoints[2].kill_node(ctx.logger.clone());
    // Start over the node killed first.
    let _ = nns_endpoints[1].start_node(ctx.logger.clone());

    // A transfer request can be started right away, even though the rejoined node
    // is likely not yet ready. Its completion will be delayed until the
    // rejoined node is up.
    transfer(ctx, &rt, &ledger.clone(), &can2.clone(), &can1.clone(), 100);
}

/// Transfers `amount` of ICP between two given canisters and verifies that the
/// balance of the target canister is `amount` more than before the request.
/// Crashes if the update request doesn't succeed or the balance is not as
/// expected afterwards.
fn transfer(
    ctx: &fondue::pot::Context,
    rt: &tokio::runtime::Runtime,
    ledger: &Canister,
    from: &util::UniversalCanister,
    to: &util::UniversalCanister,
    amount: u64,
) {
    rt.block_on(async move {
        let balance = util::get_icp_balance(
            ledger,
            &CanisterId::try_from(to.canister_id().as_slice()).unwrap(),
            None,
        )
        .await;
        util::transact_icp(ctx, ledger, from, amount, to, balance.get_e8s() + amount).await;
    });
}
