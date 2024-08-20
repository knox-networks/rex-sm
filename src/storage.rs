use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::FairMutex;

use crate::{node::Node, Kind, Rex, StateId};

/// [`StateStore`] is the storage layer for all state machines associated with a given manager.
/// Every state tree is associated with a particular Mutex
/// this allows separate state hirearchies to be acted upon concurrently
/// while making operations in a particular tree blocking
pub struct StateStore<Id, S> {
    pub trees: DashMap<Id, Arc<FairMutex<Node<Id, S>>>>,
}

impl<K> Default for StateStore<StateId<K>, K::State>
where
    K: Rex,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Entries are distinguished from Nodes
/// by the [`Arc<FairMutex<_>>`] that contains a given node.
/// Every child node in a particular tree should have
/// should be represented by `N` additional [`StoreTree`]s
/// in a given [`StateStore`] indexed by that particular node's `id` field.
pub(crate) type Tree<K> = Arc<FairMutex<Node<StateId<K>, <K as Kind>::State>>>;

impl<K: Rex> StateStore<StateId<K>, K::State> {
    #[must_use] pub fn new() -> Self {
        Self {
            trees: DashMap::new(),
        }
    }

    pub fn new_tree(node: Node<StateId<K>, K::State>) -> Tree<K> {
        Arc::new(FairMutex::new(node))
    }

    // insert node creates a new reference to the same node
    pub fn insert_ref(&self, id: StateId<K>, node: Tree<K>) {
        self.trees.insert(id, node);
    }

    // decrements the reference count on a given `Node`
    pub fn remove_ref(&self, id: StateId<K>) {
        self.trees.remove(&id);
    }

    pub fn get_tree(&self, id: StateId<K>) -> Option<Tree<K>> {
        let node = self.trees.get(&id);
        node.map(|n| n.value().clone())
    }
}
