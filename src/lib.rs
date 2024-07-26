use std::fmt;

use bigerror::ThinContext;
use tokio::time::Instant;
use uuid::Uuid;

pub mod builder;
pub mod ingress;
pub mod manager;
pub mod node;
pub mod notification;
pub mod queue;
pub mod storage;
pub mod timeout;

#[cfg(test)]
mod test_support;

pub use builder::RexBuilder;
pub use manager::{
    HashKind, Signal, SignalExt, SignalQueue, SmContext, StateMachine, StateMachineExt,
    StateMachineManager,
};
pub use notification::{
    GetTopic, Notification, NotificationManager, NotificationProcessor, NotificationQueue,
    Operation, Request, RequestInner, RexMessage, RexTopic, Subscriber, UnaryRequest,
};

/// A trait for types representing state machine lifecycles. These types should be field-less
/// enumerations or enumerations whose variants only contain field-less enumerations; note that
/// `Copy` is a required supertrait.
pub trait State: fmt::Debug + Send + PartialEq + Copy {
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

    /// represents a state that will no longer change
    fn is_terminal(&self) -> bool
    where
        Self: Sized,
    {
        self.is_failed() || self.is_completed()
    }

    /// `&dyn Kind<State = Self>` cannot do direct partial comparison
    /// due to type opacity
    /// so State::new_state(self) is called to allow a vtable lookup
    fn kind_eq(&self, kind: &dyn Kind<State = Self>) -> bool
    where
        Self: Sized,
    {
        self.get_kind().new_state() == kind.new_state()
    }
}

/// Acts as a discriminant between various [`State`] enumerations, similar to
/// [`std::mem::Discriminant`].
/// Used to define the scope for [`Signal`]s cycled through a [`StateMachineManager`].
pub trait Kind: fmt::Debug + Send {
    type State: State;
    fn new_state(&self) -> Self::State;
    fn failed_state(&self) -> Self::State;
    fn completed_state(&self) -> Self::State;
}

/// Titular trait of the library that enables Hierarchical State Machine (HSM for short) behaviour.
/// Makes sending [`Signal`]s (destined to become a [`StateMachine`]'s input)
/// and [`Notification`]s (a [`NotificationProcessor`]'s input) possible.
///
/// The [`Rex`] trait defines the _scope_ of interaction that exists between one or more
/// [`StateMachine`]s and zero or more [`NotificationProcessor`]s.
/// Below is a diagram displaying the associated
/// type hierarchy defined by validly implementing the [`Kind`] and [`Rex`] traits,
/// double colons`::` directed down and right represent type associations:
/// ```text
///            Topic
///            ::
/// Kind -> Rex::Message
/// ::      ::      ::
/// State   Input   Topic
/// ```
pub trait Rex: Kind + HashKind {
    type Input: Send + Sync + 'static + fmt::Debug;
    type Message: RexMessage;
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

impl<K> std::ops::Deref for StateId<K>
where
    K: Kind,
{
    type Target = K;

    fn deref(&self) -> &Self::Target {
        &self.kind
    }
}

impl<K> fmt::Display for StateId<K>
where
    K: Kind,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({:?}<{}>)", self.kind, self.uuid)
    }
}

impl<K: Kind> StateId<K> {
    pub fn new(kind: K, uuid: Uuid) -> Self {
        Self { kind, uuid }
    }

    pub fn new_rand(kind: K) -> Self {
        Self::new(kind, Uuid::new_v4())
    }
    // for testing purposes, easily distinguish UUIDs
    // by numerical value
    #[cfg(test)]
    pub fn new_with_u128(kind: K, v: u128) -> Self {
        Self {
            kind,
            uuid: Uuid::from_u128(v),
        }
    }
}

#[derive(ThinContext)]
pub struct RexError;
