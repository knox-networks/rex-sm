use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{manager::HashKind, node::Node, StateId};

/// [`NodeStore`] is the storage layer for all state machines associated with a given manager.
/// Every state tree is associated with a particular Mutex
/// this allows separate state hirearchies to be acted upon concurrently
/// while making operations in a particular tree blocking
pub struct NodeStore<Id, S> {
    pub trees: DashMap<Id, Arc<Mutex<Node<Id, S>>>>,
}

impl<S, K> Default for NodeStore<StateId<K>, S>
where
    K: HashKind,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Entries are distinguished from Nodes
/// by the [`Arc<Mutex<_>>`] that contains a given node.
/// Every child node in a particular tree should have
/// should be represented by `N` additional [`StoreEntry`]s
/// in a given [`NodeStore`] indexed by that particular node's `id` field.
type Entry<K, S> = Arc<Mutex<Node<StateId<K>, S>>>;
pub(crate) type OwnedEntry<K, S> = OwnedMutexGuard<Node<StateId<K>, S>>;

impl<S, K> NodeStore<StateId<K>, S>
where
    K: HashKind,
{
    pub fn new() -> Self {
        Self {
            trees: DashMap::new(),
        }
    }

    pub fn new_entry(node: Node<StateId<K>, S>) -> Entry<K, S> {
        Arc::new(Mutex::new(node))
    }

    // insert entry creates a new reference to the same node
    pub fn insert_entry(&self, id: StateId<K>, entry: Entry<K, S>) {
        self.trees.insert(id, entry);
    }

    // decrements the reference count on a given `Node`
    pub fn remove_entry(&self, id: StateId<K>) {
        self.trees.remove(&id);
    }

    pub fn get(&self, id: StateId<K>) -> Option<Entry<K, S>> {
        let node = self.trees.get(&id);
        node.map(|n| n.value().clone())
    }
}
