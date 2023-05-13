# State Storage using `Zipper`

The Data structure used to store various states uses a cursor to traverse the nodes known as the `Zipper` technique.


Example of a Zipper cursor traversing a Vector,
the *focus* provides a view "Up" and "Down" the data:
```
[1, 2, 3, 4, 5] // array with 5 entries, no zipper
 1}[2, 3, 4, 5] // zipper starts with focus at first index
[1] 2}[3, 4, 5] // moving down the array
[2, 1] 3}[4, 5]
[3, 2, 1] 4}[5]
[4, 3, 2, 1]{5  // zipper travels back up the array
```



## Zipper Advantages

* The _focus_ (cursor) of a zipper is able to move up and down a tree based on predefined rules
* Zippers can accommodate cyclical references as well as shared regions.
* data structures that implement a zipper do not require parent references. The zipper builds parent references during traversal and removes them after finishing.
* Zippers can accommodate a wide array of data structures including arrays, trees, context-free grammars, and concrete syntax trees.
* Zippers can spawn

## Zipper Disadvantages

* Whenever a data structure is accessed with a zipper, a mutually exclusive lock must be enforced since both writes and reads mutate the underlying data structure by moving nodes into parent and child regions of the zipper.
* Zippers do not tolerate partial state updates very well if a deep traversal of the data structure is needed due to the aforementioned mutex.


## Basic Rust implementation

```rust
#[derive(Debug)]
struct Zipper<T> {
    node: Node<T>,
    parent: Option<Box<Zipper<T>>>,
    idx_to_self: usize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct Node<T> {
    data: T,
    children: Vec<Node<T>>,
}

impl<T> Zipper<T> {
    fn child(mut self, index: usize) -> Zipper<T> {
        // Remove the specified child from the node's children.
        // A Zipper shouldn't let its users inspect its parent,
        // since we mutate the parents
        // to move the focused nodes out of their list of children.
        // We use swap_remove() for efficiency.
        let child = self.node.children.swap_remove(index);

        // Return a new Zipper focused on the specified child.
        Zipper {
            node: child,
            parent: Some(Box::new(self)),
            idx_to_self: index,
        }
    }

    fn parent(self) -> Zipper<T> {
            // Destructure this Zipper
            // https://github.com/rust-lang/rust/issues/16293#issuecomment-185906859
            let Zipper {
                node,
                parent,
                idx_to_self,
            } = self;

            // Destructure the parent Zipper
            let mut parent = *parent.unwrap();

            // Insert the node of this Zipper back in its parent.
            // Since we used swap_remove() to remove the child,
            // we need to do the opposite of that.
            parent.node.children.push(node);
            let last_idx = parent.node.children.len() - 1;
            parent.node.children.swap(idx_to_self, last_idx);

            // Return a new Zipper focused on the parent.
            Zipper {
                node: parent.node,
                parent: parent.parent,
                idx_to_self: parent.idx_to_self,
            }
        }

        fn finish(mut self) -> Node<T> {
            while self.parent.is_some() {
                self = self.parent();
            }

            self.node
        }
}

impl<T> Node<T> {
    fn zipper(self) -> Zipper<T> {
        Zipper {
            node: self,
            parent: None,
            idx_to_self: 0,
        }
    }
}

fn main() {
    let mut node: Node<&str> = Node {
        data: "me",
        children: vec![Node {
            data: "them",
            children: vec![],
        }],
    };
    let expected_node = node.clone();

    // access the first child as part of the zipper cursor
    let child_focus = node.zipper().child(0);
    assert_eq!(child_focus.node.data, "them");
    // finish the zipper
    node = child_focus.finish();
    assert_eq!(expected_node, node);
}
```


## References

* [Parsing with Zippers (Functional Pearl)](https://dl.acm.org/doi/epdf/10.1145/3408990)
* [Functional pearl code (OCaml)](https://github.com/pdarragh/parsing-with-zippers-paper-artifact)
* [Video demonstration](https://www.youtube.com/watch?v=6Wi-Kc6LDhc)
* [Clojure implementation](https://www.youtube.com/watch?v=GzM9ASu2luw)
