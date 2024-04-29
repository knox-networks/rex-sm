use std::{collections::HashMap, fmt, sync::Arc};

use bigerror::{ConversionError, IntoContext, LogError, Report};
use tokio::{
    sync::{
        mpsc,
        mpsc::{UnboundedReceiver, UnboundedSender},
    },
    task::JoinSet,
};
use tracing::{debug, error, trace, warn, Instrument};

use crate::{
    manager::{HashKind, Signal, SignalQueue},
    notification::{Notification, NotificationProcessor, RexMessage},
    queue::StreamableDeque,
    Rex, RexError, StateId,
};

pub trait StateRouter<K>: Send + Sync
where
    K: HashKind,
{
    type Inbound;
    fn get_id(&self, input: &Self::Inbound) -> Result<Option<StateId<K>>, Report<RexError>>;
    fn get_kind(&self) -> K;
}

pub type BoxedStateRouter<K, In> = Box<dyn StateRouter<K, Inbound = In>>;

/// top level router that holds all [`Kind`] indexed [`StateRouter`]s
pub struct PacketRouter<K, In>(Arc<HashMap<K, BoxedStateRouter<K, In>>>)
where
    K: HashKind;

impl<K: HashKind, In> Clone for PacketRouter<K, In> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<K, In> PacketRouter<K, In>
where
    for<'a> K: HashKind + TryFrom<&'a In, Error = Report<ConversionError>>,
{
    pub fn new(state_routers: Vec<BoxedStateRouter<K, In>>) -> Self {
        let mut router_map: HashMap<K, BoxedStateRouter<K, In>> = HashMap::new();
        for router in state_routers {
            if let Some(old_router) = router_map.insert(router.get_kind(), router) {
                panic!(
                    "Found multiple routers for kind: {:?}",
                    old_router.get_kind()
                );
            }
        }
        Self(Arc::new(router_map))
    }

    fn get_id(&self, packet: &In) -> Result<Option<StateId<K>>, Report<RexError>> {
        let kind = K::try_from(packet);
        let kind = kind.map_err(|e| e.into_ctx())?;
        let Some(router) = self.0.get(&kind) else {
            return Ok(None);
        };
        router.get_id(packet)
    }
}

/// Represents a bidirectional network connection
pub struct IngressAdapter<K, In, Out>
where
    K: Rex,
    In: Send + Sync + fmt::Debug,
    Out: Send + Sync + fmt::Debug,
{
    pub(crate) outbound_tx: UnboundedSender<Out>,
    pub(crate) signal_queue: SignalQueue<K>,
    pub(crate) router: PacketRouter<K, In>,
    pub inbound_tx: UnboundedSender<In>,
    // `self.inbound_rx.take()` will be used on intialization
    pub(crate) inbound_rx: Option<UnboundedReceiver<In>>,
    pub(crate) topic: <K::Message as RexMessage>::Topic,
}

impl<K, In, Out> IngressAdapter<K, In, Out>
where
    K: Rex,
    for<'a> K: TryFrom<&'a In, Error = Report<ConversionError>>,
    K::Input: TryFrom<In, Error = Report<ConversionError>>,
    K::Message: TryInto<Out, Error = Report<ConversionError>>,
    In: Send + Sync + fmt::Debug + 'static,
    Out: Send + Sync + fmt::Debug + 'static,
{
    #[must_use]
    pub fn new(
        signal_queue: SignalQueue<K>,
        outbound_tx: UnboundedSender<Out>,
        state_routers: Vec<BoxedStateRouter<K, In>>,
        topic: impl Into<<K::Message as RexMessage>::Topic>,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<In>();

        Self {
            signal_queue,
            outbound_tx,
            router: PacketRouter::new(state_routers),
            inbound_tx,
            inbound_rx: Some(inbound_rx),
            topic: topic.into(),
        }
    }

    pub(crate) fn spawn_inbound(&mut self, join_set: &mut JoinSet<()>) {
        let router = self.router.clone();
        let signal_queue = self.signal_queue.clone();
        let inbound_rx = self.inbound_rx.take().expect("inbound_rx missing");
        join_set.spawn(Self::process_inbound(router, signal_queue, inbound_rx).in_current_span());
    }

    async fn process_inbound(
        router: PacketRouter<K, In>,
        signal_queue: Arc<StreamableDeque<Signal<K>>>,
        mut packet_rx: UnboundedReceiver<In>,
    ) {
        debug!(target: "state_machine", spawning = "IngressAdapter.packet_tx");
        while let Some(packet) = packet_rx.recv().await {
            trace!("receiving packet");
            let id = match router.get_id(&packet) {
                Err(e) => {
                    error!(err = ?e, ?packet, "could not get id from router");
                    continue;
                }
                Ok(None) => {
                    warn!(?packet, "unable to route packet");
                    continue;
                }
                Ok(Some(state_id)) => state_id,
            };
            K::Input::try_from(packet)
                .map(|input| {
                    signal_queue.push_back(Signal { id, input });
                })
                .log_attached_err("ia::processors from packet failed");
        }
    }
}

impl<K, In, Out> NotificationProcessor<K::Message> for IngressAdapter<K, In, Out>
where
    K: Rex,
    for<'a> K: TryFrom<&'a In, Error = Report<ConversionError>>,
    K::Input: TryFrom<In, Error = Report<ConversionError>>,
    K::Message: TryInto<Out, Error = Report<ConversionError>>,
    In: Send + Sync + fmt::Debug + 'static,
    Out: Send + Sync + fmt::Debug + 'static,
{
    fn init(&mut self, join_set: &mut JoinSet<()>) -> UnboundedSender<Notification<K::Message>> {
        debug!("calling IngressAdapter::process_inbound");
        self.spawn_inbound(join_set);

        debug!("starting IngressAdapter notification_tx");

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Notification<K::Message>>();
        let outbound_tx = self.outbound_tx.clone();

        let _notification_handle = join_set.spawn(
            async move {
                debug!(target: "state_machine", spawning = "IngressAdapter.notification_tx");
                while let Some(notification) = input_rx.recv().await {
                    notification
                        .0
                        .try_into()
                        .map(|packet| {
                            trace!("sending packet");
                            outbound_tx.send(packet).log_err();
                        })
                        .log_attached_err("Invalid input");
                }
            }
            .in_current_span(),
        );

        input_tx
    }

    fn get_topics(&self) -> &[<K::Message as RexMessage>::Topic] {
        std::slice::from_ref(&self.topic)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{sync::mpsc::UnboundedReceiver, task::JoinSet};
    use tokio_stream::StreamExt;

    use super::*;
    use crate::{
        notification::{NotificationManager, NotificationQueue},
        test_support::*,
        RexBuilder, StateId,
    };

    type TestIngressAdapter = (
        IngressAdapter<TestKind, InPacket, OutPacket>,
        UnboundedReceiver<OutPacket>,
    );

    impl TestDefault for TestIngressAdapter {
        fn test_default() -> Self {
            let signal_queue = SignalQueue::default();
            let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();

            let adapter = IngressAdapter::new(
                signal_queue,
                outbound_tx,
                vec![Box::new(TestStateRouter)],
                TestTopic::Ingress,
            );
            (adapter, outbound_rx)
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn route_to_network() {
        let (adapter, mut network_rx) = TestIngressAdapter::test_default();
        let mut join_set = JoinSet::new();

        let notification_manager: NotificationManager<TestMsg> = NotificationManager::new(
            vec![Box::new(adapter)],
            &mut join_set,
            NotificationQueue::new(),
        );
        let notification_tx = notification_manager.init(&mut join_set);

        let unknown_packet = OutPacket(b"unknown_packet".to_vec());

        // Any packet should get to the GatewayClient since routing rules
        // are only used at the ingress of the state machine
        notification_tx.send(Notification(unknown_packet.clone().into()));
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(Ok(unknown_packet), network_rx.try_recv());

        let unsupported_packet = OutPacket(b"unsupported_packet".to_vec());

        notification_tx.send(Notification(unsupported_packet.clone().into()));
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(Ok(unsupported_packet), network_rx.try_recv());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn route_from_network() {
        let (adapter, _outbound_rx) = TestIngressAdapter::test_default();
        let signal_queue = adapter.signal_queue.clone();
        let signal_rx = signal_queue.stream().timeout(Duration::from_millis(2));
        tokio::pin!(signal_rx);

        let inboud_tx = adapter.inbound_tx.clone();
        let mut join_set = JoinSet::new();

        let notification_manager: NotificationManager<TestMsg> = NotificationManager::new(
            vec![Box::new(adapter)],
            &mut join_set,
            NotificationQueue::new(),
        );
        let _notification_tx = notification_manager.init(&mut join_set);

        // An unknown packet should be unrouteable
        let unknown_packet = InPacket(b"unknown_packet".to_vec());
        inboud_tx.send(unknown_packet).unwrap();
        signal_rx.next().await.unwrap().unwrap_err();

        let supported_packet = InPacket(b"new_state".to_vec());
        inboud_tx.send(supported_packet.clone()).unwrap();
        let signal = signal_rx.next().await.unwrap().unwrap();
        assert_eq!(
            Signal {
                id: StateId::new_with_u128(TestKind, 1),
                input: TestInput::Packet(supported_packet),
            },
            signal,
        );
    }
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn rex_builder() {
        // TODO test outbound_rx
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel::<OutPacket>();

        let (inbound_tx, builder) = RexBuilder::new_connected(outbound_tx);
        let ctx = builder
            .with_ingress_adapter(vec![Box::new(TestStateRouter)], TestTopic::Ingress)
            .build();

        let signal_rx = ctx.signal_queue.stream().timeout(Duration::from_millis(2));
        tokio::pin!(signal_rx);

        // An unknown packet should be unrouteable
        let unknown_packet = InPacket(b"unknown_packet".to_vec());
        inbound_tx.send(unknown_packet).unwrap();
        signal_rx.next().await.unwrap().unwrap_err();

        let supported_packet = InPacket(b"new_state".to_vec());
        inbound_tx.send(supported_packet.clone()).unwrap();
        let signal = signal_rx.next().await.unwrap().unwrap();
        assert_eq!(
            Signal {
                id: StateId::new_with_u128(TestKind, 1),
                input: TestInput::Packet(supported_packet),
            },
            signal,
        );
    }
}
