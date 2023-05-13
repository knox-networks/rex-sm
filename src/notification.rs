use std::{collections::HashMap, fmt, hash::Hash, sync::Arc};

use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::warn;

use bigerror::LogError;

use crate::{manager::HashKind, timeout::TimeoutInput};

/// Used to derive a marker used to route [`Notifications`]
/// to [`NotificationProcessor`]s
pub trait InputType: fmt::Debug {
    type Type: fmt::Debug + Hash + Eq + PartialEq + 'static + Send + Sync;
    fn get_type(&self) -> Self::Type;
}

/// Default [`Notification`] input type
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum DefaultType {
    Timeout,
    Gateway,
}

#[derive(Debug)]
pub enum DefaultInput<K>
where
    K: HashKind,
{
    Timeout(TimeoutInput<K>),
    #[allow(unused_variables)]
    Gateway,
}

impl<K> InputType for DefaultInput<K>
where
    K: HashKind,
{
    type Type = DefaultType;

    fn get_type(&self) -> Self::Type {
        match self {
            DefaultInput::Timeout(_) => DefaultType::Timeout,
            DefaultInput::Gateway => DefaultType::Gateway,
        }
    }
}

// --------------------------------------
// TODO: turn this into a proc macro
// https://crates.io/crates/enum-as-inner
impl<K> TryInto<TimeoutInput<K>> for DefaultInput<K>
where
    K: HashKind,
{
    type Error = Self;

    fn try_into(self) -> Result<TimeoutInput<K>, Self::Error> {
        if let DefaultInput::Timeout(input) = self {
            return Ok(input);
        }
        Err(self)
    }
}

impl<K> From<TimeoutInput<K>> for DefaultInput<K>
where
    K: HashKind,
{
    fn from(value: TimeoutInput<K>) -> Self {
        Self::Timeout(value)
    }
}
// --------------------------------------

/// This is the analogue to [`super::node_state_machine::Signal`]
/// that is meant to send messages to anything that is _not_ a
/// state machine
#[derive(Debug)]
pub struct Notification<I>(pub I)
where
    I: Send + Sync + fmt::Debug;

/// [`NotificationManager`] routes [`Notifications`] to their desired
/// destination
pub struct NotificationManager<I>
where
    I: Send + Sync + fmt::Debug + InputType,
{
    processors: Arc<HashMap<I::Type, UnboundedSender<I>>>,
}

impl<I> NotificationManager<I>
where
    I: Send + Sync + fmt::Debug + 'static + InputType,
{
    pub fn new<const N: usize>(processors: [&dyn NotificationProcessor<Input = I>; N]) -> Self {
        let processors: HashMap<I::Type, UnboundedSender<I>> = processors
            .into_iter()
            .map(|p| (p.get_type(), p.init()))
            .collect();
        Self {
            processors: Arc::new(processors),
        }
    }

    pub fn init(&self) -> UnboundedSender<Notification<I>> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Notification<I>>();
        let processors = self.processors.clone();
        tokio::spawn(async move {
            while let Some(Notification(input)) = input_rx.recv().await {
                if let Some(processor_tx) = processors.get(&input.get_type()) {
                    processor_tx.send(input).log_err();
                } else {
                    warn!(input_type = ?input.get_type(), "invalid type!");
                }
            }
        });
        input_tx
    }
}

#[async_trait::async_trait]
pub trait NotificationProcessor: Send + Sync {
    type Input: Send + Sync + fmt::Debug + InputType;

    fn init(&self) -> UnboundedSender<Self::Input>;
    fn get_type(&self) -> <Self::Input as InputType>::Type;
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
        let notification_manager = NotificationManager::new([&timeout_manager]);
        let notification_tx = notification_manager.init();

        let test_id = StateId::new_with_u128(TestKind, 1);
        // this should timeout instantly
        let timeout_duration = Duration::from_millis(1);

        let set_timeout = Notification(TimeoutInput::set_timeout(test_id, timeout_duration).into());

        notification_tx.send(set_timeout).unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(timeout_manager.signal_queue.pop_front().is_some());
    }
}
