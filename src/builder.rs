use crate::Rex;

pub struct RexBuilder<K>
where
    K: Rex,
{
    signal_queue: SignalQueue<K>,
    state_machines: Vec<BoxedStateMachine<K>>,
    notification_processors: Vec<Box<dyn NotificationProcessor<K::Message>>>,
    timeout_topic: Option<<K::Message as RexMessage>::Topic>,
    tick_rate: Option<Duration>,
}

impl<K> RexBuilder<K>
where
    K: Rex,
    K::Message: From<TimeoutInput<K>> + TryInto<TimeoutInput<K>>,
    <K::Message as TryInto<TimeoutInput<K>>>::Error: Send,
    TimeoutManager<K>: NotificationProcessor<K::Message>,
{
    #[must_use]
    pub fn new(signal_queue: SignalQueue<K>) -> Self {
        Self {
            signal_queue,
            state_machines: vec![],
            notification_processors: vec![],
            timeout_topic: None,
            tick_rate: None,
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
        self.notification_processors.push(Box::new(processor));
        self
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

    pub fn with_tick_rate(mut self, tick_rate: Duration) -> Self {
        self.tick_rate = Some(tick_rate);
        self
    }

    fn build(mut self, join_set: &mut JoinSet<()>) -> SmContext<K> {
        if let Some(topic) = self.timeout_topic {
            let timeout_manager = TimeoutManager::new(self.signal_queue.clone(), topic)
                .with_tick_rate(self.tick_rate.unwrap_or(timeout::DEFAULT_TICK_RATE));
            self.notification_processors.push(Box::new(timeout_manager));
        }

        let notification_manager = NotificationManager::new(self.notification_processors, join_set);
        let notification_queue = notification_manager.init(join_set);

        let smm =
            StateMachineManager::new(self.state_machines, self.signal_queue, notification_queue);

        smm.init(join_set);
        smm.context()
    }

    pub fn build_and_init_with_handle(self, join_set: &mut JoinSet<()>) -> SmContext<K> {
        self.build(join_set)
    }

    pub fn build_and_init(self) -> SmContext<K> {
        let mut join_set = JoinSet::new();
        let ctx = self.build(&mut join_set);
        join_set.detach_all();

        ctx
    }
}
