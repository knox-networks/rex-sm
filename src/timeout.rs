#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use bigerror::attachment::DisplayDuration;
use parking_lot::Mutex;
use tokio::{
    sync::{mpsc, mpsc::UnboundedSender},
    task::JoinSet,
    time::Instant,
};
use tracing::{debug, error, instrument, warn, Instrument};

use crate::{
    manager::{HashKind, Signal, SignalQueue},
    notification,
    notification::{Notification, NotificationProcessor, RexMessage, UnaryRequest},
    Kind, Rex, StateId,
};

pub trait TimeoutMessage<K: Rex>: RexMessage + From<UnaryRequest<K, Self::Op>> {
    type Op: notification::Operation;
}

pub const DEFAULT_TICK_RATE: Duration = Duration::from_millis(5);
const SHORT_TIMEOUT: Duration = Duration::from_secs(10);

/// convert a [`Duration`] into a "0H00m00s" string
fn hms_string(duration: Duration) -> String {
    if duration.is_zero() {
        return "ZERO".to_string();
    }
    let s = duration.as_secs();
    let ms = duration.subsec_millis();
    // if only milliseconds available
    if s == 0 {
        return format!("{ms}ms");
    }
    // Grab total hours from seconds
    let (h, s) = (s / 3600, s % 3600);
    let (m, s) = (s / 60, s % 60);

    let mut hms = String::new();
    if h != 0 {
        hms += &format!("{h:02}H");
    }
    if m != 0 {
        hms += &format!("{m:02}m");
    }
    hms += &format!("{s:02}s");

    hms
}

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
    #[instrument(skip_all, fields(%id))]
    fn set_timeout(&mut self, id: StateId<K>, instant: Instant) {
        let now = Instant::now();
        if instant < now {
            error!("requested timeout is in the past");
        }
        let duration = instant - now;
        if duration <= SHORT_TIMEOUT {
            warn!(duration = %DisplayDuration(instant - now), "setting short timeout");
        } else {
            debug!(duration = %DisplayDuration(instant - now), "setting timeout");
        }

        if let Some(old_instant) = self.ids.insert(id, instant) {
            // remove older reference to id
            // if instants differ
            if old_instant != instant {
                debug!(%id, "renewing timeout");
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
                debug!(%id, "cancelled timeout");
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Operation {
    Cancel,
    Set(Instant),
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = match self {
            Operation::Cancel => "timeout::Cancel",
            Operation::Set(_) => "timeout::Set",
        };
        write!(f, "{op}")
    }
}

impl Operation {
    pub fn from_duration(duration: Duration) -> Self {
        Self::Set(Instant::now() + duration)
    }

    pub fn from_millis(millis: u64) -> Self {
        Self::Set(Instant::now() + Duration::from_millis(millis))
    }
}

pub type TimeoutInput<K> = UnaryRequest<K, Operation>;

impl<K> UnaryRequest<K, Operation>
where
    K: Rex,
    K::Message: From<UnaryRequest<K, Operation>>,
{
    #[cfg(test)]
    pub(crate) fn set_timeout_millis(id: StateId<K>, millis: u64) -> Self {
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

/// Processes incoming [`Operation`]s and modifies the [`TimeoutLedger`]
/// through a polling loop.
pub struct TimeoutManager<K>
where
    K: Rex,
{
    // the interval at which  the TimeoutLedger checks for timeouts
    tick_rate: Duration,
    ledger: Arc<Mutex<TimeoutLedger<K>>>,
    topic: <K::Message as RexMessage>::Topic,

    pub(crate) signal_queue: SignalQueue<K>,
}

impl<K> TimeoutManager<K>
where
    K: Rex,
    K::Message: TryInto<TimeoutInput<K>>,
    <K::Message as TryInto<TimeoutInput<K>>>::Error: Send,
{
    pub fn new(
        signal_queue: SignalQueue<K>,
        topic: impl Into<<K::Message as RexMessage>::Topic>,
    ) -> Self {
        Self {
            tick_rate: DEFAULT_TICK_RATE,
            signal_queue,
            ledger: Arc::new(Mutex::new(TimeoutLedger::new())),
            topic: topic.into(),
        }
    }

    pub fn with_tick_rate(self, tick_rate: Duration) -> Self {
        Self { tick_rate, ..self }
    }

    pub fn init_inner(&self) -> UnboundedSender<Notification<K::Message>> {
        let mut join_set = JoinSet::new();
        let tx = self.init_inner_with_handle(&mut join_set);
        join_set.detach_all();
        tx
    }

    pub fn init_inner_with_handle(
        &self,
        join_set: &mut JoinSet<()>,
    ) -> UnboundedSender<Notification<K::Message>> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Notification<K::Message>>();
        let in_ledger = self.ledger.clone();

        join_set.spawn(
            async move {
                debug!(target: "state_machine", spawning = "TimeoutManager.notification_tx");
                while let Some(Notification(msg)) = input_rx.recv().await {
                    match msg.try_into() {
                        Ok(UnaryRequest { id, op }) => {
                            let mut ledger = in_ledger.lock();
                            match op {
                                Operation::Cancel => {
                                    ledger.cancel_timeout(id);
                                }
                                Operation::Set(instant) => {
                                    ledger.set_timeout(id, instant);
                                }
                            }
                        }
                        Err(_e) => {
                            warn!("Invalid input");
                            continue;
                        }
                    }
                    /*
                    let Notification::Timeout(TimeoutInput { id, op }) = notification else {
                        warn!("Invalid input");
                        continue;
                    };
                    let mut ledger = in_ledger.lock();
                    match op {
                        Operation::Cancel => {
                            ledger.cancel_timeout(id);
                        }
                        Operation::Set(instant) => {
                            ledger.set_timeout(id, instant);
                        }
                    }
                    */
                }
            }
            .in_current_span(),
        );

        let timer_ledger = self.ledger.clone();
        let mut interval = tokio::time::interval(self.tick_rate);
        let signal_queue = self.signal_queue.clone();
        join_set.spawn(
            async move {
                loop {
                    interval.tick().await;

                    let now = Instant::now();
                    let mut ledger = timer_ledger.lock();
                    // Get all instants where `instant <= now`
                    let expired: Vec<Instant> =
                        ledger.timers.range(..=now).map(|(k, _)| *k).collect();

                    for id in expired
                        .iter()
                        .filter_map(|t| ledger.timers.remove(t))
                        .flat_map(|set| set.into_iter())
                        .collect::<Vec<_>>()
                    {
                        warn!(%id, "timed out");
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
            }
            .in_current_span(),
        );

        input_tx
    }
}

impl<K> NotificationProcessor<K::Message> for TimeoutManager<K>
where
    K: Rex,
    K::Message: TryInto<TimeoutInput<K>>,
    <K::Message as TryInto<TimeoutInput<K>>>::Error: Send,
{
    fn init(&mut self, join_set: &mut JoinSet<()>) -> UnboundedSender<Notification<K::Message>> {
        self.init_inner_with_handle(join_set)
    }

    fn get_topics(&self) -> &[<K::Message as RexMessage>::Topic] {
        std::slice::from_ref(&self.topic)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct TimeoutTopic;

#[cfg(test)]
pub(crate) const TEST_TICK_RATE: Duration = Duration::from_millis(3);

#[cfg(test)]
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_millis(11);

#[cfg(test)]
mod tests {

    use super::*;
    use crate::test_support::*;

    impl TestDefault for TimeoutManager<TestKind> {
        fn test_default() -> Self {
            let signal_queue = SignalQueue::default();
            TimeoutManager::new(signal_queue, TestTopic::Timeout).with_tick_rate(TEST_TICK_RATE)
        }
    }

    #[tokio::test]
    async fn timeout_to_signal() {
        let mut timeout_manager = TimeoutManager::test_default();

        let mut join_set = JoinSet::new();
        let timeout_tx: UnboundedSender<Notification<TestMsg>> =
            timeout_manager.init(&mut join_set);

        let test_id = StateId::new_rand(TestKind);
        let timeout_duration = Duration::from_millis(5);

        let timeout = Instant::now() + timeout_duration;
        let set_timeout = UnaryRequest::set_timeout(test_id, timeout_duration);

        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(set_timeout)))
            .unwrap();

        // ensure two ticks have passed
        tokio::time::sleep(timeout_duration * 3).await;

        let Signal { id, input } = timeout_manager.signal_queue.pop_front().unwrap();
        assert_eq!(test_id, id);

        let TestInput::Timeout(signal_timeout) = input else {
            panic!("{input:?}");
        };
        assert!(
            signal_timeout >= timeout,
            "out[{signal_timeout:?}] >= in[{timeout:?}]"
        );
    }

    #[tokio::test]
    async fn timeout_cancellation() {
        let mut timeout_manager = TimeoutManager::test_default();

        let mut join_set = JoinSet::new();
        let timeout_tx: UnboundedSender<Notification<TestMsg>> =
            timeout_manager.init(&mut join_set);

        let test_id = StateId::new_rand(TestKind);
        let set_timeout = UnaryRequest::set_timeout_millis(test_id, 10);

        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(set_timeout)))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(2)).await;
        let cancel_timeout = UnaryRequest {
            id: test_id,
            op: Operation::Cancel,
        };
        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(cancel_timeout)))
            .unwrap();

        // wait out the rest of the duration and 3 ticks
        tokio::time::sleep(Duration::from_millis(3) + TEST_TICK_RATE * 2).await;

        // we should not be getting any signal since the timeout was cancelled
        assert!(timeout_manager.signal_queue.pop_front().is_none());
    }

    // this test ensures that 2/3 timers are cancelled
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn partial_timeout_cancellation() {
        let mut timeout_manager = TimeoutManager::test_default();

        let mut join_set = JoinSet::new();
        let timeout_tx: UnboundedSender<Notification<TestMsg>> =
            timeout_manager.init(&mut join_set);

        let id1 = StateId::new_with_u128(TestKind, 1);
        let id2 = StateId::new_with_u128(TestKind, 2); // gets cancelled
        let id3 = StateId::new_with_u128(TestKind, 3); // gets overridden with earlier timeout

        let timeout_duration = Duration::from_millis(5);
        let now = Instant::now();
        let timeout = now + timeout_duration;
        let early_timeout = timeout - Duration::from_millis(2);
        let set_timeout = UnaryRequest {
            id: id1,
            op: Operation::Set(timeout),
        };

        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(set_timeout)))
            .unwrap();
        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(
                set_timeout.with_id(id2),
            )))
            .unwrap();
        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(
                set_timeout.with_id(id3),
            )))
            .unwrap();

        //id1 should timeout after 5 milliseconds
        // ...
        // id2 cancellation
        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(
                set_timeout.with_id(id2).with_op(Operation::Cancel),
            )))
            .unwrap();
        // id3 should timeout 2 milliseconds earlier than id1
        timeout_tx
            .send(Notification(TestMsg::TimeoutInput(
                set_timeout
                    .with_id(id3)
                    .with_op(Operation::Set(early_timeout)),
            )))
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
