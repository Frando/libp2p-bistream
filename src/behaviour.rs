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
    PeerId, Multiaddr,
};
use tokio::time::Sleep;

use crate::stream::{BiStream, StreamOrigin};

pub const PROTOCOL_NAME: &str = "/bistream/0.1.0";

// TODO: Make configurable.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Event {
    StreamReady(BiStream),
    OpeningFailed {
        peer_id: PeerId,
        stream_id: StreamId,
        reason: OpenError,
    },
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
    peers_dialing: HashMap<PeerId, PeerDialing>,
    peers_connected: HashMap<PeerId, PeerConnected>,
}

#[derive(Debug)]
struct PeerDialing {
    timeout: Pin<Box<Sleep>>,
    requested_outbound_streams: u64,
}

impl PeerDialing {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout: Box::pin(tokio::time::sleep(timeout)),
            requested_outbound_streams: 0,
        }
    }
}

#[derive(Debug)]
struct PeerConnected {
    connection_id: ConnectionId,
    next_outbound_id: u64,
}

impl PeerConnected {
    pub fn new(connection_id: ConnectionId) -> Self {
        Self {
            connection_id,
            next_outbound_id: 1,
        }
    }
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

    // pub fn accept_bi_from_peer(
    //     &mut self,
    //     peer_id: &PeerId,
    //     reply: oneshot::Sender<Result<BiStream, OpenError>>,
    // ) {
    //     self.pending_accept
    //         .entry(peer_id.clone())
    //         .or_default()
    //         .push_back(reply)
    // }
    
    pub fn open_bi_with_addrs(&mut self, peer_id: &PeerId, addrs: Vec<Multiaddr>) -> u64 {
        let dial_opts = DialOpts::peer_id(*peer_id)
            .addresses(addrs)
            .condition(PeerCondition::Always)
            .build();
        self.open_bi_with_dial_opts(dial_opts)
    }

    pub fn open_bi(&mut self, peer_id: &PeerId) -> u64 {
        let dial_opts = DialOpts::peer_id(*peer_id)
            .condition(PeerCondition::Always)
            .build();
        self.open_bi_with_dial_opts(dial_opts)
    }

    fn open_bi_with_dial_opts(&mut self, dial_opts: DialOpts) -> u64 {
        let peer_id = dial_opts.get_peer_id().unwrap();

        // For connected peers: Open a new stream on the existing handler.
        let stream_id = if let Some(peer) = self.peers_connected.get_mut(&peer_id) {
            let stream_id = peer.next_outbound_id;
            peer.next_outbound_id += 1;
            self.events
                .push_back(NetworkBehaviourAction::NotifyHandler {
                    peer_id,
                    handler: NotifyHandler::One(peer.connection_id),
                    event: InEvent::OpenOutbound(stream_id),
                });
            stream_id
        }
        // For unconnected peers: Try to dial the peer.
        else {
            // Start dialing the peer if not yet dialing.
            if let std::collections::hash_map::Entry::Vacant(e) = self.peers_dialing.entry(peer_id) {
                e.insert(PeerDialing::new(DIAL_TIMEOUT));
                self.events.push_back(NetworkBehaviourAction::Dial {
                    opts: dial_opts,
                    handler: HandlerPrototype::new(self.protocol_name.clone()),
                });
            }
            let peer = self.peers_dialing.get_mut(&peer_id).unwrap();
            peer.requested_outbound_streams += 1;
            peer.requested_outbound_streams
        };
        stream_id
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
                let peer = self
                    .peers_connected
                    .entry(conn.peer_id)
                    .or_insert_with(|| PeerConnected::new(conn.connection_id));

                if let Some(dialing_peer) = self.peers_dialing.remove(&conn.peer_id) {
                    for stream_id in 1..=dialing_peer.requested_outbound_streams {
                        self.events
                            .push_back(NetworkBehaviourAction::NotifyHandler {
                                peer_id: conn.peer_id,
                                handler: NotifyHandler::One(conn.connection_id),
                                event: InEvent::OpenOutbound(stream_id),
                            });
                    }
                    peer.next_outbound_id += dialing_peer.requested_outbound_streams + 1;
                }
            }
            FromSwarm::ConnectionClosed(conn) => {
                // TODO: Break streams and emit errors?
                let mut remove = false;
                if let Some(peer) = self.peers_connected.get(&conn.peer_id) {
                    if peer.connection_id == conn.connection_id {
                        remove = true;
                    }
                }
                if remove {
                    self.peers_connected.remove(&conn.peer_id);
                }
            }
            FromSwarm::DialFailure(_failure) => {
                // We don't care for failed dials here, as there can be many of them if only some
                // of the dials to a peer fail and others succeed.
                // In poll() we check the dial timeouts instead.
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
        match event {
            OutEvent::OutboundStream(stream) => {
                self.events
                    .push_back(NetworkBehaviourAction::GenerateEvent(Event::StreamReady(
                        stream,
                    )));
            }
            OutEvent::InboundStream(stream) => {
                self.events
                    .push_back(NetworkBehaviourAction::GenerateEvent(Event::StreamReady(
                        stream,
                    )));
            }
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
                    for stream_id in 1..=peer.requested_outbound_streams {
                        self.events.push_back(NetworkBehaviourAction::GenerateEvent(
                            Event::OpeningFailed {
                                peer_id: *peer_id,
                                stream_id,
                                reason: OpenError::DialFailed("Dialing timeout reached".into()),
                            },
                        ));
                    }
                    false
                }
            });

        if let Some(event) = self.events.pop_front() {
            Poll::Ready(event)
        } else {
            Poll::Pending
        }

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
    OpenOutbound(u64),
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
        Handler {
            protocol_name: self.protocol_name,
            peer_id: *remote_peer_id,
            events: VecDeque::new(),
            next_inbound_stream_id: Arc::new(1.into()),
        }
    }
    fn inbound_protocol(&self) -> <Self::Handler as ConnectionHandler>::InboundProtocol {
        ReadyUpgrade::new(self.protocol_name.clone())
    }
}

#[derive(Debug)]
pub struct Handler {
    protocol_name: String,
    events: VecDeque<ConnectionHandlerEvent<ProtocolTy, OpenInfo, OutEvent, io::Error>>,
    peer_id: PeerId,
    next_inbound_stream_id: Arc<AtomicU64>,
}

impl Handler {
    fn emit_stream(
        &mut self,
        proto: Negotiated<SubstreamBox>,
        origin: StreamOrigin,
        info: OpenInfo,
    ) {
        let stream = BiStream {
            stream_id: info.stream_id,
            peer_id: self.peer_id,
            inner: proto,
            origin,
        };
        let event = match stream.origin() {
            StreamOrigin::Outbound => OutEvent::OutboundStream(stream),
            StreamOrigin::Inbound => OutEvent::InboundStream(stream),
        };
        self.events.push_back(ConnectionHandlerEvent::Custom(event));
    }
}

type StreamId = u64;

#[derive(Default, Debug)]
pub struct OpenInfo {
    stream_id: StreamId,
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
        let stream_id = self.next_inbound_stream_id.fetch_add(1, Ordering::Relaxed);
        SubstreamProtocol::new(
            ReadyUpgrade::new(self.protocol_name.clone()),
            OpenInfo { stream_id },
        )
    }

    fn on_behaviour_event(&mut self, event: InEvent) {
        match event {
            InEvent::OpenOutbound(stream_id) => {
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
