use std::{collections::HashSet, fmt, hash::Hash};

use crate::{Kind, State};

impl<Id, K> Kind for Id
where
    K: Kind,
    Id: std::ops::Deref<Target = K> + Send + fmt::Debug,
{
    type State = K::State;
    fn new_state(&self) -> Self::State {
        self.deref().new_state()
    }
    fn failed_state(&self) -> Self::State {
        self.deref().failed_state()
    }

    fn completed_state(&self) -> Self::State {
        self.deref().completed_state()
    }
}

pub struct Insert<Id> {
    pub parent_id: Option<Id>,
    pub id: Id,
}

pub struct Update<Id, S> {
    pub id: Id,
    pub state: S,
}

#[derive(Debug)]
pub struct Node<Id, S> {
    pub id: Id,
    pub state: S,
    descendant_keys: HashSet<Id>, // https://en.wikipedia.org/wiki/Brzozowski_derivative
    pub children: Vec<Node<Id, S>>,
}

impl<Id, S> Node<Id, S>
where
    S: State,
    Id: Copy + Eq + PartialEq + Hash + fmt::Display + Kind<State = S> + fmt::Debug,
{
    pub fn new(id: Id) -> Self {
        Self {
            id,
            state: id.new_state(),
            descendant_keys: HashSet::new(),
            children: Vec::new(),
        }
    }

    pub fn zipper(self) -> Zipper<Id, S> {
        Zipper {
            node: self,
            parent: None,
            self_idx: 0,
        }
    }

    pub fn get(&self, id: Id) -> Option<&Node<Id, S>> {
        if self.id == id {
            return Some(self);
        }
        if !self.descendant_keys.contains(&id) {
            return None;
        }

        let mut node = self;
        while node.descendant_keys.contains(&id) {
            node = node.child(id).unwrap();
        }
        Some(node)
    }

    pub fn get_state(&self, id: Id) -> Option<&S> {
        self.get(id).map(|n| &n.state)
    }

    pub fn child(&self, id: Id) -> Option<&Node<Id, S>> {
        self.children
            .iter()
            .find(|node| node.id == id || node.descendant_keys.contains(&id))
    }

    // get array index by of node with Id in self.descendant_keys
    pub fn child_idx(&self, id: Id) -> Option<usize> {
        self.children
            .iter()
            .enumerate()
            .find(|(_idx, node)| node.id == id || node.descendant_keys.contains(&id))
            .map(|(idx, _)| idx)
    }

    pub fn insert(&mut self, insert: Insert<Id>) {
        // temporary allocation to allow a drop in &mut implementation
        //
        // this can be optimized later but right now allocation impact
        // is non existent since Node::new
        // does not grow its `?Sized` types
        let mut swap_node = Node::new(self.id);
        std::mem::swap(&mut swap_node, self);

        swap_node = swap_node.into_insert(insert);

        std::mem::swap(&mut swap_node, self);
    }

    /// inserts a new node using self by value
    pub fn into_insert(self, Insert { parent_id, id }: Insert<Id>) -> Node<Id, S> {
        // inserts at this point should be guaranteed Some(id)
        // ince a parent_id.is_none() should be handled by the node
        // store through a new graph creation
        let parent_id = parent_id.unwrap();

        self.zipper()
            .by_id(parent_id)
            .insert_child(id)
            .finish_insert(id)
    }

    pub fn get_parent_id(&self, id: Id) -> Option<Id> {
        // root_node edge case
        if !self.descendant_keys.contains(&id) {
            return None;
        }

        let mut node = self;
        while node.descendant_keys.contains(&id) {
            let child_node = node.child(id).unwrap();
            if child_node.id == id {
                return Some(node.id);
            }
            node = child_node;
        }

        None
    }

    pub fn update(&mut self, update: Update<Id, S>) {
        // see Node::insert
        let mut swap_node = Node::new(self.id);
        std::mem::swap(&mut swap_node, self);

        swap_node = swap_node.into_update(update);

        std::mem::swap(&mut swap_node, self);
    }

    /// update a given node's state and return the parent ID if it exists
    pub fn update_and_get_parent_id(&mut self, Update { id, state }: Update<Id, S>) -> Option<Id> {
        // see Node::insert
        let mut swap_node = Node::new(self.id);
        std::mem::swap(&mut swap_node, self);

        let (parent_id, mut swap_node) = swap_node
            .zipper()
            .by_id(id)
            .set_state(state)
            .finish_update_parent_id();

        std::mem::swap(&mut swap_node, self);

        parent_id
    }

    // apply a closure to all nodes in a tree
    pub fn update_all_fn<F>(&mut self, f: F)
    where
        F: Fn(Zipper<Id, S>) -> Node<Id, S> + Clone,
    {
        // see Node::insert
        let mut swap_node = Node::new(self.id);
        std::mem::swap(&mut swap_node, self);

        swap_node = swap_node.zipper().finish_update_fn(f);

        std::mem::swap(&mut swap_node, self);
    }

    pub fn into_update(self, Update { id, state }: Update<Id, S>) -> Node<Id, S> {
        self.zipper().by_id(id).set_state(state).finish_update()
    }
}

/// Example of a [`Zipper`] cursor traversing a [`Vec`],
/// the *focus* provides a view "Up" and "Down" the data:
/// ```text
/// [1, 2, 3, 4, 5] // array with 5 entries
///  1}[2, 3, 4, 5] // zipper starts with focues at first index
/// [1] 2}[3, 4, 5] // moving down the array
/// [2, 1] 3}[4, 5]
/// [3, 2, 1] 4}[5]
/// [4, 3, 2, 1]{5  // zipper travels back up the array
/// ```
/// See `node/README.md` for further details.
pub struct Zipper<Id, S> {
    pub node: Node<Id, S>,
    pub parent: Option<Box<Zipper<Id, S>>>,
    self_idx: usize,
}

impl<Id, S> Zipper<Id, S>
where
    S: State,
    Id: Copy + Eq + PartialEq + Hash + fmt::Display + Kind<State = S> + fmt::Debug,
{
    fn by_id(mut self, id: Id) -> Zipper<Id, S> {
        let mut contains_id = self.node.descendant_keys.contains(&id);
        while contains_id {
            let idx = self.node.child_idx(id).unwrap();
            self = self.child(idx);
            contains_id = self.node.descendant_keys.contains(&id);
        }
        if self.node.id != id {
            panic!("id[{id}] should be in the node, this is a bug");
        }
        self
    }

    fn child(mut self, idx: usize) -> Zipper<Id, S> {
        // Remove the specified child from the node's children.
        //  Zipper should avoid having a parent reference
        // since parents will be mutated during node refocusing.
        // Vec::swap_remove() is used for efficiency.
        let child = self.node.children.swap_remove(idx);

        // Return a new Zipper focused on the specified child.
        Zipper {
            node: child,
            parent: Some(Box::new(self)),
            self_idx: idx,
        }
    }

    fn set_state(mut self, state: S) -> Zipper<Id, S> {
        self.node.state = state;
        self
    }

    fn insert_child(mut self, id: Id) -> Zipper<Id, S> {
        self.node.children.push(Node::new(id));
        self
    }

    fn parent(self) -> Zipper<Id, S> {
        // Destructure this Zipper
        // https://github.com/rust-lang/rust/issues/16293#issuecomment-185906859
        let Zipper {
            node,
            parent,
            self_idx,
        } = self;

        // Destructure the parent Zipper
        let mut parent = *parent.unwrap();

        // Insert the node of this Zipper back in its parent.
        // Since we used swap_remove() to remove the child,
        // we need to do the opposite of that.
        parent.node.children.push(node);
        let last_idx = parent.node.children.len() - 1;
        parent.node.children.swap(self_idx, last_idx);

        // Return a new Zipper focused on the parent.
        Zipper {
            node: parent.node,
            parent: parent.parent,
            self_idx: parent.self_idx,
        }
    }

    //  try something like Iterator::fold
    fn finish_insert(mut self, id: Id) -> Node<Id, S> {
        self.node.descendant_keys.insert(id);
        while self.parent.is_some() {
            self = self.parent();
            self.node.descendant_keys.insert(id);
        }

        self.node
    }

    pub fn finish_update(mut self) -> Node<Id, S> {
        while self.parent.is_some() {
            self = self.parent();
        }

        self.node
    }

    // only act on parent nodes
    fn finish_update_parent_id(self) -> (Option<Id>, Node<Id, S>) {
        let parent_id = self.parent.as_ref().map(|z| z.node.id);
        (parent_id, self.finish_update())
    }

    // act on all nodes
    fn finish_update_fn<F>(mut self, f: F) -> Node<Id, S>
    where
        F: Fn(Zipper<Id, S>) -> Node<Id, S> + Clone,
    {
        self.node.children = self
            .node
            .children
            .into_iter()
            .map(|n| n.zipper().finish_update_fn(f.clone()))
            .collect();
        f(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{node_state, Kind, State, StateId};

    node_state!(Alice, Bob, Charlie, Dave, Eve);

    #[test]
    fn insert_child_state() {
        let alice_id = StateId::new_rand(NodeKind::Alice);
        let bob_id = StateId::new_rand(NodeKind::Bob);
        let charlie_id = StateId::new_rand(NodeKind::Charlie);
        let dave_id = StateId::new_rand(NodeKind::Dave);
        let eve_id = StateId::new_rand(NodeKind::Eve);

        let mut tree = Node::new(alice_id);

        // =================================================
        // Graph should look like this after four insertions:
        //
        //       (Alice)
        //       /      \
        //     (Bob) (Charlie)
        //            /
        //       (Dave)
        //       /
        //  (Eve)
        // =================================================
        tree.insert(Insert {
            parent_id: Some(alice_id),
            id: bob_id,
        });
        tree.insert(Insert {
            parent_id: Some(alice_id),
            id: charlie_id,
        });
        tree.insert(Insert {
            parent_id: Some(charlie_id),
            id: dave_id,
        });
        tree.insert(Insert {
            parent_id: Some(dave_id),
            id: eve_id,
        });
        // =================================================

        // Bob =============================================
        let mut bob = tree.get_state(bob_id).unwrap();
        assert_eq!(bob, &NodeState::Bob(Bob::New));
        tree = tree.into_update(Update {
            id: bob_id,
            state: NodeState::Bob(Bob::Awaiting),
        });
        bob = tree.get_state(bob_id).unwrap();
        assert_eq!(bob, &NodeState::Bob(Bob::Awaiting));
        // =================================================

        // Charlie =========================================
        let mut charlie = tree.get_state(charlie_id).unwrap();
        assert_eq!(charlie, &NodeState::Charlie(Charlie::New));
        tree = tree.into_update(Update {
            id: charlie_id,
            state: NodeState::Charlie(Charlie::Awaiting),
        });
        charlie = tree.get_state(charlie_id).unwrap();
        assert_eq!(charlie, &NodeState::Charlie(Charlie::Awaiting));
        // =================================================

        // Dave ============================================
        let mut dave = tree.get_state(dave_id).unwrap();
        assert_eq!(dave, &NodeState::Dave(Dave::New));
        // Dave finished whatever it was that Dave was doing
        tree = tree.into_update(Update {
            id: dave_id,
            state: NodeState::Dave(Dave::Completed),
        });
        dave = tree.get_state(dave_id).unwrap();
        assert_eq!(dave, &NodeState::Dave(Dave::Completed));
        // =================================================

        // Eve =============================================
        let mut eve = tree.get_state(eve_id).unwrap();
        assert_eq!(eve, &NodeState::Eve(Eve::New));
        // Fail Eve (simulating timeout)
        tree = tree.into_update(Update {
            id: eve_id,
            state: NodeState::Eve(Eve::Failed),
        });
        eve = tree.get_state(eve_id).unwrap();
        assert_eq!(eve, &NodeState::Eve(Eve::Failed));
        // =================================================

        // =================================================
        // Eve failed! Fail everyone!
        // ...except for Dave, he is in "Completed" state
        // =================================================
        tree = tree.zipper().finish_update_fn(|mut z| {
            if !z.node.state.is_completed() {
                z.node.state.fail();
            }
            z.finish_update()
        });
        assert_eq!(&tree.state, &NodeState::Alice(Alice::Failed));
        assert_eq!(
            tree.get_state(bob_id).unwrap(),
            &NodeState::Bob(Bob::Failed)
        );
        assert_eq!(
            tree.get_state(charlie_id).unwrap(),
            &NodeState::Charlie(Charlie::Failed)
        );
        assert_eq!(
            tree.get_state(dave_id).unwrap(),
            &NodeState::Dave(Dave::Completed)
        );
        assert_eq!(
            tree.get_state(eve_id).unwrap(),
            &NodeState::Eve(Eve::Failed)
        );
    }
}
