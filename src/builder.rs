use std::time::Duration;

use bigerror::{ConversionError, Report};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinSet,
};

use crate::{
    ingress::{BoxedStateRouter, Ingress, IngressAdapter, PacketRouter},
    manager::{BoxedStateMachine, EmptyContext},
    notification::NotificationQueue,
    timeout::{self, Timeout, TimeoutManager, TimeoutMessage},
    NotificationManager, NotificationProcessor, Rex, RexMessage, SignalQueue, StateMachine,
    StateMachineManager,
};

pub struct RexBuilder<K, In = (), Out = ()>
where
    K: Rex,
    In: Send + Sync + std::fmt::Debug,
    Out: Send + Sync + std::fmt::Debug,
{
    signal_queue: SignalQueue<K>,
    notification_queue: NotificationQueue<K::Message>,
    state_machines: Vec<BoxedStateMachine<K>>,
    notification_processors: Vec<Box<dyn NotificationProcessor<K::Message>>>,
    timeout_topic: Option<<K::Message as RexMessage>::Topic>,
    tick_rate: Option<Duration>,
    outbound_tx: Option<UnboundedSender<Out>>,
    ingress_channel: Option<(UnboundedSender<In>, UnboundedReceiver<In>)>,
}

// context used before calling build
pub struct BuilderContext<K: Rex> {
    pub signal_queue: SignalQueue<K>,
    pub notification_queue: NotificationQueue<K::Message>,
}

impl<K: Rex> RexBuilder<K, (), ()> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<K, In, Out> RexBuilder<K, In, Out>
where
    K: Rex + Timeout,
    K::Message: TimeoutMessage<K>,
    In: Send + Sync + std::fmt::Debug,
    Out: Send + Sync + std::fmt::Debug,
    TimeoutManager<K>: NotificationProcessor<K::Message>,
{
    pub fn ctx(&self) -> BuilderContext<K> {
        BuilderContext {
            signal_queue: self.signal_queue.clone(),
            notification_queue: self.notification_queue.clone(),
        }
    }

    #[must_use]
    pub fn with_sm<SM: StateMachine<K> + 'static>(mut self, state_machine: SM) -> Self {
        self.state_machines.push(Box::new(state_machine));
        self
    }

    #[must_use]
    pub fn with_np<NP: NotificationProcessor<K::Message> + 'static>(
        mut self,
        processor: NP,
    ) -> Self {
        self.push_np(processor);
        self
    }

    pub fn push_np<NP: NotificationProcessor<K::Message> + 'static>(&mut self, processor: NP) {
        self.notification_processors.push(Box::new(processor));
    }

    #[must_use]
    pub fn with_ctx_np<NP: NotificationProcessor<K::Message> + 'static>(
        mut self,
        op: impl FnOnce(BuilderContext<K>) -> NP,
    ) -> Self {
        self.notification_processors.push(Box::new(op(self.ctx())));
        self
    }

    pub fn push_ctx_np<NP: NotificationProcessor<K::Message> + 'static>(
        &mut self,
        op: impl FnOnce(BuilderContext<K>) -> NP,
    ) {
        self.notification_processors.push(Box::new(op(self.ctx())));
    }

    #[must_use]
    pub fn with_boxed_np(mut self, processor: Box<dyn NotificationProcessor<K::Message>>) -> Self {
        self.notification_processors.push(processor);
        self
    }

    #[must_use]
    pub fn with_timeout_manager(
        mut self,
        timeout_topic: <K::Message as RexMessage>::Topic,
    ) -> Self {
        self.timeout_topic = Some(timeout_topic);
        self
    }

    #[must_use]
    pub fn with_tick_rate(mut self, tick_rate: Duration) -> Self {
        self.tick_rate = Some(tick_rate);
        self
    }

    fn build_timeout_manager(&mut self) {
        if let Some(topic) = self.timeout_topic {
            let timeout_manager = TimeoutManager::new(self.signal_queue.clone(), topic)
                .with_tick_rate(self.tick_rate.unwrap_or(timeout::DEFAULT_TICK_RATE));
            self.notification_processors.push(Box::new(timeout_manager));
        }
    }

    fn build_inner(mut self, join_set: &mut JoinSet<()>) -> EmptyContext<K> {
        self.build_timeout_manager();

        if !self.notification_processors.is_empty() {
            NotificationManager::new(
                self.notification_processors,
                join_set,
                self.notification_queue.clone(),
            )
            .init(join_set);
        }
        let sm_manager = StateMachineManager::new(
            self.state_machines,
            self.signal_queue,
            self.notification_queue.clone(),
        );

        sm_manager.init(join_set);
        sm_manager.ctx_builder()
    }

    pub fn build(self) -> EmptyContext<K> {
        let mut join_set = JoinSet::new();
        let ctx = self.build_inner(&mut join_set);
        join_set.detach_all();

        ctx
    }

    pub fn build_with_handle(self, join_set: &mut JoinSet<()>) -> EmptyContext<K> {
        self.build_inner(join_set)
    }
}

impl<K> RexBuilder<K, K::In, K::Out>
where
    K: Rex + Timeout + Ingress,

    K::Message: TimeoutMessage<K> + TryInto<K::Out, Error = Report<ConversionError>>,
    K::Input: TryFrom<K::In, Error = Report<ConversionError>>,
    TimeoutManager<K>: NotificationProcessor<K::Message>,
{
    #[must_use]
    pub fn new_connected(
        outbound_tx: UnboundedSender<K::Out>,
    ) -> (UnboundedSender<K::In>, RexBuilder<K, K::In, K::Out>) {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<K::In>();
        (
            inbound_tx.clone(),
            Self {
                outbound_tx: Some(outbound_tx),
                ingress_channel: Some((inbound_tx, inbound_rx)),
                ..Default::default()
            },
        )
    }

    #[must_use]
    pub fn ingress_tx(&self) -> UnboundedSender<K::In> {
        self.ingress_channel
            .as_ref()
            .map(|(tx, _)| tx.clone())
            .expect("ingress_channel uninitialized")
    }

    #[must_use]
    pub fn with_ingress_adapter(
        mut self,
        state_routers: Vec<BoxedStateRouter<K, K::In>>,
        ingress_topic: <K::Message as RexMessage>::Topic,
    ) -> Self {
        assert!(!state_routers.is_empty());
        let (tx, rx) = self
            .ingress_channel
            .take()
            .expect("ingress_channel uninitialized");
        let outbound_tx = self
            .outbound_tx
            .clone()
            .expect("builder outbound_tx uninitialized");

        let ingress_adapter = IngressAdapter {
            signal_queue: self.signal_queue.clone(),
            outbound_tx,
            router: PacketRouter::new(state_routers),
            inbound_tx: tx,
            inbound_rx: Some(rx),
            topic: ingress_topic,
        };
        self.with_np(ingress_adapter)
    }
}

impl<K, In, Out> Default for RexBuilder<K, In, Out>
where
    K: Rex,
    In: Send + Sync + std::fmt::Debug,
    Out: Send + Sync + std::fmt::Debug,
{
    fn default() -> Self {
        Self {
            notification_queue: NotificationQueue::new(),
            signal_queue: Default::default(),
            state_machines: Default::default(),
            notification_processors: Default::default(),
            timeout_topic: None,
            tick_rate: None,
            outbound_tx: None,
            ingress_channel: None,
        }
    }
}
