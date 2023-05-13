use tokio::time::Instant;

use super::{Kind, KindExt, State};

#[macro_export]
macro_rules! node_state {
    ($( $name: ident ),*) => {
        $(
        #[allow(dead_code)]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum $name {
            New,
            Awaiting,
            Completed,
            Failed,
        }
        )*

        #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
        pub enum NodeKind {
            $( $name, )*
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum NodeState {
            $( $name($name), )*
        }

        impl State for NodeState {
            fn get_kind(&self) -> &dyn Kind<State = NodeState> {
                match self {
                    $( NodeState::$name(_) => &NodeKind::$name, )*
                }
            }
        }

        impl Kind for NodeKind {
            type State = NodeState;

            fn new_state(&self) -> Self::State {
                match self {
                    $( NodeKind::$name => NodeState::$name($name::New), )*
                }
            }

            fn failed_state(&self) -> Self::State {
                match self {
                    $( NodeKind::$name => NodeState::$name($name::Failed), )*
                }
            }

            fn completed_state(&self) -> Self::State {
                match self {
                    $( NodeKind::$name => NodeState::$name($name::Completed), )*
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestState {
    New,
    Awaiting,
    Completed,
    Failed,
}

impl State for TestState {
    fn get_kind(&self) -> &dyn Kind<State = TestState> {
        &TestKind
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct TestKind;

impl Kind for TestKind {
    type State = TestState;

    fn new_state(&self) -> Self::State {
        TestState::New
    }

    fn failed_state(&self) -> Self::State {
        TestState::Failed
    }

    fn completed_state(&self) -> Self::State {
        TestState::Completed
    }
}

#[derive(Clone, Debug)]
pub enum TestInput {
    Timeout(Instant),
}

impl KindExt for TestKind {
    type Input = TestInput;

    fn state_input(&self, _state: <Self as Kind>::State) -> Option<Self::Input> {
        unimplemented!()
    }

    fn timeout_input(&self, instant: tokio::time::Instant) -> Option<Self::Input> {
        Some(TestInput::Timeout(instant))
    }
}
