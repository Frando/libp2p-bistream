use std::{
    collections::{HashMap, VecDeque},
    fmt,
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use libp2p::{
    core::{muxing::SubstreamBox, upgrade::ReadyUpgrade, Negotiated},
    swarm::{
        derive_prelude::{ConnectionId, FromSwarm},
        dial_opts::{DialOpts, PeerCondition},
        handler::{ConnectionEvent, FullyNegotiatedInbound, FullyNegotiatedOutbound},
        ConnectionHandler, ConnectionHandlerEvent, IntoConnectionHandler, KeepAlive,
        NetworkBehaviour, NetworkBehaviourAction, NotifyHandler, PollParameters, SubstreamProtocol,
    },
    PeerId,
};
use tokio::{sync::oneshot, time::Sleep};
use tracing::{error, warn};

use crate::stream::{BiStream, StreamOrigin};

pub const PROTOCOL_NAME: &'static str = "/bistream/0.1.0";

// TODO: Make configurable.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Event {
    IncomingStream(BiStream),
}

#[derive(Debug)]
pub enum OpenError {
    DialFailed(String),
    Timeout,
}

impl std::error::Error for OpenError {}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DialFailed(reason) => write!(f, "Failed to dial peer: {reason}"),
            Self::Timeout => write!(f, "Request for stream timed out"),
        }
    }
}

#[derive(Default, Debug)]
pub struct Behaviour {
    protocol_name: Option<String>,
    events: VecDeque<NetworkBehaviourAction<Event, HandlerPrototype>>,
    pending_open: HashMap<PeerId, VecDeque<oneshot::Sender<Result<BiStream, OpenError>>>>,
    pending_accept: HashMap<PeerId, VecDeque<oneshot::Sender<Result<BiStream, OpenError>>>>,
    // peers: HashMap<PeerId, Peer>,
    peers_dialing: HashMap<PeerId, PeerDialing>,
    peers_connected: HashMap<PeerId, PeerConnected>,
}

#[derive(Debug)]
struct PeerDialing {
    timeout: Pin<Box<Sleep>>,
}

impl PeerDialing {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout: Box::pin(tokio::time::sleep(timeout)),
        }
    }
}

#[derive(Debug)]
struct PeerConnected {
    connection_id: ConnectionId,
    streams: usize,
}

impl Behaviour {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_protocol_name(name: String) -> Self {
        Self {
            protocol_name: Some(name),
            ..Default::default()
        }
    }

    pub fn accept_bi_from_peer(
        &mut self,
        peer_id: &PeerId,
        reply: oneshot::Sender<Result<BiStream, OpenError>>,
    ) {
        self.pending_accept
            .entry(peer_id.clone())
            .or_default()
            .push_back(reply)
    }

    pub fn open_bi(
        &mut self,
        peer_id: &PeerId,
        reply: oneshot::Sender<Result<BiStream, OpenError>>,
    ) {
        self.pending_open
            .entry(peer_id.clone())
            .or_default()
            .push_back(reply);

        // For connected peers: Open a new stream on the existing handler.
        if let Some(peer) = self.peers_connected.get(peer_id) {
            eprintln!("open bi HAS");
            self.events
                .push_back(NetworkBehaviourAction::NotifyHandler {
                    peer_id: peer_id.clone(),
                    handler: NotifyHandler::One(peer.connection_id),
                    event: InEvent::OpenOutbound,
                });
        }
        // For unconnected peers: Try to dial the peer.
        else {
            eprintln!("open bi HAS NOT");
            self.peers_dialing
                .insert(peer_id.clone(), PeerDialing::new(DIAL_TIMEOUT));
            self.events.push_back(NetworkBehaviourAction::Dial {
                opts: DialOpts::peer_id(peer_id.clone())
                    .condition(PeerCondition::Always)
                    .build(),
                handler: HandlerPrototype::new(self.protocol_name.clone()),
            });
        }
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = HandlerPrototype;
    type OutEvent = Event;
    fn new_handler(&mut self) -> Self::ConnectionHandler {
        HandlerPrototype::new(self.protocol_name.clone())
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionEstablished(conn) => {
                eprintln!(
                    "ESTABLISH peer_id {} other_established {} failed_addresses {:?}",
                    conn.peer_id, conn.other_established, conn.failed_addresses
                );

                if let Some(_) = self.peers_dialing.remove(&conn.peer_id) {
                    self.events
                        .push_back(NetworkBehaviourAction::NotifyHandler {
                            peer_id: conn.peer_id.clone(),
                            handler: NotifyHandler::One(conn.connection_id),
                            event: InEvent::OpenOutbound,
                        });
                }

                self.peers_connected
                    .entry(conn.peer_id)
                    .or_insert_with(|| PeerConnected {
                        connection_id: conn.connection_id,
                        streams: 0,
                    });
            }
            FromSwarm::DialFailure(_failure) => {
                // We don't care for failed dials here, as there can be many of them if only some
                // of the dials to a peer fail and others succeed.
                // In poll() we check the dial timeouts instead.

                // eprintln!("FAILRURE {:?}", failure.peer_id);
                // if let Some(peer_id) = failure.peer_id {
                //     if let Some(replies) = self.pending_open.remove(&peer_id) {
                //         for reply in replies.into_iter() {
                //             let _ = reply.send(Err(OpenError::DialFailed(format!("{}", failure.error))));
                //         }
                //     }
                // }
            }
            _ => {}
        }
    }

    fn on_connection_handler_event(
        &mut self,
        _peer_id: PeerId,
        _connection: ConnectionId,
        event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        eprintln!("on_connection_event {:?}", event);
        match event {
            OutEvent::OutboundStream(stream) => {
                eprintln!("open outbound! {stream:?}");
                eprintln!("open outbound! connected {:?}", self.peers_connected);
                eprintln!("open outbound! pending {:?}", self.pending_open);
                if let Some(peer) = self.peers_connected.get_mut(&stream.peer_id()) {
                    peer.streams += 1;
                    match self
                        .pending_open
                        .get_mut(&stream.peer_id())
                        .map(|x| x.pop_front())
                        .flatten()
                    {
                        Some(reply) => {
                            if let Err(_stream) = reply.send(Ok(stream)) {
                                warn!("Reply channel for OpenBi dropped");
                            }
                        }
                        _ => error!("Bad outbound stream - no reply channel registered"),
                    }
                }
            }
            OutEvent::InboundStream(stream) => {
                // if let Some(peer) = self.peers_connected.get_mut(&stream.peer_id()) {
                //     peer.streams += 1;
                match self
                    .pending_accept
                    .get_mut(&stream.peer_id())
                    .map(|x| x.pop_front())
                    .flatten()
                {
                    Some(reply) => {
                        if let Err(_stream) = reply.send(Ok(stream)) {
                            // TODO: when does this happen / what to do?
                            warn!("Reply channel for OpenBi dropped");
                        }
                    }
                    None => {
                        self.events.push_back(NetworkBehaviourAction::GenerateEvent(
                            Event::IncomingStream(stream),
                        ));
                    }
                }
            } // }
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Event, HandlerPrototype>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        // Check timeouts of dialing peers
        self.peers_dialing
            .retain(|peer_id, peer| match peer.timeout.as_mut().poll(cx) {
                Poll::Pending => true,
                Poll::Ready(_) => {
                    if let Some(replies) = self.pending_open.remove(&peer_id) {
                        for reply in replies.into_iter() {
                            let _ = reply
                                .send(Err(OpenError::DialFailed("Dialing timeout reached".into())));
                        }
                    }
                    false
                }
            });

        Poll::Pending
    }
}

type ProtocolTy = ReadyUpgrade<String>;

#[derive(Debug)]
pub enum OutEvent {
    InboundStream(BiStream),
    OutboundStream(BiStream),
}

#[derive(Debug)]
pub enum InEvent {
    OpenOutbound,
}

#[derive(Debug)]
pub struct HandlerPrototype {
    protocol_name: String,
}

impl HandlerPrototype {
    pub fn new(protocol_name: Option<String>) -> Self {
        HandlerPrototype {
            protocol_name: protocol_name.unwrap_or_else(|| PROTOCOL_NAME.into()),
        }
    }
}

impl IntoConnectionHandler for HandlerPrototype {
    type Handler = Handler;
    fn into_handler(
        self,
        remote_peer_id: &PeerId,
        _connected_point: &libp2p::core::ConnectedPoint,
    ) -> Self::Handler {
        eprintln!("proto to handler! for {remote_peer_id}");
        Handler {
            protocol_name: self.protocol_name,
            peer_id: remote_peer_id.clone(),
            events: VecDeque::new(),
            inbound_stream_id: Arc::new(0.into()),
            outbound_stream_id: 0,
        }
    }
    fn inbound_protocol(&self) -> <Self::Handler as ConnectionHandler>::InboundProtocol {
        eprintln!("inbound protocol!");
        ReadyUpgrade::new(self.protocol_name.clone())
    }
}

#[derive(Debug)]
pub struct Handler {
    protocol_name: String,
    events: VecDeque<ConnectionHandlerEvent<ProtocolTy, OpenInfo, OutEvent, io::Error>>,
    peer_id: PeerId,
    inbound_stream_id: Arc<AtomicU64>,
    outbound_stream_id: u64,
}

impl Handler {
    fn emit_stream(
        &mut self,
        proto: Negotiated<SubstreamBox>,
        origin: StreamOrigin,
        info: OpenInfo,
    ) {
        let stream_id = info.stream_id;
        let stream = BiStream {
            stream_id,
            peer_id: self.peer_id,
            inner: proto,
            origin,
        };
        let event = match stream.origin() {
            StreamOrigin::Outbound => OutEvent::OutboundStream(stream),
            StreamOrigin::Inbound => OutEvent::InboundStream(stream),
        };
        eprintln!("EMIT {event:?}");
        self.events.push_back(ConnectionHandlerEvent::Custom(event));
    }
}

type StreamId = u64;

#[derive(Default, Debug)]
pub struct OpenInfo {
    stream_id: StreamId,
}

impl Drop for Handler {
    fn drop(&mut self) {
        eprintln!("handler drop");
    }
}

impl ConnectionHandler for Handler {
    type InEvent = InEvent;
    type OutEvent = OutEvent;
    type Error = io::Error;
    type InboundProtocol = ProtocolTy;
    type OutboundProtocol = ProtocolTy;
    type OutboundOpenInfo = OpenInfo;
    type InboundOpenInfo = OpenInfo;

    fn listen_protocol(&self) -> SubstreamProtocol<ProtocolTy, OpenInfo> {
        eprintln!("listen protocol!");
        let stream_id = self.inbound_stream_id.fetch_add(1, Ordering::Relaxed);
        SubstreamProtocol::new(
            ReadyUpgrade::new(self.protocol_name.clone()),
            OpenInfo { stream_id },
        )
    }

    fn on_behaviour_event(&mut self, event: InEvent) {
        eprintln!("on behaviour event {event:?}");
        match event {
            InEvent::OpenOutbound => {
                let stream_id = {
                    self.outbound_stream_id += 1;
                    self.outbound_stream_id
                };
                let protocol = SubstreamProtocol::new(
                    ReadyUpgrade::new(self.protocol_name.clone()),
                    OpenInfo { stream_id },
                );
                self.events
                    .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol });
            }
        }
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<ConnectionHandlerEvent<ProtocolTy, OpenInfo, OutEvent, Self::Error>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }
        Poll::Pending
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        eprintln!("handler on_connection_event");
        match event {
            ConnectionEvent::FullyNegotiatedInbound(FullyNegotiatedInbound { protocol, info }) => {
                self.emit_stream(protocol, StreamOrigin::Inbound, info);
            }
            ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                protocol,
                info,
            }) => {
                self.emit_stream(protocol, StreamOrigin::Outbound, info);
            }
            ConnectionEvent::DialUpgradeError(_)
            | ConnectionEvent::AddressChange(_)
            | ConnectionEvent::ListenUpgradeError(_) => {}
        }
    }
}
