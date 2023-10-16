use std::{collections::HashMap, fmt, sync::Arc};

use bigerror::{Report, ResultIntoContext};
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{debug, error, trace, warn};

use crate::{
    manager::{HashKind, Signal, SignalQueue},
    notification::{GetTopic, Message, Notification, NotificationProcessor, Topic},
    queue::StreamableDeque,
    StateId, StateMachineError,
};
use bigerror::{ConversionError, LogError};

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

type BoxedStateRouter<K, In> = Box<dyn StateRouter<K, Inbound = In>>;

/// top level router that holds all [`Kind`] indexed [`StateRouter`]s
pub struct PacketRouter<K, In>(HashMap<K, BoxedStateRouter<K, In>>)
where
    K: HashKind;

impl<'a, K, P: 'a> PacketRouter<K, P>
where
    K: HashKind + TryFrom<&'a P, Error = Report<ConversionError>>,
{
    fn get_id(&self, packet: &'a P) -> Result<Option<StateId<K>>, Report<StateMachineError>> {
        let kind = K::try_from(packet).into_ctx()?;
        let Some(router) = self.0.get(&kind) else {
            return Ok(None);
        };
        router.get_id(packet)
    }
}

/// Represents a bidirectional network connection
pub struct IngressAdapter<K, SI, In, Out, T, const U: usize>
where
    K: HashKind,
    SI: Send + Sync + fmt::Debug,
    In: Send + Sync + fmt::Debug,
    Out: Send + Sync + fmt::Debug,
{
    outbound_tx: UnboundedSender<Out>,
    signal_queue: Arc<StreamableDeque<Signal<K, SI>>>,
    router: Arc<PacketRouter<K, In>>,
    // Option<P> is used to guard against
    // an invalid <IngressAdapter as NotificationProcessor>::init (one where
    // IngressAdapter::init_packet_processor was not called)
    inbound_tx: Option<UnboundedSender<In>>,
    topics: [Topic<T>; U],
}

impl<K, SI, In, Out, T, const U: usize> IngressAdapter<K, SI, In, Out, T, U>
where
    for<'a> K: HashKind + TryFrom<&'a In, Error = Report<ConversionError>>,
    SI: Send + Sync + fmt::Debug + TryFrom<In, Error = Report<ConversionError>> + 'static,
    In: Send + Sync + fmt::Debug + 'static,
    Out: Send + Sync + fmt::Debug + 'static,
{
    pub fn new<const N: usize>(
        signal_queue: Arc<SignalQueue<K, SI>>,
        outbound_tx: UnboundedSender<Out>,
        state_routers: [BoxedStateRouter<K, In>; N],
        topics: [Topic<T>; U],
    ) -> Self {
        let state_routers: HashMap<K, BoxedStateRouter<K, In>> = state_routers
            .into_iter()
            .map(|r| (r.get_kind(), r))
            .collect();

        Self {
            signal_queue,
            outbound_tx,
            router: Arc::new(PacketRouter(state_routers)),
            inbound_tx: None,
            topics,
        }
    }

    // This needs to be a precursor step for now
    // TODO change to builder pattern
    pub fn init_packet_processor(&mut self) -> UnboundedSender<In> {
        let router = self.router.clone();
        let signal_queue = self.signal_queue.clone();
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel::<In>();
        let _nw_handle = tokio::spawn(async move {
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
                SI::try_from(packet)
                    .map(|input| {
                        signal_queue.push_back(Signal { id, input });
                    })
                    .log_attached_err("ia::processors from packet failed");
            }
        });
        self.inbound_tx = Some(packet_tx.clone());

        packet_tx
    }

    pub fn init_notification_processor<N>(&self) -> UnboundedSender<N>
    where
        N: TryInto<Out, Error = Report<ConversionError>> + Send + 'static + fmt::Debug,
    {
        debug!("starting IngressAdapter notification_tx");
        self.inbound_tx
            .as_ref()
            .expect("IngressAdapter did not initialize packet_tx!");

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<N>();
        let outbound_tx = self.outbound_tx.clone();

        let _notification_handle = tokio::spawn(async move {
            debug!(target: "state_machine", spawning = "IngressAdapter.notification_tx");
            while let Some(notification) = input_rx.recv().await {
                notification
                    .try_into()
                    .map(|packet| {
                        trace!("sending packet");
                        outbound_tx.send(packet).log_err();
                    })
                    .log_attached_err("Invalid input");
            }
        });

        input_tx
    }
}

impl<K, T, M, SI, In, Out, const U: usize> NotificationProcessor<T, Notification<K, M>>
    for IngressAdapter<K, SI, In, Out, T, U>
where
    for<'a> K: HashKind + TryFrom<&'a In, Error = Report<ConversionError>>,
    SI: Send + Sync + fmt::Debug + TryFrom<In, Error = Report<ConversionError>> + 'static,
    M: Message + GetTopic<T>,
    T: Send + Sync + 'static,
    In: Send + Sync + fmt::Debug + 'static,
    Out: Send
        + Sync
        + fmt::Debug
        + TryFrom<Notification<K, M>, Error = Report<ConversionError>>
        + 'static,
{
    fn init(&self) -> UnboundedSender<Notification<K, M>> {
        self.init_notification_processor()
    }

    fn get_topics(&self) -> &[Topic<T>] {
        &self.topics
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio_stream::StreamExt;

    use super::*;
    use crate::{notification::NotificationManager, test_support::*, StateId, TestDefault};

    type TestIngressAdapter = (
        IngressAdapter<TestKind, TestInput, InPacket, OutPacket, TestTopic, 1>,
        UnboundedReceiver<OutPacket>,
    );

    impl TestDefault for TestIngressAdapter {
        fn test_default() -> Self {
            let signal_queue = Arc::new(SignalQueue::new());
            let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();

            let nw_adapter = IngressAdapter::new(
                signal_queue,
                outbound_tx,
                [Box::new(TestStateRouter)],
                [Topic::Message(TestTopic::Ingress)],
            );
            (nw_adapter, outbound_rx)
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn route_to_network() {
        let (mut nw_adapter, mut network_rx) = TestIngressAdapter::test_default();
        let _inbound_tx = nw_adapter.init_packet_processor();

        let notification_manager: NotificationManager<TestTopic, Notification<_, TestMsg>> =
            NotificationManager::new([&nw_adapter]);
        let notification_tx = notification_manager.init();

        let unknown_packet = OutPacket(b"unknown_packet".to_vec());

        // Any packet should get to the GatewayClient since routing rules
        // are only used at the ingress of the state machine
        notification_tx.send(unknown_packet.clone().into()).unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(Ok(unknown_packet), network_rx.try_recv());

        let unsupported_packet = OutPacket(b"unsupported_packet".to_vec());

        notification_tx
            .send(unsupported_packet.clone().into())
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

        let notification_manager: NotificationManager<TestTopic, Notification<_, TestMsg>> =
            NotificationManager::new([&nw_adapter]);
        let _notification_tx = notification_manager.init();

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
