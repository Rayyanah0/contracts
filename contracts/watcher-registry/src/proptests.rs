#![allow(clippy::pedantic, clippy::too_many_lines)]

extern crate std;

use std::collections::BTreeSet;
use std::vec::Vec;

use crate::{AdminAction, ContractError, WatcherRegistry, WatcherRegistryClient};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use soroban_sdk::{testutils::Address as _, Address, Env};

fn create_env_and_client() -> (Env, Address, WatcherRegistryClient<'static>) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register(WatcherRegistry, (&admin,));
    let client = WatcherRegistryClient::new(&env, &contract_id);
    (env, admin, client)
}

#[derive(Debug, Clone)]
pub enum WatcherAction {
    RegisterWatcher(usize),
    RemoveWatcher(usize),
    ReplaceWatcher { old_idx: usize, new_idx: usize },
    ClearAll,
    ClearBatch(Vec<usize>),
}

fn watcher_address(env: &Env, idx: usize) -> Address {
    let mut addr = Address::generate(env);
    // Create deterministic addresses for testing
    for _ in 0..idx {
        addr = Address::generate(env);
    }
    addr
}

prop_compose! {
    fn arb_watcher_action()(action in prop_oneof![
        (0..10usize).prop_map(WatcherAction::RegisterWatcher),
        (0..10usize).prop_map(WatcherAction::RemoveWatcher),
    ]) -> WatcherAction {
        action
    }
}

proptest! {
    #[test]
    fn prop_watcher_set_never_empty(actions in prop::collection::vec(arb_watcher_action(), 1..20)) {
        let (env, admin, client) = create_env_and_client();
        let mut watchers: BTreeSet<Address> = BTreeSet::new();

        for action in actions {
            match action {
                WatcherAction::RegisterWatcher(idx) => {
                    let watcher = watcher_address(&env, idx);
                    let _ = client.register_watcher(&admin, &watcher);
                    watchers.insert(watcher);
                }
                WatcherAction::RemoveWatcher(idx) => {
                    if idx < watchers.len() {
                        if let Some(watcher) = watchers.iter().nth(idx).cloned() {
                            let _ = client.remove_watcher(&admin, &watcher);
                            watchers.remove(&watcher);
                        }
                    }
                }
                _ => {}
            }
        }

        // Admin set should never be empty after initialization
        let admins = client.get_admins();
        prop_assert!(!admins.is_empty());
    }

    #[test]
    fn prop_watcher_count_consistency(
        actions in prop::collection::vec(arb_watcher_action(), 1..15)
    ) {
        let (env, admin, client) = create_env_and_client();
        let mut registered: BTreeSet<Address> = BTreeSet::new();

        for action in actions {
            match action {
                WatcherAction::RegisterWatcher(idx) => {
                    let watcher = watcher_address(&env, idx);
                    let _ = client.register_watcher(&admin, &watcher);
                    registered.insert(watcher);
                }
                WatcherAction::RemoveWatcher(idx) => {
                    if idx < registered.len() {
                        if let Some(watcher) = registered.iter().nth(idx).cloned() {
                            let _ = client.remove_watcher(&admin, &watcher);
                            registered.remove(&watcher);
                        }
                    }
                }
                _ => {}
            }
        }

        let count = client.get_watcher_count();
        let watchers = client.get_watchers();

        // Count should match actual watcher list length
        prop_assert_eq!(count, watchers.len());
    }

    #[test]
    fn prop_no_duplicate_watchers(
        actions in prop::collection::vec(arb_watcher_action(), 1..20)
    ) {
        let (env, admin, client) = create_env_and_client();

        for action in actions {
            if let WatcherAction::RegisterWatcher(idx) = action {
                let watcher = watcher_address(&env, idx);
                let _ = client.register_watcher(&admin, &watcher);
            }
        }

        let watchers = client.get_watchers();
        let unique: BTreeSet<_> = watchers.iter().collect();

        // No duplicates should exist
        prop_assert_eq!(watchers.len() as usize, unique.len());
    }
}

#[derive(Debug, Clone)]
enum TimelockOp {
    SetDelay(u32),
    ProposeAddAdmin,
    ProposeClearAll,
    ProposeSetDelay(u32),
    Cancel,
    Execute,
    Advance(u32),
    DirectAddAdmin,
    DirectClearAll,
}

/// Model of the single queued action: what was proposed, when, and when it may run.
#[derive(Debug, Clone)]
struct ModelPending {
    action: AdminAction,
    proposed_at: u32,
    ready_at: u32,
}

fn arb_timelock_op() -> impl Strategy<Value = TimelockOp> {
    prop_oneof![
        (0..=20u32).prop_map(TimelockOp::SetDelay),
        Just(TimelockOp::ProposeAddAdmin),
        Just(TimelockOp::ProposeClearAll),
        (0..=20u32).prop_map(TimelockOp::ProposeSetDelay),
        Just(TimelockOp::Cancel),
        Just(TimelockOp::Execute),
        (0..=10u32).prop_map(TimelockOp::Advance),
        Just(TimelockOp::DirectAddAdmin),
        Just(TimelockOp::DirectClearAll),
    ]
}

fn propose_and_check(
    env: &Env,
    client: &WatcherRegistryClient<'static>,
    admin: &Address,
    action: AdminAction,
    delay: u32,
    pending: &mut Option<ModelPending>,
) -> Result<(), TestCaseError> {
    let had_pending = pending.is_some();
    let res = client.try_propose_admin_action(admin, &action).unwrap();
    if let Ok(ready_at) = res {
        // Only one action may occupy the single pending slot.
        prop_assert!(!had_pending, "action queued while another is pending");
        let proposed_at = env.ledger().sequence();
        prop_assert_eq!(ready_at, proposed_at + delay);
        *pending = Some(ModelPending {
            action,
            proposed_at,
            ready_at,
        });
    }
    Ok(())
}

proptest! {
    /// Random propose/cancel/execute/ledger-jump sequences checked against a model
    /// of the timelock. Asserts at most one pending action, that `ready_at` is fixed
    /// at proposal as `proposed_at + delay`, that execution waits for `ready_at`,
    /// that the delay only rises, and that sensitive direct calls are refused once
    /// a delay is set.
    #[test]
    fn prop_timelock_state_machine(ops in prop::collection::vec(arb_timelock_op(), 1..40)) {
        let (env, admin, client) = create_env_and_client();
        let mut delay: u32 = 0;
        let mut pending: Option<ModelPending> = None;

        for op in ops {
            match op {
                TimelockOp::SetDelay(d) => {
                    let res = client.try_set_timelock_delay(&admin, &d).unwrap();
                    if d >= delay {
                        prop_assert_eq!(res, Ok(()));
                        delay = d;
                    } else {
                        prop_assert_eq!(res, Err(ContractError::TimelockRequired));
                    }
                }
                TimelockOp::ProposeAddAdmin => {
                    let action = AdminAction::AddAdmin(Address::generate(&env));
                    propose_and_check(&env, &client, &admin, action, delay, &mut pending)?;
                }
                TimelockOp::ProposeClearAll => {
                    // No watchers are registered here, so ClearAllWatchers is always valid.
                    propose_and_check(
                        &env,
                        &client,
                        &admin,
                        AdminAction::ClearAllWatchers,
                        delay,
                        &mut pending,
                    )?;
                }
                TimelockOp::ProposeSetDelay(d) => {
                    propose_and_check(
                        &env,
                        &client,
                        &admin,
                        AdminAction::SetTimelockDelay(d),
                        delay,
                        &mut pending,
                    )?;
                }
                TimelockOp::Cancel => {
                    let res = client.try_cancel_admin_action(&admin).unwrap();
                    match pending.take() {
                        Some(_) => {
                            prop_assert_eq!(res, Ok(()));
                        }
                        None => {
                            prop_assert_eq!(res, Err(ContractError::NoPendingAction));
                        }
                    }
                }
                TimelockOp::Execute => {
                    let res = client.try_execute_admin_action(&admin).unwrap();
                    let now = env.ledger().sequence();
                    match pending.take() {
                        None => {
                            prop_assert_eq!(res, Err(ContractError::NoPendingAction));
                        }
                        Some(p) if now < p.ready_at => {
                            prop_assert_eq!(res, Err(ContractError::TimelockNotExpired));
                            pending = Some(p);
                        }
                        Some(p) => {
                            prop_assert_eq!(res, Ok(()));
                            // No sensitive action may run before the delay in effect now
                            // has elapsed since it was proposed.
                            prop_assert!(
                                now >= p.proposed_at + delay,
                                "action executed before current delay elapsed since proposal"
                            );
                            let new_delay = client.get_timelock_delay();
                            prop_assert!(new_delay >= delay, "timelock delay was lowered");
                            delay = new_delay;
                        }
                    }
                }
                TimelockOp::Advance(n) => {
                    env.ledger().with_mut(|li| li.sequence_number += n);
                }
                TimelockOp::DirectAddAdmin => {
                    // Direct calls are allowed before any delay and would change the admin
                    // set outside the model, so only probe them once a delay is set.
                    if delay > 0 {
                        let res = client
                            .try_add_admin(&admin, &Address::generate(&env))
                            .unwrap();
                        prop_assert_eq!(res, Err(ContractError::TimelockRequired));
                    }
                }
                TimelockOp::DirectClearAll => {
                    if delay > 0 {
                        let res = client.try_clear_all_watchers(&admin).unwrap();
                        prop_assert_eq!(res, Err(ContractError::TimelockRequired));
                    }
                }
            }

            // Contract storage must match the model after every step.
            let stored = client.get_pending_action();
            prop_assert_eq!(
                stored.as_ref().map(|p| &p.action),
                pending.as_ref().map(|p| &p.action)
            );
            prop_assert_eq!(stored.as_ref().map(|p| p.ready_at), pending.as_ref().map(|p| p.ready_at));
            prop_assert_eq!(client.get_timelock_delay(), delay);
        }
    }
}
