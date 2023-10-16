/*!
```text
  ╔═════════════════════════════════╗
  ║S => Signal       [ Kind, Input ]║
  ║N => Notification [ Kind, Input ]║                                                    ▲
  ╚═════════════════════════════════╝                                                    │
                              ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓                            │
                              ┃                             ┃                            │
                              ┃                             ┃                          other
                              ┃                             ┃                            │
                 ┌─────S──────┃       TimeoutManager        ◀─────────┐                  │
                 │            ┃                             ┃         │                  │
                 │            ┃                             ┃         │   ┏━━━━━━━━━━━━━━▼━━━━━━━━━━━━━━┓
                 │            ┃                             ┃         N   ┃                             ┃
                 │            ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛         │   ┃                             ┃
                 │                                                    │   ┃    Outlet (External I/O)    ┃
                 ├───────────────────────────S────────────────────────┼───┫[GatewayClient + Service TX +┃
                 │                                                    │   ┃           etc...]           ┃
                 │                    ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─           │   ┃                             ┃
                 │                          Signals &      │          │   ┃                             ┃
  ┏━━━━━━━━━━━━━━▼━━━━━━━━━━━━━━┓     │   Notifications               │   ┗━━━━━━━━━━━━━━▲━━━━━━━━━━━━━━┛
  ┃                             ┃      ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘          │                  │
  ┃                             ┃                           ╔══════════════════╗         │
  ┃                             ┃                           ║                  ║         N
  ┃     StateMachineManager     ◀───┬──┐                    ║   Notification   ║         │
  ┃                             ┃   │  │                    ║      Queue       ║─────────┘
  ┃                             ┃   │  │                    ║                  ║
  ┃                             ┃   │  │                    ╚═════════▲════════╝
  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛   │  │                              │
                 │                  │  │                              N
                 S                  │  │                              │
                 │                  S  S                ┏━━━━━━━━━━━━━┻━━━━━━━━━━━━┓
       ╔═════════▼════════╗         │  │                ┃                          ┃
       ║                  ║         │  │                ┃                          ┃
       ║   Signal Queue   ║─┐       │  │         ┌──┬───▶   NotificationManager    ┃
       ║                  ║ │       │  │         │  │   ┃                          ┃
       ╚══════════════════╝ │       │  │         │  │   ┃                          ┃
                 │          S ┌─────┴──┴─────┐   N  N   ┗━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                 │          │ │              │   │  │
                 │          └─▶ StateMachine │───┼──┘
                 │         ┌──┴───────────┐  │   │
                 │         │              ├──┘   │
                 └────S────▶ StateMachine │──────┘
                           │              │
                           └──────────────┘
```
*/

use bigerror::{LogError, OptionReport};
use std::{collections::HashMap, fmt, hash::Hash, sync::Arc, time::Duration};
use tracing::debug;

use tokio::sync::{
    mpsc::{error::SendError, UnboundedSender},
    Mutex,
};
use tokio_stream::StreamExt;
use tracing::instrument;

use crate::{
    node::{Node, Update},
    queue::StreamableDeque,
    storage::*,
    timeout::TimeoutInput,
    Kind, KindExt, State, StateId,
};

pub trait HashKind: Kind + fmt::Debug + Hash + Eq + PartialEq + 'static + Copy
where
    Self: Send + Sync,
{
}

impl<K> HashKind for K where
    K: Kind + fmt::Debug + Hash + Eq + PartialEq + Send + Sync + 'static + Copy
{
}

/// The [`Signal`] struct represents a routable input meant to be consumed
/// by a state machine processor.
/// The `id` field holds:
/// * The routing logic accessed by the [`Kind`] portion of the id
/// * a distinguishing identifier that separates state of the _same_ kind
///
///
/// The `input` field :
/// * holds an event or message meant to be processed by a given state machine
/// * an event is defined as any output emitted by a state machine
#[derive(Debug, PartialEq)]
pub struct Signal<K, I>
where
    K: Kind,
    I: Send + Sync + fmt::Debug,
{
    pub id: StateId<K>,
    pub input: I,
}

impl<K, I> Signal<K, I>
where
    K: Kind + KindExt<Input = I>,
    I: Send + Sync + fmt::Debug,
{
    fn state_change(id: StateId<K>, state: <K as Kind>::State) -> Option<Self> {
        id.kind.state_input(state).map(|input| Self { id, input })
    }
}

pub type SignalQueue<K, I> = StreamableDeque<Signal<K, I>>;

/// [SignalExt] calls [`Signal::state_change`] to consume a [`Kind::State`] and emit
/// a state change [`Signal`] with a valid [`StateMachine::Input`]
pub trait SignalExt<K, I>
where
    K: Kind + KindExt<Input = I>,
{
    fn signal_state_change(&self, id: StateId<K>, state: <K as Kind>::State);
}

impl<K, I> SignalExt<K, I> for SignalQueue<K, I>
where
    K: Kind + KindExt<Input = I>,
    I: Send + Sync + 'static + fmt::Debug,
{
    fn signal_state_change(&self, id: StateId<K>, state: <K as Kind>::State) {
        if let Some(sig) = Signal::state_change(id, state) {
            self.push_back(sig);
        }
    }
}

/// Store the injectable dependencies provided by the [`StateMachineManager`]
/// to a given state machine processor.
pub struct ProcessorContext<S, I, N, K>
where
    S: State + fmt::Debug,
    I: Send + Sync + 'static + fmt::Debug,
    N: fmt::Debug + Send + Sync,
    K: HashKind,
{
    pub signal_queue: Arc<SignalQueue<K, I>>,
    pub notification_queue: UnboundedSender<N>,
    pub state_store: Arc<NodeStore<StateId<K>, S>>,
}

impl<S, I, N, K> Clone for ProcessorContext<S, I, N, K>
where
    S: State + fmt::Debug,
    I: Send + Sync + 'static + fmt::Debug,
    N: fmt::Debug + Send + Sync,
    K: HashKind,
{
    fn clone(&self) -> Self {
        Self {
            signal_queue: self.signal_queue.clone(),
            notification_queue: self.notification_queue.clone(),
            state_store: self.state_store.clone(),
        }
    }
}

/// Manages the [`Signal`] scope of various [`State`]s and [`StateMachine`]s bounded by
/// a [`Kind`] enumerable
pub struct StateMachineManager<S, I, N, K>
where
    S: State + fmt::Debug,
    I: Send + Sync + fmt::Debug,
    N: fmt::Debug + Send + Sync,
    K: HashKind,
{
    signal_queue: Arc<SignalQueue<K, I>>,
    notification_queue: UnboundedSender<N>,
    state_machines: Arc<HashMap<K, BoxedStateMachine<S, I, N, K>>>,
    state_store: Arc<NodeStore<StateId<K>, S>>,
}

impl<S, I, N, K> StateMachineManager<S, I, N, K>
where
    S: State + fmt::Debug + Send + 'static + Copy,
    I: Send + Sync + 'static + fmt::Debug,
    N: fmt::Debug + Send + Sync + 'static,
    K: HashKind + Kind<State = S>,
{
    pub fn context(&self) -> ProcessorContext<S, I, N, K> {
        ProcessorContext {
            signal_queue: self.signal_queue.clone(),
            notification_queue: self.notification_queue.clone(),
            state_store: self.state_store.clone(),
        }
    }
    // const fn does not currently support Iterable
    pub fn new<const U: usize>(
        state_machines: [BoxedStateMachine<S, I, N, K>; U],
        signal_queue: Arc<SignalQueue<K, I>>,
        notification_queue: UnboundedSender<N>,
    ) -> Self {
        let state_machines: HashMap<K, BoxedStateMachine<S, I, N, K>> = state_machines
            .into_iter()
            .map(|sm| (sm.get_kind(), sm))
            .collect();
        Self {
            signal_queue,
            notification_queue,
            state_machines: Arc::new(state_machines),
            state_store: Arc::new(NodeStore::new()),
        }
    }
    pub fn init(&self) {
        let stream_queue = self.signal_queue.clone();
        let sm_dispatcher = self.state_machines.clone();
        let ctx = self.context();
        tokio::spawn(async move {
            debug!(target:  "state_machine", spawning = "StateMachineManager.signal_queue");
            let mut stream = stream_queue.stream();
            while let Some(signal) = stream.next().await {
                let Signal { id, input } = signal;
                let Ok(sm) = sm_dispatcher
                    .get(&id)
                    .ok_or_not_found_kv("state_machine", id)
                    .and_log_err()
                else {
                    continue;
                };
                sm.process(ctx.clone(), id, input).await;
            }
        });
    }
}

type BoxedStateMachine<S, I, N, K> = Box<dyn StateMachine<K, S, N, Input = I>>;

/// Represents the trait that a state machine must fulfill to process signals
/// A [`StateMachine`] consumes the `input` portion of a [`Signal`] and...
/// * optionally emits [`Signal`]s - consumed by the [`StateMachineManager`] `signal_queue`
/// * optionally emits [`Notification`]s  - consumed by the [`NotificationQueue`]
#[async_trait::async_trait]
pub trait StateMachine<K, S, N>: Send + Sync
where
    K: HashKind + Kind<State = S>,
    S: State + fmt::Debug + 'static + Copy,
    N: fmt::Debug + Send + Sync,
{
    type Input: Send + Sync + fmt::Debug;
    async fn process(
        &self,
        ctx: ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
        input: Self::Input,
    );

    fn get_kind(&self) -> K;

    async fn get_state(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
    ) -> Option<S> {
        let entry = ctx.state_store.get(id)?.clone().lock_owned().await;
        entry.get_state(id).copied()
    }

    async fn get_entry(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
    ) -> Option<OwnedEntry<K, S>> {
        let entry = ctx.state_store.get(id)?;

        Some(entry.clone().lock_owned().await)
    }

    async fn update_state(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
        state: S,
    ) {
        self.get_entry(ctx, id)
            .await
            .unwrap()
            .update(Update { id, state });
    }
}

#[async_trait::async_trait]
pub trait StateMachineExt<K, S, N>: StateMachine<K, S, N>
where
    K: Kind<State = S> + KindExt<Input = Self::Input> + HashKind,
    S: State + fmt::Debug + Send + 'static + Copy,
    N: fmt::Debug + Send + Sync + From<TimeoutInput<K>>,
{
    /// NOTE [`StateMachineExt::new`] is created without a hierarchy
    fn create_entry(&self, ctx: &ProcessorContext<S, Self::Input, N, K>, id: StateId<K>) {
        ctx.state_store
            .insert_entry(id, Arc::new(Mutex::new(Node::new(id))))
    }

    fn has_entry(&self, ctx: &ProcessorContext<S, Self::Input, N, K>, id: StateId<K>) -> bool {
        ctx.state_store.get(id).is_some()
    }

    async fn fail(&self, ctx: &ProcessorContext<S, Self::Input, N, K>, id: StateId<K>) {
        self.update_state_and_signal(ctx, id, id.failed_state())
            .await
    }

    async fn complete(&self, ctx: &ProcessorContext<S, Self::Input, N, K>, id: StateId<K>) {
        self.update_state_and_signal(ctx, id, id.completed_state())
            .await
    }

    /// represents a state that should ignore
    /// all input
    fn invalid_state(state: S) -> bool {
        state.is_failed() || state.is_completed()
    }

    /// update state is meant to be used to signal a parent state of a child state
    /// _if_ a parent exists, this function makes no assumptions of the potential
    /// structure of a state hierarchy and _should_ be just as performant on a single
    /// state tree as it is for multiple states
    async fn update_state_and_signal(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
        state: S,
    ) {
        let Some(mut entry) = self.get_entry(ctx, id).await else {
            // TODO propagate error
            tracing::error!(?id, "Entry not found!");
            panic!("missing entry");
        };

        if let Some(id) = entry.update_and_get_parent_id(Update { id, state }) {
            ctx.signal_queue.signal_state_change(id, state);
        }
    }

    #[instrument(skip_all, fields(?notification))]
    fn notify(&self, ctx: &ProcessorContext<S, Self::Input, N, K>, notification: N) {
        ctx.notification_queue.send(notification).log_err();
    }

    fn set_timeout(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
        duration: Duration,
    ) -> Result<(), SendError<N>> {
        ctx.notification_queue
            .send(TimeoutInput::set_timeout(id, duration).into())
    }

    fn set_timeout_millis(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
        millis: u64,
    ) -> Result<(), SendError<N>> {
        ctx.notification_queue
            .send(TimeoutInput::set_timeout_millis(id, millis).into())
    }

    fn cancel_timeout(
        &self,
        ctx: &ProcessorContext<S, Self::Input, N, K>,
        id: StateId<K>,
    ) -> Result<(), SendError<N>> {
        ctx.notification_queue
            .send(TimeoutInput::cancel_timeout(id).into())
    }
}

impl<K, S, N, T> StateMachineExt<K, S, N> for T
where
    T: StateMachine<K, S, N>,
    K: Kind<State = S> + KindExt<Input = Self::Input> + HashKind,
    S: State + fmt::Debug + Send + 'static + Copy,
    N: fmt::Debug + Send + Sync + From<TimeoutInput<K>>,
{
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::{
        node::{Insert, Node},
        notification::{self, NotificationManager},
        storage::NodeStore,
        test_support::TestMsg,
        timeout::{TimeoutManager, TEST_TICK_RATE, TEST_TIMEOUT},
        KindExt,
    };

    use dashmap::DashMap;
    use tokio::{sync::mpsc, time::Instant};
    use tracing::*;

    type NotificationInput = notification::Notification<ComponentKind, TestMsg>;

    #[derive(Clone, Debug)]
    pub struct Packet {
        msg: u64,
        sender: StateId<ComponentKind>,
        who_sleeps: WhoSleeps,
    }

    #[derive(Clone, Debug)]
    pub enum Input {
        Ping(PingInput),
        Pong(PongInput),
        Menu(MenuInput),
    }

    // determines whether Ping or Pong will await before packet send
    //
    #[derive(Clone, PartialEq, Debug)]
    pub struct WhoSleeps(Option<ComponentKind>);

    #[derive(Clone, PartialEq, Debug)]
    pub enum MenuInput {
        Play(WhoSleeps),
        PingPongComplete,
        FailedPing,
        FailedPong,
    }

    #[derive(Copy, Clone, PartialEq, Default, Debug)]
    pub enum MenuState {
        #[default]
        Ready,
        Done,
        Failed,
    }

    #[derive(Copy, Clone, PartialEq, Default, Debug)]
    pub enum PingState {
        #[default]
        Ready,
        Sending,
        Done,
        Failed,
    }

    #[derive(Clone, Debug)]
    pub enum PingInput {
        StartSending(StateId<ComponentKind>, WhoSleeps),
        Packet(Packet),
        RecvTimeout(Instant),
    }

    #[derive(Copy, Clone, PartialEq, Default, Debug)]
    pub enum PongState {
        #[default]
        Ready,
        Responding,
        Done,
        Failed,
    }

    #[derive(Clone, Debug)]
    pub enum PongInput {
        Packet(Packet),
        RecvTimeout(Instant),
    }

    #[derive(Copy, Clone, PartialEq, Debug)]
    pub enum ComponentState {
        Ping(PingState),
        Pong(PongState),
        Menu(MenuState),
    }

    impl State for ComponentState {
        fn get_kind(&self) -> &dyn Kind<State = Self> {
            match self {
                ComponentState::Ping(_) => &ComponentKind::Ping,
                ComponentState::Pong(_) => &ComponentKind::Pong,
                ComponentState::Menu(_) => &ComponentKind::Menu,
            }
        }
    }

    #[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
    pub enum ComponentKind {
        Ping,
        Pong,
        Menu,
    }

    impl KindExt for ComponentKind {
        type Input = Input;

        fn state_input(&self, state: <Self as Kind>::State) -> Option<Self::Input> {
            if *self != ComponentKind::Menu {
                return None;
            }

            match state {
                ComponentState::Ping(PingState::Done) => {
                    Some(Input::Menu(MenuInput::PingPongComplete))
                }
                ComponentState::Ping(PingState::Failed) => Some(Input::Menu(MenuInput::FailedPing)),
                ComponentState::Pong(PongState::Failed) => Some(Input::Menu(MenuInput::FailedPong)),
                _ => None,
            }
        }

        fn timeout_input(&self, instant: tokio::time::Instant) -> Option<Self::Input> {
            match self {
                ComponentKind::Ping => Some(Input::Ping(PingInput::RecvTimeout(instant))),
                ComponentKind::Pong => Some(Input::Pong(PongInput::RecvTimeout(instant))),
                ComponentKind::Menu => None,
            }
        }
    }

    impl Kind for ComponentKind {
        type State = ComponentState;

        fn new_state(&self) -> Self::State {
            match self {
                ComponentKind::Ping => ComponentState::Ping(PingState::default()),
                ComponentKind::Pong => ComponentState::Pong(PongState::default()),
                ComponentKind::Menu => ComponentState::Menu(MenuState::default()),
            }
        }

        fn failed_state(&self) -> Self::State {
            match self {
                ComponentKind::Ping => ComponentState::Ping(PingState::Failed),
                ComponentKind::Pong => ComponentState::Pong(PongState::Failed),
                ComponentKind::Menu => ComponentState::Menu(MenuState::Failed),
            }
        }

        fn completed_state(&self) -> Self::State {
            match self {
                ComponentKind::Ping => ComponentState::Ping(PingState::Done),
                ComponentKind::Pong => ComponentState::Pong(PongState::Done),
                ComponentKind::Menu => ComponentState::Menu(MenuState::Done),
            }
        }
    }

    struct MenuStateMachine {
        failures: Arc<DashMap<StateId<ComponentKind>, MenuInput>>,
    }

    #[async_trait::async_trait]
    impl StateMachine<ComponentKind, ComponentState, NotificationInput> for MenuStateMachine {
        type Input = Input;
        async fn process(
            &self,
            ctx: ProcessorContext<ComponentState, Self::Input, NotificationInput, ComponentKind>,
            id: StateId<ComponentKind>,
            input: Self::Input,
        ) {
            let Input::Menu(input) = input else {
                error!(input = ?input, "invalid input!");
                return;
            };

            let state = self.get_state(&ctx, id).await;
            if let Some(true) = state.map(Self::invalid_state) {
                warn!(?id, ?state, "Ignoring input due to invalid state");
                return;
            }

            match input {
                MenuInput::Play(who_sleeps) => {
                    let ping_id = StateId::new_rand(ComponentKind::Ping);
                    let pong_id = StateId::new_rand(ComponentKind::Pong);
                    // Menu + Ping + Pong
                    let menu_tree = Node::new(id)
                        .into_insert(Insert {
                            parent_id: Some(id),
                            id: ping_id,
                        })
                        .into_insert(Insert {
                            parent_id: Some(id),
                            id: pong_id,
                        });

                    let entry = NodeStore::new_entry(menu_tree);
                    for id in [id, ping_id, pong_id] {
                        ctx.state_store.insert_entry(id, entry.clone());
                    }

                    // signal to Ping state machine
                    ctx.signal_queue.push_back(Signal {
                        id: ping_id,
                        input: Input::Ping(PingInput::StartSending(pong_id, who_sleeps)),
                    });
                }
                MenuInput::PingPongComplete => {
                    info!("I'M DONE!");
                    self.complete(&ctx, id).await
                }
                failure @ (MenuInput::FailedPing | MenuInput::FailedPong) => {
                    let mut entry = self.get_entry(&ctx, id).await.unwrap();
                    // set all states to failed state
                    entry.update_all_fn(|mut z| {
                        z.node.state.fail();
                        self.cancel_timeout(&ctx, z.node.id).unwrap(); // cancel all other timeouts
                        z.finish_update()
                    });

                    error!(input = ?failure, "Encountered Failure!");
                    self.failures.insert(id, failure);
                }
            }
        }

        fn get_kind(&self) -> ComponentKind {
            ComponentKind::Menu
        }
    }

    impl MenuStateMachine {
        fn new() -> Self {
            Self {
                failures: Arc::new(DashMap::new()),
            }
        }
    }

    struct PingStateMachine;

    #[async_trait::async_trait]
    impl StateMachine<ComponentKind, ComponentState, NotificationInput> for PingStateMachine {
        type Input = Input;
        async fn process(
            &self,
            ctx: ProcessorContext<ComponentState, Self::Input, NotificationInput, ComponentKind>,
            id: StateId<ComponentKind>,
            input: Self::Input,
        ) {
            let Input::Ping(input) = input else {
                error!(?input, "invalid input!");
                return;
            };
            let state = self.get_state(&ctx, id).await.unwrap();
            if Self::invalid_state(state) {
                warn!(?id, ?state, "Ignoring input due to invalid state");
                return;
            }

            match input {
                PingInput::StartSending(pong_id, who_sleeps) => {
                    self.update_state(&ctx, id, ComponentState::Ping(PingState::Sending))
                        .await;
                    info!(msg = 0, "PINGING");
                    ctx.signal_queue.push_back(Signal {
                        id: pong_id,
                        input: Input::Pong(PongInput::Packet(Packet {
                            msg: 0,
                            sender: id,
                            who_sleeps,
                        })),
                    });
                    // TODO let timeout = now + Duration::from_millis(250);
                }
                PingInput::Packet(Packet { msg: 25.., .. }) => {
                    info!("PING Complete!");
                    self.complete(&ctx, id).await;
                    self.cancel_timeout(&ctx, id).unwrap();
                }

                PingInput::Packet(Packet {
                    mut msg,
                    sender,
                    who_sleeps,
                }) => {
                    self.set_timeout(&ctx, id, TEST_TIMEOUT).unwrap();
                    msg += 5;

                    // sleep after resetting own timeout
                    if let WhoSleeps(Some(ComponentKind::Ping)) = who_sleeps {
                        info!(?msg, who = ?self.get_kind(), "SLEEPING");
                        // refresh timeout halfway through sleep
                        let half_sleep = Duration::from_millis(msg) / 2;
                        tokio::time::sleep(half_sleep).await;
                        self.set_timeout(&ctx, id, TEST_TIMEOUT).unwrap();
                        tokio::time::sleep(half_sleep).await;
                    }

                    info!(?msg, "PINGING");
                    ctx.signal_queue.push_back(Signal {
                        id: sender,
                        input: Input::Pong(PongInput::Packet(Packet {
                            msg,
                            sender: id,
                            who_sleeps,
                        })),
                    });
                }
                PingInput::RecvTimeout(_) => self.fail(&ctx, id).await,
            }
        }

        fn get_kind(&self) -> ComponentKind {
            ComponentKind::Ping
        }
    }

    struct PongStateMachine;

    #[async_trait::async_trait]
    impl StateMachine<ComponentKind, ComponentState, NotificationInput> for PongStateMachine {
        type Input = Input;
        async fn process(
            &self,
            ctx: ProcessorContext<ComponentState, Self::Input, NotificationInput, ComponentKind>,
            id: StateId<ComponentKind>,
            input: Self::Input,
        ) {
            let Input::Pong(input) = input else {
                error!(?input, "invalid input!");
                return;
            };
            let state = self.get_state(&ctx, id).await.unwrap();
            if Self::invalid_state(state) {
                warn!(?id, ?state, "Ignoring input due to invalid state");
                return;
            }

            match input {
                PongInput::Packet(Packet {
                    // https://doc.rust-lang.org/book/ch18-03-pattern-syntax.html#-bindings
                    msg: mut msg @ 20..,
                    sender,
                    who_sleeps,
                }) => {
                    msg += 5;
                    info!(?msg, "PONGING");
                    self.complete(&ctx, id).await;
                    self.cancel_timeout(&ctx, id).unwrap();
                    ctx.signal_queue.push_back(Signal {
                        id: sender,
                        input: Input::Ping(PingInput::Packet(Packet {
                            msg,
                            sender: id,
                            who_sleeps,
                        })),
                    });
                }
                PongInput::Packet(Packet {
                    mut msg,
                    sender,
                    who_sleeps,
                }) => {
                    if msg == 0 {
                        self.update_state(&ctx, id, ComponentState::Pong(PongState::Responding))
                            .await
                    }
                    msg += 5;

                    if let WhoSleeps(Some(ComponentKind::Pong)) = who_sleeps {
                        info!(?msg, who = ?self.get_kind(), "SLEEPING");
                        // refresh timeout halfway through sleep
                        let half_sleep = Duration::from_millis(msg) / 2;
                        tokio::time::sleep(half_sleep).await;
                        self.set_timeout(&ctx, id, TEST_TIMEOUT).unwrap();
                        tokio::time::sleep(half_sleep).await;
                    }

                    self.set_timeout(&ctx, id, TEST_TIMEOUT).unwrap();
                    info!(?msg, "PONGING");
                    ctx.signal_queue.push_back(Signal {
                        id: sender,
                        input: Input::Ping(PingInput::Packet(Packet {
                            msg,
                            sender: id,
                            who_sleeps,
                        })),
                    });
                }
                PongInput::RecvTimeout(_) => self.fail(&ctx, id).await,
            }
        }
        fn get_kind(&self) -> ComponentKind {
            ComponentKind::Pong
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn state_machine() {
        // This test does not initialize the NotificationManager
        let (notification_tx, _notification_rx) = mpsc::unbounded_channel();

        let signal_queue = Arc::new(StreamableDeque::new());
        let manager = StateMachineManager::new(
            [
                Box::new(MenuStateMachine::new()),
                Box::new(PingStateMachine),
                Box::new(PongStateMachine),
            ],
            signal_queue,
            notification_tx,
        );
        manager.init();

        let menu_id = StateId::new_rand(ComponentKind::Menu);
        manager.signal_queue.push_back(Signal {
            id: menu_id,
            input: Input::Menu(MenuInput::Play(WhoSleeps(None))),
        });
        tokio::time::sleep(Duration::from_millis(1)).await;

        let entry = manager.state_store.get(menu_id).unwrap();
        let node = entry.lock_owned().await;
        let ping_id = node.children[0].id;
        let pong_id = node.children[1].id;
        assert_eq!(menu_id, node.id);
        assert_eq!(ComponentState::Menu(MenuState::Done), node.state);

        // !!NOTE!! ============================================================
        // we are trying to acquire another lock...
        // * from the SAME Mutex
        // * in the SAME thread
        // this WILL deadlock unless the previous lock is dropped
        drop(node);
        // !!NOTE!! ============================================================

        // ensure MenuState is also indexed by ping id
        let entry = manager.state_store.get(ping_id).unwrap();
        let node = entry.lock_owned().await;
        let state = node.get_state(ping_id).unwrap();
        assert_eq!(ComponentState::Ping(PingState::Done), *state);

        drop(node);

        let entry = manager.state_store.get(pong_id).unwrap();
        let node = entry.lock_owned().await;
        let state = node.get_state(pong_id).unwrap();
        assert_eq!(ComponentState::Pong(PongState::Done), *state);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn state_machine_timeout() {
        let signal_queue = Arc::new(StreamableDeque::new());
        let notification_manager =
            NotificationManager::new([
                &TimeoutManager::new(signal_queue.clone()).with_tick_rate(TEST_TICK_RATE / 2)
            ]);

        let menu_sm = MenuStateMachine::new();
        let menu_failures = menu_sm.failures.clone();
        let manager = StateMachineManager::new(
            [
                Box::new(menu_sm),
                Box::new(PingStateMachine),
                Box::new(PongStateMachine),
            ],
            signal_queue,
            notification_manager.init(),
        );

        manager.init();

        let menu_id = StateId::new_rand(ComponentKind::Menu);
        manager.signal_queue.push_back(Signal {
            id: menu_id,
            input: Input::Menu(MenuInput::Play(WhoSleeps(Some(ComponentKind::Ping)))),
        });

        tokio::time::sleep(TEST_TIMEOUT * 4).await;

        let entry = manager.state_store.get(menu_id).unwrap();
        let node = entry.lock_owned().await;
        let ping_node = &node.children[0];
        let pong_node = &node.children[1];
        assert_eq!(menu_id, node.id);
        assert_eq!(ComponentState::Menu(MenuState::Failed), node.state);
        assert_eq!(ComponentState::Ping(PingState::Failed), ping_node.state);
        assert_eq!(ComponentState::Pong(PongState::Failed), pong_node.state);

        // !!NOTE!! ============================================================
        // we are trying to acquire another lock...
        // * from the SAME Mutex
        // * in the SAME thread
        // this WILL deadlock unless the previous lock is dropped
        drop(node);
        // !!NOTE!! ============================================================

        // Ensure that our Menu failed due to Pong timing out since
        // Ping "slept on" the packet
        assert_eq!(MenuInput::FailedPong, *menu_failures.get(&menu_id).unwrap());

        // Now fail due to Ping
        let menu_id = StateId::new_rand(ComponentKind::Menu);
        manager.signal_queue.push_back(Signal {
            id: menu_id,
            input: Input::Menu(MenuInput::Play(WhoSleeps(Some(ComponentKind::Pong)))),
        });

        tokio::time::sleep(TEST_TIMEOUT * 4).await;

        let entry = manager.state_store.get(menu_id).unwrap();
        let node = entry.lock_owned().await;
        let ping_node = &node.children[0];
        let pong_node = &node.children[1];
        assert_eq!(menu_id, node.id);
        assert_eq!(ComponentState::Menu(MenuState::Failed), node.state);
        assert_eq!(ComponentState::Ping(PingState::Failed), ping_node.state);
        assert_eq!(ComponentState::Pong(PongState::Failed), pong_node.state);
        // Ensure that our Menu failed due to Ping
        assert_eq!(MenuInput::FailedPing, *menu_failures.get(&menu_id).unwrap());
    }
}
