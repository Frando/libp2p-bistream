use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use anyhow::anyhow;
use futures::{Future, FutureExt, StreamExt};
use libp2p::{
    core::{
        self,
        muxing::StreamMuxerBox,
        transport::{Boxed, OrTransport},
        ConnectedPoint,
    },
    dcutr, dns, identify,
    identity::{ed25519, Keypair},
    kad, mdns, multiaddr, noise,
    swarm::{derive_prelude::ListenerId, ConnectionLimits, SwarmBuilder, SwarmEvent},
    swarm::{ConnectionHandler, Executor, IntoConnectionHandler, NetworkBehaviour},
    tcp, yamux, Multiaddr, PeerId, Swarm, Transport,
};
use tokio::sync::{
    mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{error, info, warn};

use libp2p_bistream as bistream;

use crate::behaviour::NodeBehaviour;
use crate::util::{GlobalIp, MatchProtocol};

pub const MAX_RELAYS: usize = 2;

/// Default bootstrap nodes
///
/// Based on https://github.com/ipfs/go-ipfs-config/blob/master/bootstrap_peers.go#L17.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    // "/dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    // "/dnsaddr/bootstrap.libp2p.io/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    // "/dnsaddr/bootstrap.libp2p.io/p2p/QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb",
    //
    // "/dnsaddr/bootstrap.libp2p.io/p2p/QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
    "/ip4/145.40.118.135/tcp/4001/p2p/QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
    // "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
    // mars.i.ipfs.io
];

#[derive(Debug, Default)]
pub struct Config {
    seed: Option<String>,
}

#[derive(Debug)]
pub struct ListenConfig {
    listen_addr: Multiaddr,
    relays: Option<Vec<Multiaddr>>,
}

impl ListenConfig {
    pub fn new() -> Self {
        Self {
            listen_addr: "/ip4/0.0.0.0/tcp/4001".parse().unwrap(),
            relays: None,
        }
    }

    pub fn with_relays(self, relays: Option<Vec<Multiaddr>>) -> Self {
        Self { relays, ..self }
    }
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_key_seed(self, seed: Option<String>) -> Self {
        Self { seed }
    }
}

pub struct NodeEvents {
    event_rx: Receiver<Event>,
}

impl NodeEvents {
    pub async fn next(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }
}

#[derive(Clone)]
pub struct NodeController {
    command_tx: UnboundedSender<Command>,
}
impl NodeController {
    pub fn dial(&mut self, addrs: Vec<Multiaddr>) {
        let _ = self.command_tx.send(Command::Dial(addrs));
    }

    pub fn listen(&mut self, config: ListenConfig) {
        let _ = self.command_tx.send(Command::Listen { config });
    }

    pub fn open_bi(
        &self,
        peer_id: PeerId,
    ) -> impl Future<Output = anyhow::Result<bistream::BiStream>> {
        let (tx, rx) = oneshot::channel();
        let _ = self.command_tx.send(Command::OpenBi(peer_id, tx));
        rx.map(|r| r.map_or_else(|e| Err(anyhow::anyhow!(e)), |r| r.map_err(|err| err.into())))
    }
}

#[derive(Default)]
struct BistreamManager {
    requests: HashMap<PeerId, HashMap<u64, BiStreamReply>>,
}

impl BistreamManager {
    pub fn insert_request(&mut self, peer_id: PeerId, stream_id: u64, reply: BiStreamReply) {
        let peer = self.requests.entry(peer_id).or_default();
        peer.insert(stream_id, reply);
    }

    pub fn inject_stream(&mut self, stream: bistream::BiStream) {
        if let Some(reply) = self.take_reply(&stream.peer_id(), &stream.stream_id()) {
            let _ = reply.send(Ok(stream));
        }
    }
    pub fn inject_failure(
        &mut self,
        peer_id: &PeerId,
        stream_id: u64,
        reason: bistream::OpenError,
    ) {
        if let Some(reply) = self.take_reply(peer_id, &stream_id) {
            let _ = reply.send(Err(reason));
        }
    }

    pub fn inject_event(&mut self, event: bistream::Event) -> Option<bistream::BiStream> {
        match event {
            bistream::Event::StreamReady(stream) => {
                if stream.is_dial() {
                    self.inject_stream(stream);
                    None
                } else {
                    Some(stream)
                }
            }
            bistream::Event::OpeningFailed {
                peer_id,
                stream_id,
                reason,
            } => {
                self.inject_failure(&peer_id, stream_id, reason);
                None
            }
        }
    }

    fn take_reply(&mut self, peer_id: &PeerId, stream_id: &u64) -> Option<BiStreamReply> {
        let reply = self
            .requests
            .get_mut(peer_id)
            .and_then(|peer| peer.remove(stream_id));

        if reply.is_some()
            && self
                .requests
                .get(peer_id)
                .map(|peer| peer.is_empty())
                .unwrap_or(false)
        {
            self.requests.remove(peer_id);
        }
        reply
    }
}

pub struct Node {
    swarm: Swarm<NodeBehaviour>,
    event_tx: Sender<Event>,
    command_rx: UnboundedReceiver<Command>,
    command_tx: UnboundedSender<Command>,
    listen_addrs: HashSet<Multiaddr>,
    allowed_relays: HashSet<PeerId>,
    active_relays: HashMap<PeerId, HashSet<ListenerId>>,
    pending_peers: HashMap<PeerId, VecDeque<Command>>,
    bistream_manager: BistreamManager,
}

pub type BiStreamReply = oneshot::Sender<Result<bistream::BiStream, bistream::OpenError>>;

#[derive(Debug)]
pub enum Command {
    Dial(Vec<Multiaddr>),
    Listen { config: ListenConfig },
    OpenBi(PeerId, BiStreamReply),
}

#[derive(Debug)]
pub enum Event {
    NewListenAddr(Multiaddr),
    IncomingBiStream(bistream::BiStream),
    ConnectionEstablished {
        peer_id: PeerId,
        endpoint: ConnectedPoint,
    },
    NodeError(anyhow::Error),
}

// This is as long as it gets with libp2p
pub type NodeSwarmEvent = SwarmEvent<
    // Out event of our behaviour (generated in ProcMacro) - NodeBehaviourEvent
    <NodeBehaviour as NetworkBehaviour>::OutEvent,
    // Error type of the connection handler
    <<<NodeBehaviour as NetworkBehaviour>::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::Error
>;

impl Node {
    pub fn new(config: Config) -> anyhow::Result<(Node, NodeEvents, NodeController)> {
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1024);

        let keypair = if let Some(seed) = config.seed {
            generate_ed25519_from_seed(seed.as_bytes())
        } else {
            Keypair::generate_ed25519()
        };

        let (transport, relay_client) = build_transport(&keypair);
        let peer_id = keypair.public().to_peer_id();
        let limits = ConnectionLimits::default().with_max_established_per_peer(Some(1));
        let behaviour = NodeBehaviour::new(&keypair, relay_client)?;
        let swarm = SwarmBuilder::with_executor(transport, behaviour, peer_id, Tokio)
            .connection_limits(limits)
            // .notify_handler_buffer_size(config.notify_handler_buffer_size.try_into()?)
            // .connection_event_buffer_size(config.connection_event_buffer_size)
            .dial_concurrency_factor(10u8.try_into().unwrap())
            .build();

        let mut node = Self {
            swarm,
            event_tx,
            command_rx,
            command_tx: command_tx.clone(),
            listen_addrs: HashSet::new(),
            allowed_relays: HashSet::new(),
            active_relays: HashMap::new(),
            pending_peers: HashMap::new(),
            bistream_manager: BistreamManager::default(),
        };

        // Set bootstrap addrs for DHT.
        if let Some(kad) = node.swarm.behaviour_mut().kademlia.as_mut() {
            for addr in DEFAULT_BOOTSTRAP {
                let addr: Multiaddr = addr.parse()?;
                let peer_id = PeerId::try_from_multiaddr(&addr);
                if let Some(peer_id) = peer_id {
                    kad.add_address(&peer_id, addr);
                }
            }
        }

        let controller = NodeController { command_tx };
        let events = NodeEvents { event_rx };
        Ok((node, events, controller))
    }

    pub fn start(config: Config) -> anyhow::Result<(NodeEvents, NodeController)> {
        let (mut node, events, controller) = Node::new(config)?;
        tokio::task::spawn(async move { node.run().await });
        Ok((events, controller))
    }

    async fn emit_event(&mut self, event: Event) {
        if let Err(_) = self.event_tx.send(event).await {
            warn!("Event channel is full or closed")
        }
    }

    async fn new_listen_addr(&mut self, addr: Multiaddr) -> anyhow::Result<()> {
        if !self.listen_addrs.contains(&addr) {
            self.listen_addrs.insert(addr.clone());
            self.emit_event(Event::NewListenAddr(addr)).await;
        }
        Ok(())
    }

    pub async fn run(&mut self) {
        loop {
            tokio::select! {
                swarm_event = self.swarm.next() => {
                    let swarm_event = swarm_event.expect("the swarm will never die");
                    if let Err(err) = self.handle_swarm_event(swarm_event).await {
                        error!("swarm error: {:?}", err);
                        self.emit_event(Event::NodeError(err)).await;
                        break;
                    }
                },
                command = self.command_rx.recv() => {
                    match command {
                        None => break,
                        Some(command) => {
                            if let Err(err) = self.handle_command(command).await {
                                error!("command error: {:?}", err);
                                self.emit_event(Event::NodeError(err)).await;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_command(&mut self, command: Command) -> anyhow::Result<()> {
        match command {
            Command::Listen { config } => {
                eprintln!("Local Peer ID: {}", self.swarm.local_peer_id());
                self.swarm.listen_on(config.listen_addr)?;

                if let Some(relays) = config.relays {
                    for addr in relays {
                        if let Some(peer_id) = PeerId::try_from_multiaddr(&addr) {
                            eprintln!("dial relay for {peer_id} on {addr}");
                            self.swarm.dial(addr.clone())?;
                            self.allowed_relays.insert(peer_id);
                        } else {
                            eprintln!("skip relay {addr}: no peer id");
                            warn!("Peer ID is required for relay addresses");
                        }
                    }
                }
                // Bootstrap DHT if enabled.
                // if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                //     kad.bootstrap()?;
                // }
                //
                // let bootstrap_addrs = DEFAULT_BOOTSTRAP
                //     .iter()
                //     .map(|addr| addr.parse::<Multiaddr>())
                //     .collect::<Result<Vec<Multiaddr>, _>>()?;
                // for addr in bootstrap_addrs {
                //     // eprintln!("dial {}", addr);
                //     self.swarm.dial(addr)?;
                // }
            }

            Command::Dial(addrs) => {
                for addr in addrs {
                    let _ = self.swarm.dial(addr);
                }
            }

            Command::OpenBi(peer_id, reply) => {
                let stream_id = self.swarm.behaviour_mut().bistream.open_bi(&peer_id);
                self.bistream_manager
                    .insert_request(peer_id, stream_id, reply);
                // if self.swarm.is_connected(&peer_id) {
                //     self.swarm.behaviour_mut().bistream.open_bi(&peer_id, reply)
                // } else {
                //     let addrs = self.swarm.behaviour_mut().addresses_of_peer(&peer_id);
                //     if !addrs.is_empty() {
                //         for addr in addrs {
                //             let _ = self.swarm.dial(addr);
                //         }
                //     } else {
                //         if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                //             kad.get_closest_peers(peer_id);
                //         }
                //     }
                //     let command = Command::OpenBi(peer_id, reply);
                //     self.pending_peers
                //         .entry(peer_id)
                //         .or_default()
                //         .push_back(command);
                // }
            }
        }
        Ok(())
    }

    async fn handle_swarm_event(&mut self, event: NodeSwarmEvent) -> anyhow::Result<()> {
        info!("swarm event: {event:?}");
        use crate::behaviour::NodeBehaviourEvent::*;
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                self.new_listen_addr(address).await?;
            }

            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                if let Some(mut pending) = self.pending_peers.remove(&peer_id) {
                    while let Some(command) = pending.pop_front() {
                        self.command_tx.send(command)?;
                    }
                }
                let _ = self
                    .event_tx
                    .send(Event::ConnectionEstablished { peer_id, endpoint })
                    .await;
            }

            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint,
                cause,
                ..
            } => {
                eprintln!(
                    "Connection closed to {peer_id} on {} (dialer: {}, relays: {}) cause {:?}",
                    endpoint.get_remote_address(),
                    if endpoint.is_dialer() { "us" } else { "them" },
                    endpoint.is_relayed(),
                    cause
                )
            }

            // Dial pending peers if discovered via MDNS.
            SwarmEvent::Behaviour(Mdns(mdns::Event::Discovered(discovered))) => {
                for (peer_id, multiaddr) in discovered {
                    if self.pending_peers.contains_key(&peer_id)
                        && !self.swarm.is_connected(&peer_id)
                    {
                        self.swarm.dial(multiaddr)?;
                    }
                }
            }

            // Dial pending peers if discovered via DHT.
            SwarmEvent::Behaviour(Kademlia(kad::KademliaEvent::OutboundQueryProgressed {
                result: kad::QueryResult::GetClosestPeers(Ok(kad::GetClosestPeersOk { peers, .. })),
                ..
            })) => {
                for peer_id in peers {
                    if self.pending_peers.contains_key(&peer_id)
                        && !self.swarm.is_connected(&peer_id)
                    {
                        // TODO: Verify that kademlia provides the peer address at this point.
                        self.swarm.dial(peer_id)?;
                    }
                }
            }

            // Emit new incoming streams.
            SwarmEvent::Behaviour(Bistream(event)) => {
                if let Some(unhandled_stream) = self.bistream_manager.inject_event(event) {
                    self.event_tx
                        .send(Event::IncomingBiStream(unhandled_stream))
                        .await
                        .map_err(|_| anyhow!("Event receiver dropped"))?;
                }
            }

            // Use relay nodes.
            SwarmEvent::Behaviour(Identify(identify::Event::Received { peer_id, info })) => {
                // TODO: Only if we are not publicly reachable ourselves
                if self.allowed_relays.contains(&peer_id) && !self.active_relays.contains_key(&peer_id)
                    // TODO: Make sure to decrease again
                    && self.active_relays.len() < MAX_RELAYS
                    && info
                        .protocols
                        .iter()
                        .any(|x| x.as_bytes() == dcutr::PROTOCOL_NAME)
                {
                    self.listen_relay(peer_id, &info.listen_addrs[..])?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn listen_relay(&mut self, peer_id: PeerId, addrs: &[Multiaddr]) -> anyhow::Result<()> {
        let listener_ids = addrs
            .iter()
            .filter(|addr| {
                addr.is_global_ip()
                    && addr.has_protocol(|p| matches!(p, multiaddr::Protocol::Tcp(_)))
                    && !addr.has_protocol(|p| matches!(p, multiaddr::Protocol::P2pCircuit))
            })
            .cloned()
            .map(|addr| {
                addr.with(multiaddr::Protocol::P2p(peer_id.into()))
                    .with(multiaddr::Protocol::P2pCircuit)
            })
            .filter_map(|addr| {
                info!("Listen on relay {addr}");
                match self.swarm.listen_on(addr.clone()) {
                    Ok(id) => Some(id),
                    Err(err) => {
                        error!("Failed to listen via DCUTR on {}: {:?}", addr, err);
                        None
                    }
                }
            });
        let listener_ids: HashSet<_> = listener_ids.collect();
        if !listener_ids.is_empty() {
            self.active_relays.insert(peer_id, listener_ids);
        }
        Ok(())
    }
}

struct Tokio;
impl Executor for Tokio {
    fn exec(&self, fut: std::pin::Pin<Box<dyn futures::Future<Output = ()> + Send>>) {
        tokio::task::spawn(fut);
    }
}

fn build_transport(
    keypair: &Keypair,
) -> (
    Boxed<(PeerId, StreamMuxerBox)>,
    libp2p::relay::v2::client::Client,
) {
    let port_reuse = true;
    let connection_timeout = Duration::from_secs(30);

    // TCP
    let tcp_config = tcp::Config::default().port_reuse(port_reuse);
    let tcp_transport = tcp::tokio::Transport::new(tcp_config);

    // Noise config for TCP
    let auth_config = {
        let dh_keys = noise::Keypair::<noise::X25519Spec>::new()
            .into_authentic(keypair)
            .expect("Noise key generation failed");

        noise::NoiseConfig::xx(dh_keys).into_authenticated()
    };

    // Stream muxer config for TCP
    let muxer_config = {
        let mut yamux_config = yamux::YamuxConfig::default();
        yamux_config.set_max_buffer_size(16 * 1024 * 1024); // TODO: configurable
        yamux_config.set_receive_window_size(16 * 1024 * 1024); // TODO: configurable
        yamux_config.set_window_update_mode(yamux::WindowUpdateMode::on_read());
        // yamux_config.set_window_update_mode(yamux::WindowUpdateMode::on_receive());
        yamux_config
    };

    // Enable Relay
    let (relay_transport, relay_client) =
        libp2p::relay::v2::client::Client::new_transport_and_behaviour(
            keypair.public().to_peer_id(),
        );

    let transport = OrTransport::new(relay_transport, tcp_transport);
    let transport = transport
        .upgrade(core::upgrade::Version::V1Lazy)
        .authenticate(auth_config)
        .multiplex(muxer_config)
        .timeout(connection_timeout)
        .boxed();

    let dns_cfg = dns::ResolverConfig::cloudflare();
    let dns_opts = dns::ResolverOpts::default();
    let transport = dns::TokioDnsConfig::custom(transport, dns_cfg, dns_opts)
        .unwrap()
        .boxed();

    (transport, relay_client)
}

fn generate_ed25519_from_seed(seed_bytes: &[u8]) -> Keypair {
    let mut bytes = [0u8; 32];
    let len = seed_bytes.len().min(32);
    bytes[..len].copy_from_slice(&seed_bytes[..len]);

    let secret_key = ed25519::SecretKey::from_bytes(&mut bytes)
        .expect("this returns `Err` only if the length is wrong; the length is correct; qed");
    Keypair::Ed25519(secret_key.into())
}
