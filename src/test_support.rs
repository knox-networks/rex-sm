use bigerror::{error_stack::Report, ConversionError, Reportable};
use tokio::time::Instant;

use super::{Kind, Rex, State};
use crate::{
    ingress::{Ingress, StateRouter},
    notification::{GetTopic, RexMessage},
    timeout::{NoRetain, Timeout, TimeoutInput, TimeoutMessage},
    RexError, StateId,
};

pub trait TestDefault {
    fn test_default() -> Self;
}

#[macro_export]
macro_rules! node_state {
    ($( $name: ident ),*) => {
        $(
        #[allow(dead_code)]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[allow(dead_code)]
pub enum TestTopic {
    Timeout,
    Ingress,
    Other,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TestMsg {
    TimeoutInput(TimeoutInput<TestKind>),
    Ingress(OutPacket),
    Other,
}

impl RexMessage for TestMsg {
    type Topic = TestTopic;
}

#[derive(Copy, Clone, Debug, derive_more::Display)]
pub struct Hold<T>(pub(crate) T);
impl TimeoutMessage<TestKind> for TestMsg {
    type Item = NoRetain;
}

impl GetTopic<TestTopic> for TestMsg {
    fn get_topic(&self) -> TestTopic {
        match self {
            TestMsg::TimeoutInput(_) => TestTopic::Timeout,
            TestMsg::Ingress(_) => TestTopic::Ingress,
            TestMsg::Other => TestTopic::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
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

impl Ingress for TestKind {
    type In = InPacket;
    type Out = OutPacket;
}

impl TryFrom<InPacket> for TestInput {
    type Error = Report<ConversionError>;

    fn try_from(packet: InPacket) -> Result<Self, Self::Error> {
        Ok(Self::Packet(packet))
    }
}
impl Timeout for TestKind {}

#[derive(Clone, Debug, PartialEq)]
pub enum TestInput {
    Timeout(Instant),
    Packet(InPacket),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutPacket(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq)]
pub struct InPacket(pub Vec<u8>);

impl Rex for TestKind {
    type Input = TestInput;
    type Message = TestMsg;

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
    fn get_id(&self, input: &Self::Inbound) -> Result<Option<StateId<TestKind>>, Report<RexError>> {
        let packet = &input.0;
        match packet {
            _ if packet.starts_with(b"unsupported") => Err(RexError::attach("wrong packet type")),
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

impl TryInto<OutPacket> for TestMsg {
    type Error = Report<ConversionError>;

    fn try_into(self) -> Result<OutPacket, Self::Error> {
        if let Self::Ingress(packet) = self {
            return Ok(packet);
        }
        Err(ConversionError::attach_dbg(self))
    }
}

impl TryInto<TimeoutInput<TestKind>> for TestMsg {
    type Error = Report<ConversionError>;

    fn try_into(self) -> Result<TimeoutInput<TestKind>, Self::Error> {
        if let Self::TimeoutInput(timeout) = self {
            return Ok(timeout);
        }
        Err(ConversionError::attach_dbg(self))
    }
}

impl From<OutPacket> for TestMsg {
    fn from(val: OutPacket) -> Self {
        TestMsg::Ingress(val)
    }
}

impl From<TimeoutInput<TestKind>> for TestMsg {
    fn from(value: TimeoutInput<TestKind>) -> Self {
        TestMsg::TimeoutInput(value)
    }
}
