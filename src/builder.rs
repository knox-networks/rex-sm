use std::time::Duration;

use bigerror::{ConversionError, Report};
use tokio::{sync::mpsc::UnboundedSender, task::JoinSet};

use crate::{
    ingress::{BoxedStateRouter, IngressAdapter},
    manager::BoxedStateMachine,
    notification::NotificationQueue,
    timeout::{self, TimeoutInput, TimeoutManager},
    NotificationManager, NotificationProcessor, Rex, RexMessage, SignalQueue, SmContext,
    StateMachine, StateMachineManager,
};

pub struct RexBuilder<K, Out = ()>
where
    K: Rex,
    Out: Send + Sync + std::fmt::Debug,
{
    signal_queue: SignalQueue<K>,
    notification_queue: NotificationQueue<K::Message>,
    state_machines: Vec<BoxedStateMachine<K>>,
    notification_processors: Vec<Box<dyn NotificationProcessor<K::Message>>>,
    timeout_topic: Option<<K::Message as RexMessage>::Topic>,
    tick_rate: Option<Duration>,
    outbound_tx: Option<UnboundedSender<Out>>,
}

pub struct InitContext<K: Rex> {
    pub signal_queue: SignalQueue<K>,
    pub notification_queue: NotificationQueue<K::Message>,
}

impl<K, Out> RexBuilder<K, Out>
where
    K: Rex,
    K::Message: From<TimeoutInput<K>> + TryInto<TimeoutInput<K>>,
    <K::Message as TryInto<TimeoutInput<K>>>::Error: Send,
    TimeoutManager<K>: NotificationProcessor<K::Message>,
    Out: Send + Sync + std::fmt::Debug + 'static,
{
    fn ctx(&self) -> InitContext<K> {
        InitContext {
            signal_queue: self.signal_queue.clone(),
            notification_queue: self.notification_queue.clone(),
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sm<SM: StateMachine<K> + 'static>(&mut self, state_machine: SM) -> &mut Self {
        self.state_machines.push(Box::new(state_machine));
        self
    }

    pub fn with_np<NP: NotificationProcessor<K::Message> + 'static>(
        &mut self,
        processor: NP,
    ) -> &mut Self {
        self.notification_processors.push(Box::new(processor));
        self
    }

    pub fn with_ctx_np<NP: NotificationProcessor<K::Message> + 'static>(
        &mut self,
        op: impl FnOnce(InitContext<K>) -> NP,
    ) -> &mut Self {
        self.notification_processors.push(Box::new(op(self.ctx())));
        self
    }

    pub fn with_boxed_np(
        &mut self,
        processor: Box<dyn NotificationProcessor<K::Message>>,
    ) -> &mut Self {
        self.notification_processors.push(processor);
        self
    }

    pub fn with_outbound_tx(&mut self, tx: UnboundedSender<Out>) -> &mut Self {
        self.outbound_tx = Some(tx);
        self
    }

    pub fn with_ingress_adapter<In>(
        &mut self,
        state_routers: Vec<BoxedStateRouter<K, In>>,
        ingress_topic: <K::Message as RexMessage>::Topic,
    ) -> &mut Self
    where
        for<'a> K: TryFrom<&'a In, Error = Report<ConversionError>>,
        In: Send + Sync + std::fmt::Debug + 'static,
        K::Input: TryFrom<In, Error = Report<ConversionError>>,
        K::Message: TryInto<Out, Error = Report<ConversionError>>,
    {
        let outbound_tx = self
            .outbound_tx
            .clone()
            .expect("RexBuilder: outbound_tx is uninitialized");
        self.with_ctx_np(move |ctx| {
            IngressAdapter::new(ctx.signal_queue, outbound_tx, state_routers, ingress_topic)
        });
        self
    }

    pub fn with_timeout_manager(
        &mut self,
        timeout_topic: <K::Message as RexMessage>::Topic,
    ) -> &mut Self {
        self.timeout_topic = Some(timeout_topic);
        self
    }

    pub fn with_tick_rate(&mut self, tick_rate: Duration) -> &mut Self {
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

    fn build_inner(mut self, join_set: &mut JoinSet<()>) -> SmContext<K> {
        self.build_timeout_manager();

        if !self.notification_processors.is_empty() {
            NotificationManager::new(
                self.notification_processors,
                join_set,
                self.notification_queue.clone(),
            )
            .init(join_set);
        }
        let smm = StateMachineManager::new(
            self.state_machines,
            self.signal_queue,
            self.notification_queue.clone(),
        );

        smm.init(join_set);
        smm.context()
    }

    pub fn build(self) -> SmContext<K> {
        let mut join_set = JoinSet::new();
        let ctx = self.build_inner(&mut join_set);
        join_set.detach_all();

        ctx
    }

    pub fn build_with_handle(self, join_set: &mut JoinSet<()>) -> SmContext<K> {
        self.build_inner(join_set)
    }
}

impl<K, Out> Default for RexBuilder<K, Out>
where
    K: Rex,
    K::Message: From<TimeoutInput<K>> + TryInto<TimeoutInput<K>>,
    <K::Message as TryInto<TimeoutInput<K>>>::Error: Send,
    TimeoutManager<K>: NotificationProcessor<K::Message>,
    Out: Send + Sync + std::fmt::Debug + 'static,
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
        }
    }
}
