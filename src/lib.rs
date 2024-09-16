use std::fmt;

use bigerror::reportable;
use tokio::time::Instant;
use tracing::error;
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
pub use timeout::Timeout;

/// A trait for types representing state machine lifecycles. These types should be field-less
/// enumerations or enumerations whose variants only contain field-less enumerations; note that
/// `Copy` is a required supertrait.
pub trait State: fmt::Debug + Send + PartialEq + Copy {
    type Input: Send + Sync + 'static + fmt::Debug;
}

/// Acts as a discriminant between various [`State`] enumerations, similar to
/// [`std::mem::Discriminant`].
/// Used to define the scope for [`Signal`]s cycled through a [`StateMachineManager`].
pub trait Kind: fmt::Debug + Send + Sized {
    type State: State<Input = Self::Input> + AsRef<Self>;
    type Input: Send + Sync + 'static + fmt::Debug;
    fn new_state(&self) -> Self::State;
    fn failed_state(&self) -> Self::State;
    fn completed_state(&self) -> Self::State;
    // /// represents a state that will no longer change
    fn is_terminal(state: Self::State) -> bool {
        let kind = state.as_ref();
        kind.completed_state() == state || kind.failed_state() == state
    }
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
///
/// Kind -> Rex::Message
///   ::              ::
///   State::Input    Topic
/// ```
pub trait Rex: Kind + HashKind
where
    Self::State: AsRef<Self>,
{
    type Message: RexMessage;
    fn state_input(&self, state: Self::State) -> Option<Self::Input>;
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
        write!(
            f,
            "{:?}<{}>",
            self.kind,
            (!self.is_nil())
                .then(|| bs58::encode(self.uuid).into_string())
                .unwrap_or_else(|| "NIL".to_string())
        )
    }
}

impl<K: Kind> StateId<K> {
    pub fn new(kind: K, uuid: Uuid) -> Self {
        Self { kind, uuid }
    }

    pub fn new_rand(kind: K) -> Self {
        Self::new(kind, Uuid::new_v4())
    }

    pub fn nil(kind: K) -> Self {
        Self::new(kind, Uuid::nil())
    }
    pub fn is_nil(&self) -> bool {
        self.uuid == Uuid::nil()
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

#[derive(Debug, thiserror::Error)]
#[error("StateMachineError")]
pub struct RexError;
reportable!(RexError);
