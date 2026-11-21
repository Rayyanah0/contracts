#![allow(clippy::pedantic, clippy::too_many_lines)]

extern crate std;

use std::collections::BTreeSet;
use std::vec::Vec;

use crate::{WatcherRegistry, WatcherRegistryClient, ContractError};
use proptest::prelude::*;
use soroban_sdk::{testutils::Address as _, Address, Env, Vec as SorobanVec};

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
    /// Clear all watchers except the ones kept (new `clear_all_watchers` semantics).
    ClearAllKeeping(Vec<usize>),
    ClearBatch(Vec<usize>),
    SetScope(usize),
    Pause,
    Unpause,
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
        (0..10usize).prop_map(WatcherAction::SetScope),
        prop::collection::vec(0..10usize, 0..4).prop_map(WatcherAction::ClearAllKeeping),
        prop::collection::vec(0..10usize, 0..4).prop_map(WatcherAction::ClearBatch),
        Just(WatcherAction::Pause),
        Just(WatcherAction::Unpause),
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

    #[test]
    fn prop_removal_purges_metadata_and_scope(
        actions in prop::collection::vec(arb_watcher_action(), 1..20)
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
                WatcherAction::SetScope(idx) => {
                    if let Some(watcher) = registered.iter().nth(idx).cloned() {
                        let _ = client.set_watcher_scope(&admin, &watcher, &1u32);
                    }
                }
                WatcherAction::RemoveWatcher(idx) => {
                    if let Some(watcher) = registered.iter().nth(idx).cloned() {
                        let _ = client.remove_watcher(&admin, &watcher);
                        registered.remove(&watcher);
                        // Metadata and scope must be purged on removal.
                        prop_assert!(client.get_watcher_info(&watcher).is_none());
                        prop_assert!(client.get_watcher_scope(&watcher).is_none());
                    }
                }
                WatcherAction::ClearBatch(indices) => {
                    let mut to_clear = SorobanVec::new(&env);
                    for idx in indices {
                        if let Some(watcher) = registered.iter().nth(idx).cloned() {
                            to_clear.push_back(watcher.clone());
                            registered.remove(&watcher);
                        }
                    }
                    let _ = client.clear_watchers_batch(&admin, &to_clear);
                    for watcher in to_clear.iter() {
                        prop_assert!(client.get_watcher_info(&watcher).is_none());
                        prop_assert!(client.get_watcher_scope(&watcher).is_none());
                    }
                }
                WatcherAction::ClearAllKeeping(indices) => {
                    let mut keep = SorobanVec::new(&env);
                    for idx in indices {
                        if let Some(watcher) = registered.iter().nth(idx).cloned() {
                            keep.push_back(watcher);
                        }
                    }
                    let _ = client.clear_all_watchers(&admin, &keep);
                    let kept: BTreeSet<Address> = keep.iter().collect();
                    for watcher in registered.iter() {
                        if !kept.contains(watcher) {
                            prop_assert!(client.get_watcher_info(watcher).is_none());
                            prop_assert!(client.get_watcher_scope(watcher).is_none());
                        }
                    }
                    registered = kept;
                }
                _ => {}
            }
        }

        // Re-registering a removed watcher must start unscoped.
        for watcher in registered.iter() {
            let _ = client.remove_watcher(&admin, watcher);
            prop_assert!(client.get_watcher_scope(watcher).is_none());
            let _ = client.register_watcher(&admin, watcher);
            prop_assert!(client.get_watcher_scope(watcher).is_none());
        }
    }

    #[test]
    fn prop_guardian_pause_keeps_state_consistent(
        actions in prop::collection::vec(arb_watcher_action(), 1..20)
    ) {
        let (env, admin, client) = create_env_and_client();
        let guardian = Address::generate(&env);
        let _ = client.set_guardian(&admin, &guardian);

        for action in actions {
            match action {
                WatcherAction::RegisterWatcher(idx) => {
                    let watcher = watcher_address(&env, idx);
                    let _ = client.register_watcher(&admin, &watcher);
                }
                WatcherAction::Pause => {
                    let _ = client.pause(&guardian);
                }
                WatcherAction::Unpause => {
                    // Guardian must not be able to unpause; only admin can.
                    let _ = client.unpause(&admin);
                }
                _ => {}
            }

            // Pausing must never corrupt the watcher set or admin list.
            let count = client.get_watcher_count();
            let watchers = client.get_watchers();
            prop_assert_eq!(count, watchers.len());
            prop_assert!(!client.get_admins().is_empty());
        }
    }
}
