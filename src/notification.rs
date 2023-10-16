use std::{
    collections::HashMap,
    fmt::{self, Debug},
    hash::Hash,
    sync::Arc,
};

use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{debug, trace, warn};

use bigerror::LogError;

use crate::{manager::HashKind, timeout::TimeoutInput};

// represents custom Notifications that are sent to topics
pub trait Message: fmt::Debug + Send + Sync + 'static
where
    Self: Send + Sync,
{
}

impl<T> Message for T where T: fmt::Debug + Send + Sync + 'static {}

/// Used to derive a marker used to route [`Notification`]s
/// to [`NotificationProcessor`]s
pub trait GetTopic<T>: fmt::Debug {
    fn get_topic(&self) -> Topic<T>;
}

// `()` is meant to be used for unused messages/msg topics
impl GetTopic<()> for () {
    fn get_topic(&self) -> Topic<()> {
        Topic::Message(())
    }
}

// routing logic for [`Notification`]s that implement
// [`GetTopic`]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum Topic<T> {
    Timeout,
    Message(T),
}

/// This is the analogue to [`super::node_state_machine::Signal`]
/// that is meant to send messages to anything that is _not_ a
/// state machine
#[derive(Debug, Clone)]
pub enum Notification<K, M>
where
    K: HashKind,
    M: Message,
{
    Timeout(TimeoutInput<K>),
    Message(M),
}

impl<K, M, T> GetTopic<T> for Notification<K, M>
where
    M: Message + GetTopic<T>,
    K: HashKind,
{
    fn get_topic(&self) -> Topic<T> {
        match self {
            Notification::Timeout(_) => Topic::Timeout,
            Notification::Message(msg) => msg.get_topic(),
        }
    }
}

impl<K, M> TryInto<TimeoutInput<K>> for Notification<K, M>
where
    K: HashKind,
    M: Message,
{
    type Error = Self;

    fn try_into(self) -> Result<TimeoutInput<K>, Self::Error> {
        if let Notification::Timeout(input) = self {
            return Ok(input);
        }
        Err(self)
    }
}

impl<K, M> From<TimeoutInput<K>> for Notification<K, M>
where
    K: HashKind,
    M: Message,
{
    fn from(value: TimeoutInput<K>) -> Self {
        Self::Timeout(value)
    }
}
// --------------------------------------

/// [`NotificationManager`] routes [`Notifications`] to their desired
/// destination
pub struct NotificationManager<T, N>
where
    N: Send + Sync + fmt::Debug + Clone + GetTopic<T>,
    T: fmt::Debug + Hash + Eq + PartialEq + 'static + Copy,
{
    processors: Arc<HashMap<Topic<T>, Vec<UnboundedSender<N>>>>,
}

impl<T, N> NotificationManager<T, N>
where
    T: fmt::Debug + Hash + Eq + PartialEq + 'static + Copy + Send + Sync,
    N: Send + Sync + fmt::Debug + Clone + 'static + GetTopic<T>,
{
    pub fn new<const U: usize>(processors: [&dyn NotificationProcessor<T, N>; U]) -> Self {
        let processors: HashMap<Topic<T>, Vec<UnboundedSender<N>>> =
            processors
                .into_iter()
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

    pub fn init(&self) -> UnboundedSender<N> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<N>();
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
                            "nm::processors send failed for topic {:?}",
                            topic
                        ));
                    }
                    last.send(notification).log_attached_err(format!(
                        "nm::processors send last failed for topic {:?}",
                        topic
                    ));
                } else {
                    warn!(input_type = ?notification.get_topic(), ?notification, "invalid type!");
                }
            }
        });
        input_tx
    }
}

#[async_trait::async_trait]
pub trait NotificationProcessor<T, N>: Send + Sync
where
    N: GetTopic<T>,
{
    fn init(&self) -> UnboundedSender<N>;
    fn get_topics(&self) -> &[Topic<T>];
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
        let notification_manager: NotificationManager<TestTopic, Notification<_, TestMsg>> =
            NotificationManager::new([&timeout_manager, &timeout_manager_two]);
        let notification_tx = notification_manager.init();

        let test_id = StateId::new_with_u128(TestKind, 1);
        // this should timeout instantly
        let timeout_duration = Duration::from_millis(1);

        let set_timeout =
            Notification::Timeout(TimeoutInput::set_timeout(test_id, timeout_duration));
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
