use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
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
use tracing::{error, warn};

mod stream;
pub use stream::{BiStream, StreamOrigin};
use tokio::sync::oneshot;

pub const PROTOCOL_NAME: &'static str = "/p2pstream/bistream/0.0.1";

#[derive(Debug)]
pub enum Event {
    IncomingStream(BiStream),
}

#[derive(Default, Debug)]
pub struct Behaviour {
    protocol_name: Option<String>,
    events: VecDeque<NetworkBehaviourAction<Event, Handler>>,
    conns: HashMap<PeerId, ConnectionId>,
    pending_open: HashMap<PeerId, VecDeque<oneshot::Sender<anyhow::Result<BiStream>>>>,
    pending_accept: HashMap<PeerId, VecDeque<oneshot::Sender<anyhow::Result<BiStream>>>>,
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
        reply: oneshot::Sender<anyhow::Result<BiStream>>,
    ) {
        self.pending_accept
            .entry(peer_id.clone())
            .or_default()
            .push_back(reply)
    }

    pub fn open_bi(&mut self, peer_id: &PeerId, reply: oneshot::Sender<anyhow::Result<BiStream>>) {
        self.pending_open
            .entry(peer_id.clone())
            .or_default()
            .push_back(reply);
        if let Some(conn_id) = self.conns.get(peer_id) {
            self.events
                .push_back(NetworkBehaviourAction::NotifyHandler {
                    peer_id: peer_id.clone(),
                    handler: NotifyHandler::One(*conn_id),
                    event: InEvent::OpenOutbound
                });
        } else {
            self.events.push_back(NetworkBehaviourAction::Dial {
                opts: DialOpts::peer_id(peer_id.clone())
                    .condition(PeerCondition::Always)
                    .build(),
                handler: Handler::new(self.protocol_name.clone()),
            });
        }
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = Handler;
    type OutEvent = Event;
    fn new_handler(&mut self) -> Self::ConnectionHandler {
        Handler::new(self.protocol_name.clone())
    }
    fn on_connection_handler_event(
        &mut self,
        _peer_id: PeerId,
        _connection: ConnectionId,
        event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        match event {
            OutEvent::OutboundStream(stream) => {
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
                    _ =>  error!("Bad outbound stream - no reply channel registered")
                }
            }
            OutEvent::InboundStream(stream) => {
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
            }
        }
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionEstablished(conn) => {
                self.conns.insert(conn.peer_id, conn.connection_id);
                self.events
                    .push_back(NetworkBehaviourAction::NotifyHandler {
                        peer_id: conn.peer_id,
                        handler: NotifyHandler::One(conn.connection_id),
                        event: InEvent::ConnectionEstablished {
                            peer_id: conn.peer_id,
                        },
                    });
                // This would auto-open a new channel if we dialed.
                // if conn.endpoint.is_dialer() {
                //     self.events
                //         .push_back(NetworkBehaviourAction::NotifyHandler {
                //             peer_id: conn.peer_id,
                //             handler: NotifyHandler::One(conn.connection_id),
                //             event: InEvent::OpenOutbound,
                //         });
                // }
            }
            FromSwarm::DialFailure(failure) => {
                let err = format!("Failed to dial peer: {}", failure.error);
                if let Some(peer_id) = failure.peer_id {
                    if let Some(replies) = self.pending_open.remove(&peer_id) {
                        for reply in replies.into_iter() {
                            let _ = reply.send(Err(anyhow::anyhow!(err.clone())));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }
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
    ConnectionEstablished { peer_id: PeerId },
    OpenOutbound,
}

#[derive(Default, Debug)]
pub struct Handler {
    protocol_name: String,
    events: VecDeque<ConnectionHandlerEvent<ProtocolTy, OpenInfo, OutEvent, io::Error>>,
    peer_id: Option<PeerId>,
    inbound_stream_id: Arc<AtomicU64>,
    outbound_stream_id: u64,
}

impl Handler {
    pub fn new(protocol_name: Option<String>) -> Self {
        let protocol_name = protocol_name.unwrap_or_else(|| PROTOCOL_NAME.to_string());
        Self {
            protocol_name,
            ..Default::default()
        }
    }

    fn emit_stream(
        &mut self,
        proto: Negotiated<SubstreamBox>,
        origin: StreamOrigin,
        info: OpenInfo,
    ) {
        let peer_id = self
            .peer_id
            .expect("May not emit streams before connection is established.");
        let stream_id = info.stream_id;
        let stream = BiStream {
            stream_id,
            peer_id,
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
        let stream_id = self.inbound_stream_id.fetch_add(1, Ordering::Relaxed);
        let open_info = OpenInfo { stream_id };
        SubstreamProtocol::new(ReadyUpgrade::new(self.protocol_name.clone()), open_info)
    }

    fn on_behaviour_event(&mut self, event: InEvent) {
        match event {
            InEvent::ConnectionEstablished { peer_id } => {
                self.peer_id = Some(peer_id);
            }
            InEvent::OpenOutbound => {
                self.outbound_stream_id += 1;
                let stream_id = self.outbound_stream_id;
                let open_info = OpenInfo { stream_id };
                let protocol = SubstreamProtocol::new(ReadyUpgrade::new(self.protocol_name.clone()), open_info);
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
