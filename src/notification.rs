use std::{
    collections::HashMap,
    fmt::{self, Debug},
    hash::Hash,
    sync::Arc,
};

use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{debug, trace, warn, Instrument};

use bigerror::LogError;

use crate::{HashKind, Rex, StateId};

// a PubSub message that is able to be sent to [`NotificationProcessor`]s that subscribe to one
// or more [`RexTopic`]s
pub trait RexMessage: GetTopic<Self::Topic> + Clone + fmt::Debug + Send + Sync + 'static
where
    Self: Send + Sync,
{
    type Topic: RexTopic;
}

// TODO add #[from_inner] attribute macro
#[macro_export]
macro_rules! from_inner {
    ($($msg:ident::$variant:ident($inner:path))*) => {
        $(
            impl From<$inner> for $msg {
                fn from(inner: $inner) -> Self {
                    $msg::$variant(inner)
                }
            }
        )*
    };
}

pub trait ToNotification<M>
where
    M: RexMessage,
{
    fn notification(self) -> Notification<M>;
}

impl<T, M> ToNotification<M> for T
where
    T: Into<M>,
    M: RexMessage,
{
    fn notification(self) -> Notification<M> {
        let msg: M = self.into();
        Notification(msg)
    }
}

/// Used to derive a marker used to route [`Notification`]s
/// to [`NotificationProcessor`]s
pub trait GetTopic<T: RexTopic>: fmt::Debug {
    fn get_topic(&self) -> T;
}

/// This is the analogue to [`super::node_state_machine::Signal`]
/// that is meant to send messages to anything that is _not_ a
/// state machine
#[derive(Debug, Clone)]
pub struct Notification<M: RexMessage>(pub M);

impl<M, T> GetTopic<T> for Notification<M>
where
    T: RexTopic,
    M: RexMessage + GetTopic<T>,
{
    fn get_topic(&self) -> T {
        self.0.get_topic()
    }
}

pub trait RexTopic: fmt::Debug + Hash + Eq + PartialEq + Copy + Send + Sync + 'static {}
impl<T> RexTopic for T where T: fmt::Debug + Hash + Eq + PartialEq + Copy + Send + Sync + 'static {}

// --------------------------------------

pub type Subscriber<M> = UnboundedSender<Notification<M>>;
/// [`NotificationManager`] routes [`Notifications`] to their desired
/// destination
pub struct NotificationManager<M>
where
    M: RexMessage,
{
    processors: Arc<HashMap<M::Topic, Vec<Subscriber<M>>>>,
}

impl<M> NotificationManager<M>
where
    M: RexMessage,
{
    pub fn new(processors: &[&dyn NotificationProcessor<M>]) -> Self {
        let processors: HashMap<M::Topic, Vec<UnboundedSender<Notification<M>>>> = processors
            .iter()
            .fold(HashMap::new(), |mut subscribers, processor| {
                let subscriber_tx = processor.init();
                for topic in processor.get_topics() {
                    subscribers
                        .entry(*topic)
                        .or_default()
                        .push(subscriber_tx.clone());
                }
                subscribers
            });
        Self {
            processors: Arc::new(processors),
        }
    }

    pub fn init(&self) -> UnboundedSender<Notification<M>> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Notification<M>>();
        let processors = self.processors.clone();
        tokio::spawn(async move {
            debug!(spawning = "NotificationManager.processors");
            while let Some(notification) = input_rx.recv().await {
                trace!(?notification);
                let topic = notification.get_topic();
                if let Some(subscribers) = processors.get(&topic) {
                    let Some((last, rest)) = subscribers.split_last() else {
                        continue;
                    };
                    for tx in rest {
                        tx.send(notification.clone()).log_attached_err(format!(
                            "nm::processors send failed for topic {topic:?}"
                        ));
                    }
                    last.send(notification).log_attached_err(format!(
                        "nm::processors send last failed for topic {topic:?}"
                    ));
                } else {
                    warn!(topic = ?notification.get_topic(), ?notification, "NotificationProcessor not found");
                }
            }
        }.in_current_span());
        input_tx
    }
}

pub trait NotificationProcessor<M>: Send + Sync
where
    M: RexMessage,
{
    fn init(&self) -> UnboundedSender<Notification<M>>;
    fn get_topics(&self) -> &[M::Topic];
}

/// A message that is expected to return a result
/// to the associated [`StateId`] that that did the initial request
#[derive(Debug, Clone)]
pub struct UnaryRequest<K, O>
where
    K: HashKind,
    O: Operation,
{
    pub id: StateId<K>,
    pub op: O,
}

impl<K, O> UnaryRequest<K, O>
where
    K: HashKind,
    O: Operation,
{
    pub fn new(id: StateId<K>, op: O) -> Self {
        Self { id, op }
    }
}

impl<K: HashKind, O: Operation + Copy> Copy for UnaryRequest<K, O> {}

/// Defines the unit of work held by a [`UnaryRequest`]
pub trait Operation: std::fmt::Display + Clone {}
impl<Op> Operation for Op where Op: std::fmt::Display + Clone {}

pub trait Request<K>
where
    K: HashKind,
    Self: Operation,
{
    fn request(self, id: StateId<K>) -> UnaryRequest<K, Self>;
}

impl<K: Rex, Op: Operation> Request<K> for Op
where
    K::Message: From<UnaryRequest<K, Op>>,
{
    fn request(self, id: StateId<K>) -> UnaryRequest<K, Op> {
        UnaryRequest { id, op: self }
    }
}

pub trait RequestInner<K>
where
    K: HashKind,
    Self: Sized,
{
    fn request_inner<Op>(self, id: StateId<K>) -> UnaryRequest<K, Op>
    where
        Op: Operation + From<Self>;
}

impl<K, T> RequestInner<K> for T
where
    K: HashKind,
{
    fn request_inner<Op>(self, id: StateId<K>) -> UnaryRequest<K, Op>
    where
        Op: Operation + From<T>,
    {
        UnaryRequest {
            id,
            op: self.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{test_support::*, StateId, TestDefault};

    #[tokio::test]
    async fn route_to_timeout_manager() {
        use crate::timeout::*;

        let timeout_manager = TimeoutManager::test_default();
        let timeout_manager_two = TimeoutManager::test_default();
        let notification_manager: NotificationManager<TestMsg> =
            NotificationManager::new(&[&timeout_manager, &timeout_manager_two]);
        let notification_tx = notification_manager.init();

        let test_id = StateId::new_with_u128(TestKind, 1);
        // this should timeout instantly
        let timeout_duration = Duration::from_millis(1);

        let set_timeout = Notification(TimeoutInput::set_timeout(test_id, timeout_duration).into());
        notification_tx.send(set_timeout).unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        let timeout_one = timeout_manager
            .signal_queue
            .pop_front()
            .expect("timeout one");
        let timeout_two = timeout_manager_two
            .signal_queue
            .pop_front()
            .expect("timeout two");
        assert_eq!(timeout_one.id, timeout_two.id);
    }
}
