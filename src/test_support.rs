use bigerror::{error_stack::Report, ConversionError, Reportable};
use tokio::time::Instant;

use crate::{
    ingress::StateRouter,
    notification::{GetTopic, Notification, Topic},
    StateId, StateMachineError,
};

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

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum TestTopic {
    Ingress,
    Other,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TestMsg {
    Ingress(OutPacket),
    Other,
}

impl GetTopic<TestTopic> for TestMsg {
    fn get_topic(&self) -> Topic<TestTopic> {
        match self {
            TestMsg::Ingress(_) => Topic::Message(TestTopic::Ingress),
            TestMsg::Other => Topic::Message(TestTopic::Other),
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

#[derive(Clone, Debug, PartialEq)]
pub enum TestInput {
    Timeout(Instant),
    Packet(InPacket),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutPacket(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq)]
pub struct InPacket(pub Vec<u8>);

impl KindExt for TestKind {
    type Input = TestInput;

    fn state_input(&self, _state: <Self as Kind>::State) -> Option<Self::Input> {
        unimplemented!()
    }

    fn timeout_input(&self, instant: tokio::time::Instant) -> Option<Self::Input> {
        Some(TestInput::Timeout(instant))
    }
}

pub struct TestStateRouter;
impl StateRouter<TestKind> for TestStateRouter {
    type Inbound = InPacket;
    fn get_id(
        &self,
        input: &Self::Inbound,
    ) -> Result<Option<StateId<TestKind>>, Report<StateMachineError>> {
        let packet = &input.0;
        match packet {
            _ if packet.starts_with(b"unsupported") => {
                Err(StateMachineError::attach("wrong packet type"))
            }
            _ if packet.starts_with(b"unknown") => Ok(None),
            _ if packet.starts_with(b"new_state") => Ok(Some(StateId::new_with_u128(TestKind, 1))),
            _ => unimplemented!(),
        }
    }

    fn get_kind(&self) -> TestKind {
        TestKind
    }
}
impl<'a> TryFrom<&'a InPacket> for TestKind {
    type Error = Report<ConversionError>;
    fn try_from(_value: &'a InPacket) -> Result<Self, Self::Error> {
        Ok(TestKind)
    }
}

impl TryFrom<InPacket> for TestInput {
    type Error = Report<ConversionError>;
    fn try_from(value: InPacket) -> Result<Self, Self::Error> {
        Ok(Self::Packet(value))
    }
}

impl TryFrom<Notification<TestKind, TestMsg>> for OutPacket {
    type Error = Report<ConversionError>;

    fn try_from(value: Notification<TestKind, TestMsg>) -> Result<Self, Self::Error> {
        if let Notification::Message(TestMsg::Ingress(packet)) = value {
            return Ok(packet);
        }
        Err(ConversionError::attach_debug(value))
    }
}

impl From<OutPacket> for Notification<TestKind, TestMsg> {
    fn from(val: OutPacket) -> Self {
        Notification::Message(TestMsg::Ingress(val))
    }
}
