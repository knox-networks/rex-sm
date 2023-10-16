use std::fmt;

use bigerror::reportable;
use tokio::time::Instant;
use tracing::error;
use uuid::Uuid;

pub mod ingress;
pub mod manager;
pub mod node;
pub mod notification;
pub mod queue;
pub mod storage;
pub mod timeout;

#[cfg(test)]
mod test_support;

pub use manager::{HashKind, Signal, SignalQueue};

/// A trait representing a distinct zero sized state lifecycle
pub trait State: fmt::Debug + Send + PartialEq {
    fn get_kind(&self) -> &dyn Kind<State = Self>;
    fn fail(&mut self)
    where
        Self: Sized,
    {
        *self = self.get_kind().failed_state();
    }
    fn complete(&mut self)
    where
        Self: Sized,
    {
        *self = self.get_kind().completed_state();
    }
    fn is_completed(&self) -> bool
    where
        Self: Sized,
        for<'a> &'a Self: PartialEq<&'a Self>,
    {
        self == &self.get_kind().completed_state()
    }

    fn is_failed(&self) -> bool
    where
        Self: Sized,
        for<'a> &'a Self: PartialEq<&'a Self>,
    {
        self == &self.get_kind().failed_state()
    }
    fn is_new(&self) -> bool
    where
        Self: Sized,
        for<'a> &'a Self: PartialEq<&'a Self>,
    {
        self == &self.get_kind().new_state()
    }
}

/// Represents an association of [`State`] enumerations
/// Used to define the scope for [`node_state_machine::Signal`]s
/// cycled through a [`node_state_machine::StateMachineManager`]
pub trait Kind: fmt::Debug + Send {
    type State;
    fn new_state(&self) -> Self::State;
    fn failed_state(&self) -> Self::State;
    fn completed_state(&self) -> Self::State;
}

/// Extends [`Kind`] behaviour to include operations involving [`Signal`] `input`s
pub trait KindExt: Kind {
    type Input;
    fn state_input(&self, state: <Self as Kind>::State) -> Option<Self::Input>;
    fn timeout_input(&self, instant: Instant) -> Option<Self::Input>;
}

/// Implements [`node::Node`] `Id` generics by holding a [`Kind`] field
/// and a [`Uuid`] to be used as a _distinguishing_ identifier
#[derive(Debug, Hash, Eq, PartialEq, Copy, Clone)]
pub struct StateId<K: Kind> {
    pub kind: K,
    pub uuid: Uuid,
}

impl<K> StateId<K>
where
    K: Kind + fmt::Debug,
{
    pub fn deref_uuid(&self) -> &Uuid {
        &self.uuid
    }
}

impl<K> std::ops::Deref for StateId<K>
where
    K: Kind + fmt::Debug,
{
    type Target = K;

    fn deref(&self) -> &Self::Target {
        &self.kind
    }
}

impl<K> fmt::Display for StateId<K>
where
    K: Kind + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl<K: Kind> StateId<K> {
    pub fn new(kind: K, uuid: Uuid) -> Self {
        Self { kind, uuid }
    }

    #[cfg(any(feature = "test", test))]
    pub fn new_rand(kind: K) -> Self {
        Self::new(kind, Uuid::new_v4())
    }
    // for testing purposes, easily distinguish UUIDs
    // by numerical value
    #[cfg(any(feature = "test", test))]
    pub fn new_with_u128(kind: K, v: u128) -> Self {
        Self {
            kind,
            uuid: Uuid::from_u128(v),
        }
    }
}

#[cfg(any(feature = "test", test))]
pub trait TestDefault {
    fn test_default() -> Self;
}

#[derive(Debug, thiserror::Error)]
#[error("StateMachineError")]
pub struct StateMachineError;
reportable!(StateMachineError);
