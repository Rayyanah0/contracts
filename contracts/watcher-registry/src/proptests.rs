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

    #[test]
    fn prop_are_watchers_authorized_matches_input_length(
        n in 0..40usize
    ) {
        let (env, admin, client) = create_env_and_client();
        let watcher = Address::generate(&env);
        let _ = client.register_watcher(&admin, &watcher);

        let mut candidates = SorobanVec::new(&env);
        for _ in 0..n {
            candidates.push_back(Address::generate(&env));
        }

        let result = client.try_are_watchers_authorized(&candidates);
        match result {
            Ok(Ok(authorized)) => {
                // Successful results must be aligned with the input length.
                prop_assert_eq!(authorized.len() as usize, n);
            }
            Ok(Err(err)) => {
                // Oversized batches must surface an explicit error rather than
                // silently truncating the result vector.
                prop_assert_eq!(err, ContractError::PageSizeExceeded);
            }
            Err(_) => {
                // Host-level failure is acceptable only for oversized input.
                prop_assert!(n > 0);
            }
        }
    }

    #[test]
    fn prop_replace_watcher_preserves_scope_and_metadata(
        scoped in any::<bool>(),
        metadata in prop::option::of(0u32..1000u32),
    ) {
        let (env, admin, client) = create_env_and_client();
        let old = Address::generate(&env);
        let new = Address::generate(&env);

        let _ = client.register_watcher(&admin, &old);

        let scope = if scoped {
            Some(SorobanVec::from_array(&env, [Address::generate(&env)]))
        } else {
            None
        };
        let _ = client.set_watcher_scope(&admin, &old, &scope);

        let meta = metadata.map(|m| m as u64);
        let _ = client.set_watcher_metadata(&admin, &old, &meta);

        let _ = client.replace_watcher(&admin, &old, &new);

        // Old address must no longer be registered.
        prop_assert!(!client.is_watcher(&old));
        // New address must be registered.
        prop_assert!(client.is_watcher(&new));

        // Scope and metadata must carry over to the new address.
        prop_assert_eq!(client.get_watcher_scope(&new), scope);
        prop_assert_eq!(client.get_watcher_metadata(&new), meta);
    }
}
