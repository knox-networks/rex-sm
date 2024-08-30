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

use std::{collections::HashMap, fmt, hash::Hash, sync::Arc, time::Duration};

use bigerror::{LogError, OptionReport};
use parking_lot::FairMutex;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tracing::{debug, Instrument};

use crate::{
    node::{Insert, Node, Update},
    notification::{Notification, NotificationQueue},
    queue::StreamableDeque,
    storage::{StateStore, Tree},
    timeout::{Retain, RetainItem, TimeoutInput},
    Kind, Rex, State, StateId,
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
pub struct Signal<K>
where
    K: Rex,
{
    pub id: StateId<K>,
    pub input: K::Input,
}

impl<K> Signal<K>
where
    K: Rex,
{
    fn state_change(id: StateId<K>, state: K::State) -> Option<Self> {
        id.kind.state_input(state).map(|input| Self { id, input })
    }
}

pub type SignalQueue<K> = Arc<StreamableDeque<Signal<K>>>;

/// [`SignalExt`] calls [`Signal::state_change`] to consume a [`Kind::State`] and emit
/// a state change [`Signal`] with a valid [`StateMachine::Input`]
pub trait SignalExt<K>
where
    K: Rex,
{
    fn signal_state_change(&self, id: StateId<K>, state: K::State);
}

impl<K> SignalExt<K> for StreamableDeque<Signal<K>>
where
    K: Rex,
{
    fn signal_state_change(&self, id: StateId<K>, state: K::State) {
        if let Some(sig) = Signal::state_change(id, state) {
            self.push_back(sig);
        }
    }
}

/// Store the injectable dependencies provided by the [`StateMachineManager`]
/// to a given state machine processor.
pub struct SmContext<K>
where
    K: Rex,
{
    pub signal_queue: SignalQueue<K>,
    pub notification_queue: NotificationQueue<K::Message>,
    pub state_store: Arc<StateStore<StateId<K>, K::State>>,
}

impl<K> SmContext<K>
where
    K: Rex,
{
    pub fn notify(&self, notification: Notification<K::Message>) {
        self.notification_queue.send(notification);
    }

    pub fn get_state(&self, id: StateId<K>) -> Option<K::State> {
        let tree = self.state_store.get_tree(id)?;
        let guard = tree.lock();
        guard.get_state(id).copied()
    }
}
impl<K> Clone for SmContext<K>
where
    K: Rex,
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
pub struct StateMachineManager<K>
where
    K: Rex,
{
    signal_queue: SignalQueue<K>,
    notification_queue: NotificationQueue<K::Message>,
    state_machines: Arc<HashMap<K, BoxedStateMachine<K>>>,
    state_store: Arc<StateStore<StateId<K>, K::State>>,
}

impl<K> StateMachineManager<K>
where
    K: Rex,
{
    #[must_use]
    pub fn context(&self) -> SmContext<K> {
        SmContext {
            signal_queue: self.signal_queue.clone(),
            notification_queue: self.notification_queue.clone(),
            state_store: self.state_store.clone(),
        }
    }

    // const fn does not currently support Iterable
    pub fn new(
        state_machines: Vec<BoxedStateMachine<K>>,
        signal_queue: SignalQueue<K>,
        notification_queue: NotificationQueue<K::Message>,
    ) -> Self {
        let sm_count = state_machines.len();
        let state_machines: HashMap<K, BoxedStateMachine<K>> = state_machines
            .into_iter()
            .map(|sm| (sm.get_kind(), sm))
            .collect();
        assert_eq!(
            sm_count,
            state_machines.len(),
            "multiple state machines using the same kind, SMs: {sm_count}, Kinds: {}",
            state_machines.len(),
        );
        Self {
            signal_queue,
            notification_queue,
            state_machines: Arc::new(state_machines),
            state_store: Arc::new(StateStore::new()),
        }
    }

    pub fn init(&self, join_set: &mut JoinSet<()>) {
        let stream_queue = self.signal_queue.clone();
        let sm_dispatcher = self.state_machines.clone();
        let ctx = self.context();
        join_set.spawn(
            async move {
                debug!(target:  "state_machine", spawning = "StateMachineManager.signal_queue");
                let mut stream = stream_queue.stream();
                while let Some(Signal { id, input }) = stream.next().await {
                    let Ok(sm) = sm_dispatcher
                        .get(&id)
                        .expect_kv("state_machine", id)
                        .and_log_err()
                    else {
                        continue;
                    };
                    sm.process(ctx.clone(), id, input);
                }
            }
            .in_current_span(),
        );
    }
}

pub(crate) type BoxedStateMachine<K> = Box<dyn StateMachine<K>>;

/// Represents the trait that a state machine must fulfill to process signals
/// A [`StateMachine`] consumes the `input` portion of a [`Signal`] and...
/// * optionally emits [`Signal`]s - consumed by the [`StateMachineManager`] `signal_queue`
/// * optionally emits [`Notification`]s  - consumed by the [`NotificationQueue`]
pub trait StateMachine<K>: Send + Sync
where
    K: Rex,
{
    fn process(&self, ctx: SmContext<K>, id: StateId<K>, input: K::Input);

    fn get_kind(&self) -> K;

    fn get_state(&self, ctx: &SmContext<K>, id: StateId<K>) -> Option<K::State> {
        let tree = ctx.state_store.get_tree(id)?;
        let guard = tree.lock();
        guard.get_state(id).copied()
    }

    fn get_tree(&self, ctx: &SmContext<K>, id: StateId<K>) -> Option<Tree<K>> {
        ctx.state_store.get_tree(id)
    }

    fn new_child(&self, ctx: &SmContext<K>, id: StateId<K>, parent_id: StateId<K>) {
        let tree = ctx.state_store.get_tree(parent_id).unwrap();
        ctx.state_store.insert_ref(id, tree.clone());
        let mut tree = tree.lock();
        tree.insert(Insert {
            parent_id: Some(parent_id),
            id,
        });
    }

    /// Panic: will panic if passed in an id without a previously stored state
    fn update(&self, ctx: &SmContext<K>, id: StateId<K>, state: K::State) {
        let tree = ctx.state_store.get_tree(id).expect("missing id for update");
        let mut guard = tree.lock();

        guard.update(Update { id, state });
    }
}

pub trait StateMachineExt<K>: StateMachine<K>
where
    K: Rex,
    K::Message: Retain<K>,
{
    /// NOTE [`StateMachineExt::new`] is created without a hierarchy
    fn create_tree(&self, ctx: &SmContext<K>, id: StateId<K>) {
        ctx.state_store
            .insert_ref(id, Arc::new(FairMutex::new(Node::new(id))));
    }

    fn has_state(&self, ctx: &SmContext<K>, id: StateId<K>) -> bool {
        ctx.state_store.get_tree(id).is_some()
    }

    fn fail(&self, ctx: &SmContext<K>, id: StateId<K>) -> Option<StateId<K>> {
        self.update_state_and_signal(ctx, id, id.failed_state())
    }

    fn complete(&self, ctx: &SmContext<K>, id: StateId<K>) -> Option<StateId<K>> {
        self.update_state_and_signal(ctx, id, id.completed_state())
    }

    /// represents a state that will no longer change
    fn terminal_state(state: K::State) -> bool {
        state.is_failed() || state.is_completed()
    }

    /// update state is meant to be used to signal a parent state of a child state
    /// _if_ a parent exists, this function makes no assumptions of the potential
    /// structure of a state hierarchy and _should_ be just as performant on a single
    /// state tree as it is for multiple states.
    /// Returns the parent's [`StateId`] if there was one.
    fn update_state_and_signal(
        &self,
        ctx: &SmContext<K>,
        id: StateId<K>,
        state: K::State,
    ) -> Option<StateId<K>> {
        let Some(tree) = self.get_tree(ctx, id) else {
            // TODO propagate error
            tracing::error!(%id, "Tree not found!");
            panic!("missing SmTree");
        };
        let mut guard = tree.lock();

        if let Some(id) = guard.update_and_get_parent_id(Update { id, state }) {
            ctx.signal_queue.signal_state_change(id, state);

            return Some(id);
        }

        None
    }

    fn notify(&self, ctx: &SmContext<K>, msg: impl Into<K::Message>) {
        ctx.notify(Notification(msg.into()));
    }

    fn set_timeout(&self, ctx: &SmContext<K>, id: StateId<K>, duration: Duration) {
        ctx.notification_queue
            .priority_send(Notification(TimeoutInput::set_timeout(id, duration).into()));
    }

    fn return_in(
        &self,
        ctx: &SmContext<K>,
        id: StateId<K>,
        item: RetainItem<K>,
        duration: Duration,
    ) {
        ctx.notification_queue.priority_send(Notification(
            TimeoutInput::retain(id, item, duration).into(),
        ));
    }

    fn cancel_timeout(&self, ctx: &SmContext<K>, id: StateId<K>) {
        ctx.notification_queue
            .priority_send(Notification(TimeoutInput::cancel_timeout(id).into()));
    }

    fn get_parent_id(&self, ctx: &SmContext<K>, id: StateId<K>) -> Option<StateId<K>> {
        self.get_tree(ctx, id).and_then(|tree| {
            let guard = tree.lock();
            guard.get_parent_id(id)
        })
    }
}

impl<K, T> StateMachineExt<K> for T
where
    T: StateMachine<K>,
    K: Rex,
    K::Message: Retain<K>,
{
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bigerror::{ConversionError, Report};
    use dashmap::DashMap;
    use tokio::time::Instant;
    use tracing::*;

    use super::*;
    use crate::{
        node::{Insert, Node},
        notification::GetTopic,
        storage::StateStore,
        test_support::Hold,
        timeout::{Retain, Return, TimeoutTopic, TEST_TICK_RATE, TEST_TIMEOUT},
        Rex, RexBuilder, RexMessage,
    };

    impl From<TimeoutInput<Game>> for GameMsg {
        fn from(value: TimeoutInput<Game>) -> Self {
            Self(value)
        }
    }

    #[derive(Debug, Clone)]
    pub struct GameMsg(TimeoutInput<Game>);
    impl GetTopic<TimeoutTopic> for GameMsg {
        fn get_topic(&self) -> TimeoutTopic {
            TimeoutTopic
        }
    }
    impl RexMessage for GameMsg {
        type Topic = TimeoutTopic;
    }

    impl TryInto<TimeoutInput<Game>> for GameMsg {
        type Error = Report<ConversionError>;

        fn try_into(self) -> Result<TimeoutInput<Game>, Self::Error> {
            Ok(self.0)
        }
    }

    impl Retain<Game> for GameMsg {
        type Item = Hold<Packet>;
    }

    #[derive(Copy, Clone, Debug, derive_more::Display)]
    #[display("{msg}")]
    pub struct Packet {
        msg: u64,
        sender: StateId<Game>,
        who_holds: WhoHolds,
    }

    #[derive(Clone, Debug, derive_more::From)]
    pub enum Input {
        Ping(PingInput),
        Pong(PongInput),
        Menu(MenuInput),
    }

    // determines whether Ping or Pong will await before packet send
    //
    #[derive(Copy, Clone, PartialEq, Debug)]
    pub struct WhoHolds(Option<Game>);

    #[derive(Clone, PartialEq, Debug)]
    pub enum MenuInput {
        Play(WhoHolds),
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

    #[derive(Clone, Debug, derive_more::From)]
    pub enum PingInput {
        StartSending(StateId<Game>, WhoHolds),
        Returned(Hold<Packet>),
        Packet(Packet),
        #[allow(dead_code)]
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

    #[derive(Clone, Debug, derive_more::From)]
    pub enum PongInput {
        Packet(Packet),
        #[allow(dead_code)]
        RecvTimeout(Instant),
        Returned(Hold<Packet>),
    }

    #[derive(Copy, Clone, PartialEq, Debug)]
    pub enum GameState {
        Ping(PingState),
        Pong(PongState),
        Menu(MenuState),
    }

    impl State for GameState {
        fn get_kind(&self) -> &dyn Kind<State = Self> {
            match self {
                GameState::Ping(_) => &Game::Ping,
                GameState::Pong(_) => &Game::Pong,
                GameState::Menu(_) => &Game::Menu,
            }
        }
    }

    #[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
    pub enum Game {
        Ping,
        Pong,
        Menu,
    }

    impl Rex for Game {
        type Input = Input;
        type Message = GameMsg;

        fn state_input(&self, state: <Self as Kind>::State) -> Option<Self::Input> {
            if *self != Game::Menu {
                return None;
            }

            match state {
                GameState::Ping(PingState::Done) => Some(MenuInput::PingPongComplete),
                GameState::Ping(PingState::Failed) => Some(MenuInput::FailedPing),
                GameState::Pong(PongState::Failed) => Some(MenuInput::FailedPong),
                _ => None,
            }
            .map(|i| i.into())
        }

        fn timeout_input(&self, instant: Instant) -> Option<Self::Input> {
            match self {
                Game::Ping => Some(PingInput::RecvTimeout(instant).into()),
                Game::Pong => Some(PongInput::RecvTimeout(instant).into()),
                Game::Menu => None,
            }
        }
    }

    impl Return for Game {
        fn return_item(&self, item: RetainItem<Self>) -> Option<Self::Input> {
            match self {
                Game::Ping => Some(Input::Ping(item.into())),
                Game::Pong => Some(Input::Pong(item.into())),
                Game::Menu => None,
            }
        }
    }

    impl Kind for Game {
        type State = GameState;

        fn new_state(&self) -> Self::State {
            match self {
                Game::Ping => GameState::Ping(PingState::default()),
                Game::Pong => GameState::Pong(PongState::default()),
                Game::Menu => GameState::Menu(MenuState::default()),
            }
        }

        fn failed_state(&self) -> Self::State {
            match self {
                Game::Ping => GameState::Ping(PingState::Failed),
                Game::Pong => GameState::Pong(PongState::Failed),
                Game::Menu => GameState::Menu(MenuState::Failed),
            }
        }

        fn completed_state(&self) -> Self::State {
            match self {
                Game::Ping => GameState::Ping(PingState::Done),
                Game::Pong => GameState::Pong(PongState::Done),
                Game::Menu => GameState::Menu(MenuState::Done),
            }
        }
    }

    struct MenuStateMachine {
        failures: Arc<DashMap<StateId<Game>, MenuInput>>,
    }

    impl StateMachine<Game> for MenuStateMachine {
        #[instrument(name = "menu", skip_all)]
        fn process(&self, ctx: SmContext<Game>, id: StateId<Game>, input: Input) {
            let Input::Menu(input) = input else {
                error!(input = ?input, "invalid input!");
                return;
            };

            let state = self.get_state(&ctx, id);
            if let Some(true) = state.map(Self::terminal_state) {
                warn!(%id, ?state, "Ignoring input due to invalid state");
                return;
            }

            match input {
                MenuInput::Play(who_holds) => {
                    let ping_id = StateId::new_rand(Game::Ping);
                    let pong_id = StateId::new_rand(Game::Pong);
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

                    let tree = StateStore::new_tree(menu_tree);
                    for id in [id, ping_id, pong_id] {
                        ctx.state_store.insert_ref(id, tree.clone());
                    }

                    // signal to Ping state machine
                    ctx.signal_queue.push_back(Signal {
                        id: ping_id,
                        input: Input::Ping(PingInput::StartSending(pong_id, who_holds)),
                    });
                }
                MenuInput::PingPongComplete => {
                    info!("I'M DONE!");
                    self.complete(&ctx, id);
                }
                failure @ (MenuInput::FailedPing | MenuInput::FailedPong) => {
                    let tree = self.get_tree(&ctx, id).unwrap();
                    let mut guard = tree.lock();
                    // set all states to failed state
                    guard.update_all_fn(|mut z| {
                        z.node.state.fail();
                        self.cancel_timeout(&ctx, z.node.id); // cancel all other timeouts
                        z.finish_update()
                    });

                    error!(input = ?failure, "Encountered Failure!");
                    self.failures.insert(id, failure);
                }
            }
        }

        fn get_kind(&self) -> Game {
            Game::Menu
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

    impl StateMachine<Game> for PingStateMachine {
        #[instrument(name = "ping", skip_all)]
        fn process(&self, ctx: SmContext<Game>, id: StateId<Game>, input: Input) {
            let Input::Ping(input) = input else {
                error!(?input, "invalid input!");
                return;
            };
            let state = self.get_state(&ctx, id).unwrap();
            if Self::terminal_state(state) {
                warn!(%id, ?state, "Ignoring input due to invalid state");
                return;
            }

            match input {
                PingInput::StartSending(pong_id, who_holds) => {
                    self.update(&ctx, id, GameState::Ping(PingState::Sending));
                    info!(msg = 0, "PINGING");
                    ctx.signal_queue.push_back(Signal {
                        id: pong_id,
                        input: Input::Pong(PongInput::Packet(Packet {
                            msg: 0,
                            sender: id,
                            who_holds,
                        })),
                    });
                    // TODO let timeout = now + Duration::from_millis(250);
                }
                PingInput::Packet(Packet { msg: 25.., .. }) => {
                    info!("PING Complete!");
                    self.complete(&ctx, id);
                    self.cancel_timeout(&ctx, id);
                }
                PingInput::Packet(mut packet) => {
                    self.set_timeout(&ctx, id, TEST_TIMEOUT);
                    packet.msg += 5;

                    if let WhoHolds(Some(Game::Ping)) = packet.who_holds {
                        info!(msg = packet.msg, "HOLDING");
                        // hold for half theduration of the message
                        let hold_for = Duration::from_millis(packet.msg);
                        self.return_in(&ctx, id, Hold(packet), hold_for);
                        return;
                    }

                    info!(msg = packet.msg, "PINGING");
                    return_packet(&ctx, id, packet);
                }
                PingInput::Returned(Hold(packet)) => {
                    self.set_timeout(&ctx, id, TEST_TIMEOUT);
                    info!(msg = packet.msg, "PINGING");
                    return_packet(&ctx, id, packet);
                }

                PingInput::RecvTimeout(_) => {
                    self.fail(&ctx, id);
                }
            }
        }

        fn get_kind(&self) -> Game {
            Game::Ping
        }
    }

    struct PongStateMachine;

    impl StateMachine<Game> for PongStateMachine {
        #[instrument(name = "pong", skip_all)]
        fn process(&self, ctx: SmContext<Game>, id: StateId<Game>, input: Input) {
            let Input::Pong(input) = input else {
                error!(?input, "invalid input!");
                return;
            };
            let state = self.get_state(&ctx, id).unwrap();
            if Self::terminal_state(state) {
                warn!(%id, ?state, "Ignoring input due to invalid state");
                return;
            }

            match input {
                PongInput::Packet(Packet {
                    // https://doc.rust-lang.org/book/ch18-03-pattern-syntax.html#-bindings
                    msg: mut msg @ 20..,
                    sender,
                    who_holds,
                }) => {
                    msg += 5;
                    info!(?msg, "PONGING");
                    self.complete(&ctx, id);
                    self.cancel_timeout(&ctx, id);
                    ctx.signal_queue.push_back(Signal {
                        id: sender,
                        input: Input::Ping(PingInput::Packet(Packet {
                            msg,
                            sender: id,
                            who_holds,
                        })),
                    });
                }
                PongInput::Packet(mut packet) => {
                    self.set_timeout(&ctx, id, TEST_TIMEOUT);
                    if packet.msg == 0 {
                        self.update(&ctx, id, GameState::Pong(PongState::Responding));
                    }
                    packet.msg += 5;

                    if let WhoHolds(Some(Game::Pong)) = packet.who_holds {
                        info!(msg = packet.msg, "HOLDING");
                        // hold for half theduration of the message
                        let hold_for = Duration::from_millis(packet.msg) / 2;
                        self.return_in(&ctx, id, Hold(packet), hold_for);
                        return;
                    }

                    info!(msg = packet.msg, "PONGING");
                    let recipient = packet.sender;
                    packet.sender = id;
                    ctx.signal_queue.push_back(Signal {
                        id: recipient,
                        input: PingInput::Packet(packet).into(),
                    });
                }
                PongInput::Returned(Hold(packet)) => {
                    self.set_timeout(&ctx, id, TEST_TIMEOUT);
                    info!(msg = packet.msg, "PONGING");
                    return_packet(&ctx, id, packet);
                }
                PongInput::RecvTimeout(_) => {
                    self.fail(&ctx, id);
                }
            }
        }
        fn get_kind(&self) -> Game {
            Game::Pong
        }
    }
    fn return_packet(ctx: &SmContext<Game>, id: StateId<Game>, mut packet: Packet) {
        let recipient = packet.sender;
        packet.sender = id;
        ctx.signal_queue.push_back(Signal {
            id: recipient,
            input: match *id {
                Game::Ping => PongInput::Packet(packet).into(),
                Game::Pong => PingInput::Packet(packet).into(),
                Game::Menu => unreachable!(),
            },
        });
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn state_machine() {
        // This test does not initialize the NotificationManager
        let ctx = RexBuilder::new()
            .with_sm(MenuStateMachine::new())
            .with_sm(PingStateMachine)
            .with_sm(PongStateMachine)
            .build();

        let menu_id = StateId::new_rand(Game::Menu);
        ctx.signal_queue.push_back(Signal {
            id: menu_id,
            input: Input::Menu(MenuInput::Play(WhoHolds(None))),
        });
        tokio::time::sleep(Duration::from_millis(1)).await;

        let tree = ctx.state_store.get_tree(menu_id).unwrap();
        let node = tree.lock();
        let ping_id = node.children[0].id;
        let pong_id = node.children[1].id;
        assert_eq!(menu_id, node.id);
        assert_eq!(GameState::Menu(MenuState::Done), node.state);

        // !!NOTE!! ============================================================
        // we are trying to acquire another lock...
        // * from the SAME Mutex
        // * in the SAME thread
        // this WILL deadlock unless the previous lock is dropped
        drop(node);
        // !!NOTE!! ============================================================

        // ensure MenuState is also indexed by ping id
        let tree = ctx.state_store.get_tree(ping_id).unwrap();
        let node = tree.lock();
        let state = node.get_state(ping_id).unwrap();
        assert_eq!(GameState::Ping(PingState::Done), *state);

        drop(node);

        let tree = ctx.state_store.get_tree(pong_id).unwrap();
        let node = tree.lock();
        let state = node.get_state(pong_id).unwrap();
        assert_eq!(GameState::Pong(PongState::Done), *state);
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn pong_timeout() {
        let menu_sm = MenuStateMachine::new();
        let menu_failures = menu_sm.failures.clone();
        let ctx = RexBuilder::new()
            .with_sm(menu_sm)
            .with_sm(PingStateMachine)
            .with_sm(PongStateMachine)
            .with_timeout_manager(TimeoutTopic)
            .with_tick_rate(TEST_TICK_RATE / 2)
            .build();

        let menu_id = StateId::new_rand(Game::Menu);
        ctx.signal_queue.push_back(Signal {
            id: menu_id,
            input: Input::Menu(MenuInput::Play(WhoHolds(Some(Game::Ping)))),
        });

        tokio::time::sleep(TEST_TIMEOUT * 4).await;

        {
            let tree = ctx.state_store.get_tree(menu_id).unwrap();
            let node = tree.lock();
            let ping_node = &node.children[0];
            let pong_node = &node.children[1];
            assert_eq!(menu_id, node.id);
            assert_eq!(GameState::Menu(MenuState::Failed), node.state);
            assert_eq!(GameState::Ping(PingState::Failed), ping_node.state);
            assert_eq!(GameState::Pong(PongState::Failed), pong_node.state);

            // !!NOTE!! ============================================================
            // we are trying to acquire another lock...
            // * from the SAME Mutex
            // * in the SAME thread
            // this WILL deadlock unless the previous lock is dropped
            drop(node);
            // !!NOTE!! ============================================================
        }

        // Ensure that our Menu failed due to Pong timing out since
        // Ping "slept on" the packet
        assert_eq!(MenuInput::FailedPong, *menu_failures.get(&menu_id).unwrap());
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn ping_timeout() {
        // Now fail due to Ping
        let menu_sm = MenuStateMachine::new();
        let menu_failures = menu_sm.failures.clone();
        let ctx = RexBuilder::new()
            .with_sm(menu_sm)
            .with_sm(PingStateMachine)
            .with_sm(PongStateMachine)
            .with_timeout_manager(TimeoutTopic)
            .with_tick_rate(TEST_TICK_RATE / 2)
            .build();
        let menu_id = StateId::new_rand(Game::Menu);
        ctx.signal_queue.push_back(Signal {
            id: menu_id,
            input: Input::Menu(MenuInput::Play(WhoHolds(Some(Game::Pong)))),
        });

        tokio::time::sleep(TEST_TIMEOUT * 4).await;

        let tree = ctx.state_store.get_tree(menu_id).unwrap();
        let node = tree.lock();
        let ping_node = &node.children[0];
        let pong_node = &node.children[1];
        assert_eq!(menu_id, node.id);
        assert_eq!(GameState::Menu(MenuState::Failed), node.state);
        assert_eq!(GameState::Ping(PingState::Failed), ping_node.state);
        assert_eq!(GameState::Pong(PongState::Failed), pong_node.state);
        // Ensure that our Menu failed due to Ping
        assert_eq!(MenuInput::FailedPing, *menu_failures.get(&menu_id).unwrap());
    }
}
