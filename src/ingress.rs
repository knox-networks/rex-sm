use std::{collections::HashMap, fmt, sync::Arc};

use bigerror::{ConversionError, IntoContext, LogError, Report};
use tokio::{
    sync::{mpsc, mpsc::UnboundedSender},
    task::JoinSet,
};
use tracing::{debug, error, trace, warn, Instrument};

use crate::{
    manager::{HashKind, Signal, SignalQueue},
    notification::{Notification, NotificationProcessor, RexMessage},
    queue::StreamableDeque,
    Rex, StateId, StateMachineError,
};

pub trait StateRouter<K>: Send + Sync
where
    K: HashKind,
{
    type Inbound;
    fn get_id(
        &self,
        input: &Self::Inbound,
    ) -> Result<Option<StateId<K>>, Report<StateMachineError>>;
    fn get_kind(&self) -> K;
}

pub type BoxedStateRouter<K, In> = Box<dyn StateRouter<K, Inbound = In>>;

/// top level router that holds all [`Kind`] indexed [`StateRouter`]s
pub struct PacketRouter<K, In>(HashMap<K, BoxedStateRouter<K, In>>)
where
    K: HashKind;

impl<K, P> PacketRouter<K, P>
where
    for<'a> K: HashKind + TryFrom<&'a P, Error = Report<ConversionError>>,
{
    fn get_id(&self, packet: &P) -> Result<Option<StateId<K>>, Report<StateMachineError>> {
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
    outbound_tx: UnboundedSender<Out>,
    signal_queue: Arc<StreamableDeque<Signal<K>>>,
    router: Arc<PacketRouter<K, In>>,
    // Option<P> is used to guard against
    // an invalid <IngressAdapter as NotificationProcessor>::init (one where
    // IngressAdapter::init_packet_processor was not called)
    inbound_tx: Option<UnboundedSender<In>>,
    topic: <K::Message as RexMessage>::Topic,
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
    pub fn new(
        signal_queue: Arc<SignalQueue<K>>,
        outbound_tx: UnboundedSender<Out>,
        state_routers: Vec<BoxedStateRouter<K, In>>,
        topic: impl Into<<K::Message as RexMessage>::Topic>,
    ) -> Self {
        let mut router_map: HashMap<K, BoxedStateRouter<K, In>> = HashMap::new();
        for router in state_routers {
            if let Some(old_router) = router_map.insert(router.get_kind(), router) {
                panic!(
                    "Found multiple routers for kind: {:?}",
                    old_router.get_kind()
                );
            }
        }

        Self {
            signal_queue,
            outbound_tx,
            router: Arc::new(PacketRouter(router_map)),
            inbound_tx: None,
            topic: topic.into(),
        }
    }

    pub fn init_packet_processor(&mut self) -> UnboundedSender<In> {
        let mut join_set = JoinSet::new();
        let tx = self.init_packet_processor_with_handle(&mut join_set);
        join_set.detach_all();
        tx
    }

    // This needs to be a precursor step for now
    // TODO change to builder pattern
    pub fn init_packet_processor_with_handle(
        &mut self,
        join_set: &mut JoinSet<()>,
    ) -> UnboundedSender<In> {
        let router = self.router.clone();
        let signal_queue = self.signal_queue.clone();
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel::<In>();
        let _nw_handle = join_set.spawn(
            async move {
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
            .in_current_span(),
        );
        self.inbound_tx = Some(packet_tx.clone());

        packet_tx
    }

    pub fn init_notification_processor(&self) -> UnboundedSender<Notification<K::Message>> {
        let mut join_set = JoinSet::new();
        let tx = self.init_notification_processor_with_handle(&mut join_set);
        join_set.detach_all();
        tx
    }

    pub fn init_notification_processor_with_handle(
        &self,
        join_set: &mut JoinSet<()>,
    ) -> UnboundedSender<Notification<K::Message>> {
        debug!("starting IngressAdapter notification_tx");
        self.inbound_tx
            .as_ref()
            .expect("IngressAdapter did not initialize packet_tx!");

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
    fn init(&self, join_set: &mut JoinSet<()>) -> UnboundedSender<Notification<K::Message>> {
        self.init_notification_processor_with_handle(join_set)
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
    use crate::{notification::NotificationManager, test_support::*, StateId};

    type TestIngressAdapter = (
        IngressAdapter<TestKind, InPacket, OutPacket>,
        UnboundedReceiver<OutPacket>,
    );

    impl TestDefault for TestIngressAdapter {
        fn test_default() -> Self {
            let signal_queue = Arc::new(SignalQueue::new());
            let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();

            let nw_adapter = IngressAdapter::new(
                signal_queue,
                outbound_tx,
                vec![Box::new(TestStateRouter)],
                TestTopic::Ingress,
            );
            (nw_adapter, outbound_rx)
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn route_to_network() {
        let (mut nw_adapter, mut network_rx) = TestIngressAdapter::test_default();
        let _inbound_tx = nw_adapter.init_packet_processor();
        let mut join_set = JoinSet::new();

        let notification_manager: NotificationManager<TestMsg> =
            NotificationManager::new(&[&nw_adapter], &mut join_set);
        let notification_tx = notification_manager.init(&mut join_set);

        let unknown_packet = OutPacket(b"unknown_packet".to_vec());

        // Any packet should get to the GatewayClient since routing rules
        // are only used at the ingress of the state machine
        notification_tx
            .send(Notification(unknown_packet.clone().into()))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(Ok(unknown_packet), network_rx.try_recv());

        let unsupported_packet = OutPacket(b"unsupported_packet".to_vec());

        notification_tx
            .send(Notification(unsupported_packet.clone().into()))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(Ok(unsupported_packet), network_rx.try_recv());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn route_from_network() {
        let (mut nw_adapter, _outbound_rx) = TestIngressAdapter::test_default();
        let signal_queue = nw_adapter.signal_queue.clone();
        let signal_rx = signal_queue.stream().timeout(Duration::from_millis(2));
        tokio::pin!(signal_rx);

        let inboud_tx = nw_adapter.init_packet_processor();
        let mut join_set = JoinSet::new();

        let notification_manager: NotificationManager<TestMsg> =
            NotificationManager::new(&[&nw_adapter], &mut join_set);
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
}
