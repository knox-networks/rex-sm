#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{
        mpsc::{self, UnboundedSender},
        Mutex,
    },
    time::Instant,
};
use tracing::{debug, warn};

use crate::{
    manager::{HashKind, Signal},
    notification::{DefaultInput, DefaultType, InputType, NotificationProcessor},
    queue::StreamableDeque,
    Kind, KindExt, StateId,
};

/// TimeoutLedger` contains a [`BTreeMap`] that uses [`Instant`]s to time out
/// specific [`StateId`]s and a [`HashMap`] that indexes `Instant`s by [`StateId`].
///
/// This double indexing allows [`Operation::Cancel`]s to go
/// through without having to provide an `Instant`.
#[derive(Debug)]
struct TimeoutLedger<K>
where
    K: Kind,
{
    timers: BTreeMap<Instant, HashSet<StateId<K>>>,
    ids: HashMap<StateId<K>, Instant>,
}

impl<K> TimeoutLedger<K>
where
    K: HashKind + Copy,
{
    fn new() -> Self {
        Self {
            timers: BTreeMap::new(),
            ids: HashMap::new(),
        }
    }

    // set timeout for a given instant and associate it with a given id
    // remove old instants associated with the same id if they exist
    fn set_timeout(&mut self, id: StateId<K>, instant: Instant) {
        if let Some(old_instant) = self.ids.insert(id, instant) {
            // remove older reference to id
            // if instants differ
            if old_instant != instant {
                debug!(?id, "renewing timeout");
                self.timers.get_mut(&old_instant).map(|set| set.remove(&id));
            }
        }

        self.timers
            .entry(instant)
            .and_modify(|set| {
                set.insert(id);
            })
            .or_default()
            .insert(id);
    }

    // remove existing timeout by id, this should remove
    // one entry in `self.ids` and one entry in `self.timers[id_instant]`
    fn cancel_timeout(&mut self, id: StateId<K>) {
        if let Some(instant) = self.ids.remove(&id) {
            // remove reference to id
            // from associated instant
            let removed_id = self.timers.get_mut(&instant).map(|set| set.remove(&id));
            // if
            //   `instant` is missing from `self.timers`
            // or
            //   `id` is missing from `self.timers[instant]`:
            //   warn
            if matches!(removed_id, None | Some(false)) {
                warn!("timers[{instant:?}][{id}] not found, cancellation ignored");
            } else {
                warn!(?id, "cancelled timeout");
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Operation {
    Cancel,
    Set(Instant),
}

impl Operation {
    pub fn from_duration(duration: Duration) -> Self {
        Self::Set(Instant::now() + duration)
    }

    pub fn from_millis(millis: u64) -> Self {
        Self::Set(Instant::now() + Duration::from_millis(millis))
    }
}

#[derive(Copy, Clone, Debug)]
pub struct TimeoutInput<K>
where
    K: HashKind,
{
    pub(crate) id: StateId<K>,
    pub(crate) op: Operation,
}

impl<K> TimeoutInput<K>
where
    K: HashKind,
{
    pub fn set_timeout_millis(id: StateId<K>, millis: u64) -> Self {
        Self {
            id,
            op: Operation::from_millis(millis),
        }
    }

    pub fn set_timeout(id: StateId<K>, duration: Duration) -> Self {
        Self {
            id,
            op: Operation::from_duration(duration),
        }
    }

    pub fn cancel_timeout(id: StateId<K>) -> Self {
        Self {
            id,
            op: Operation::Cancel,
        }
    }

    #[cfg(test)]
    fn with_id(&self, id: StateId<K>) -> Self {
        Self { id, ..*self }
    }
    #[cfg(test)]
    fn with_op(&self, op: Operation) -> Self {
        Self { op, ..*self }
    }
}

/// Processes incoming [`TimeoutInput`]s and modifies the [`TimeoutLedger`]
/// through a polling loop.
pub struct TimeoutManager<K, SI>
where
    K: HashKind,
    SI: Send + Sync + 'static + fmt::Debug,
{
    // the interval at which  the TimeoutLedger checks for timeouts
    tick_rate: Duration,
    ledger: Arc<Mutex<TimeoutLedger<K>>>,
    pub(crate) signal_queue: Arc<StreamableDeque<Signal<K, SI>>>,
}

impl<K, SI> TimeoutManager<K, SI>
where
    K: HashKind + KindExt<Input = SI> + Copy,
    SI: Send + Sync + 'static + fmt::Debug,
{
    pub fn new(tick_rate: Duration, signal_queue: Arc<StreamableDeque<Signal<K, SI>>>) -> Self {
        Self {
            tick_rate,
            signal_queue,
            ledger: Arc::new(Mutex::new(TimeoutLedger::new())),
        }
    }

    pub fn init_inner<NI>(&self) -> UnboundedSender<NI>
    where
        NI: TryInto<TimeoutInput<K>> + Send + 'static,
    {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<NI>();
        let in_ledger = self.ledger.clone();

        tokio::spawn(async move {
            while let Some(notification) = input_rx.recv().await {
                let Ok(TimeoutInput{ id, op })  = notification.try_into() else {
                    warn!("Invalid input");
                    continue;
                };
                let mut ledger = in_ledger.lock().await;
                match op {
                    Operation::Cancel => {
                        ledger.cancel_timeout(id);
                    }
                    Operation::Set(instant) => {
                        ledger.set_timeout(id, instant);
                    }
                }
            }
        });

        let timer_ledger = self.ledger.clone();
        let tick_rate = self.tick_rate;
        let signal_queue = self.signal_queue.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick_rate).await;

                let now = Instant::now();
                let mut ledger = timer_ledger.lock().await;
                // Get all instants where `instant <= now`
                let expired: Vec<Instant> = ledger.timers.range(..=now).map(|(k, _)| *k).collect();

                for id in expired
                    .iter()
                    .filter_map(|t| ledger.timers.remove(t))
                    .flat_map(|set| set.into_iter())
                    .collect::<Vec<_>>()
                {
                    warn!(?id, "timed out");
                    ledger.ids.remove(&id);
                    if let Some(input) = id.timeout_input(now) {
                        // caveat with this push_front setup is
                        // that later timeouts will be on top of the stack
                        signal_queue.push_front(Signal { id, input });
                    } else {
                        warn!("timeout not supported!");
                    }
                }
            }
        });

        input_tx
    }
}

impl<K, I> NotificationProcessor for TimeoutManager<K, I>
where
    K: HashKind + KindExt<Input = I> + Copy,
    I: Send + Sync + 'static + fmt::Debug,
{
    type Input = DefaultInput<K>;

    fn init(&self) -> UnboundedSender<Self::Input> {
        self.init_inner()
    }

    fn get_type(&self) -> <Self::Input as InputType>::Type {
        DefaultType::Timeout
    }
}

#[cfg(test)]
pub(crate) const TEST_TICK_RATE: Duration = Duration::from_millis(1);

#[cfg(test)]
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_millis(11);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_support::*, TestDefault};

    impl TestDefault for TimeoutManager<TestKind, TestInput> {
        fn test_default() -> Self {
            let signal_queue = Arc::new(StreamableDeque::<Signal<TestKind, TestInput>>::new());
            TimeoutManager::new(TEST_TICK_RATE, signal_queue)
        }
    }

    #[tokio::test]
    async fn timeout_to_signal() {
        let timeout_manager = TimeoutManager::test_default();

        let timeout_tx = timeout_manager.init();

        let test_id = StateId::new(TestKind);
        let timeout_duration = Duration::from_millis(5);

        let timeout = Instant::now() + timeout_duration;
        let set_timeout = TimeoutInput::set_timeout(test_id, timeout_duration);

        timeout_tx.send(set_timeout.into()).unwrap();

        // ensure two ticks have passed
        tokio::time::sleep(timeout_duration * 3).await;

        let Signal { id, input } = timeout_manager.signal_queue.pop_front().unwrap();
        assert_eq!(test_id, id);

        let TestInput::Timeout(signal_timeout) = input;
        assert!(
            signal_timeout >= timeout,
            "out[{signal_timeout:?}] >= in[{timeout:?}]"
        );
    }

    #[tokio::test]
    async fn timeout_cancellation() {
        let timeout_manager = TimeoutManager::test_default();

        let timeout_tx = timeout_manager.init();

        let test_id = StateId::new(TestKind);
        let set_timeout = TimeoutInput::set_timeout_millis(test_id, 10);

        timeout_tx.send(set_timeout.into()).unwrap();

        tokio::time::sleep(Duration::from_millis(2)).await;
        let cancel_timeout = TimeoutInput {
            id: test_id,
            op: Operation::Cancel,
        };
        timeout_tx.send(cancel_timeout.into()).unwrap();

        // wait out the rest of the duration and 3 ticks
        tokio::time::sleep(Duration::from_millis(3) + TEST_TICK_RATE * 2).await;

        // we should not be getting any signal since the timeout was cancelled
        assert!(timeout_manager.signal_queue.pop_front().is_none());
    }

    // this test ensures that 2/3 timers are cancelled
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn partial_timeout_cancellation() {
        let timeout_manager = TimeoutManager::test_default();

        let timeout_tx = timeout_manager.init();

        let id1 = StateId::new_with_u128(TestKind, 1);
        let id2 = StateId::new_with_u128(TestKind, 2); // gets cancelled
        let id3 = StateId::new_with_u128(TestKind, 3); // gets overridden with earlier timeout

        let timeout_duration = Duration::from_millis(5);
        let now = Instant::now();
        let timeout = now + timeout_duration;
        let early_timeout = timeout - Duration::from_millis(2);
        let set_timeout = TimeoutInput {
            id: id1,
            op: Operation::Set(timeout),
        };

        timeout_tx.send(set_timeout.into()).unwrap();
        timeout_tx.send(set_timeout.with_id(id2).into()).unwrap();
        timeout_tx.send(set_timeout.with_id(id3).into()).unwrap();

        //id1 should timeout after 5 milliseconds
        // ...
        // id2 cancellation
        timeout_tx
            .send(set_timeout.with_id(id2).with_op(Operation::Cancel).into())
            .unwrap();
        // id3 should timeout 2 milliseconds earlier than id1
        timeout_tx
            .send(
                set_timeout
                    .with_id(id3)
                    .with_op(Operation::Set(early_timeout))
                    .into(),
            )
            .unwrap();

        tokio::time::sleep(timeout_duration * 3).await;

        let first_timeout = timeout_manager.signal_queue.pop_front().unwrap();
        assert_eq!(id3, first_timeout.id);

        let second_timeout = timeout_manager.signal_queue.pop_front().unwrap();
        assert_eq!(id1, second_timeout.id);

        // ... and id2 should be cancelled
        assert!(timeout_manager.signal_queue.pop_front().is_none());
    }
}
