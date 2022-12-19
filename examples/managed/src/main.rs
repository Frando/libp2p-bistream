use std::time::Duration;

use bistream::{BiStream, Connection};
use clap::Parser;
use futures::{io::BufReader, AsyncBufReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    core::{self, muxing::StreamMuxerBox, transport::Boxed},
    identify,
    identity::{ed25519, Keypair},
    noise,
    swarm::SwarmBuilder,
    swarm::{Executor, NetworkBehaviour},
    tcp, yamux, Multiaddr, PeerId, Swarm, Transport,
};

use libp2p_bistream as bistream;
use tracing::info;

pub const PROTOCOL_NAME: &str = "/bistream/example/0.1.0";

#[derive(Parser)]
pub struct Args {
    #[clap(short, long)]
    seed: Option<String>,
    #[clap(short, long)]
    listen_port: Option<usize>,
    #[clap(short, long)]
    connect_peer: Option<String>,
    #[clap(short = 'a', long)]
    connect_addr: Vec<Multiaddr>,
    #[clap(short, long)]
    verbose: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let mut node = Node::new(args.seed)?;
    node.verbose = args.verbose;

    // Enable managed mode for the bistream behavior.
    let mut endpoint = node.bistreams();

    // Listen if listening is enabled.
    if let Some(port) = args.listen_port {
        let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{port}").parse()?;
        node.swarm.listen_on(listen_addr)?;
    }

    // Drive the swarm.
    tokio::spawn(async move { node.drive().await });

    // Connect to a single peer...
    if let Some(peer) = args.connect_peer {
        let peer_id: PeerId = peer.parse()?;
        let conn = endpoint
            .connect_with_addrs(peer_id, args.connect_addr.into())
            .await?;
        peer_loop(conn).await?;
    // ... or accept any number of connections.
    } else {
        loop {
            let conn = endpoint.accept().await?;
            tokio::spawn(async move {
                if let Err(err) = peer_loop(conn).await {
                    eprintln!("ERR: {err}");
                }
            });
        }
    }
    Ok(())
}

async fn peer_loop(conn: Connection) -> anyhow::Result<()> {
    // Open two stream ourselves.
    tokio::spawn(open_streams(conn.clone()));

    // Accept any number of incoming streams and run an upper-casing echo loop.
    loop {
        let stream = conn.accept_stream().await?;
        tokio::spawn(async move {
            if let Err(err) = echo_loop_upper(stream).await {
                println!("Error on stream: {}", err);
            }
        });
    }

    async fn open_streams(conn: Connection) -> anyhow::Result<()> {
        let stream1 = conn.open_stream().await?;
        let s1 = tokio::spawn(stream_loop(format!("Message for {stream1}"), stream1));
        let stream2 = conn.open_stream().await?;
        let s2 = tokio::spawn(stream_loop(format!("Message for {stream2}"), stream2));
        s1.await??;
        s2.await??;
        Ok(())
    }
}

async fn echo_loop_upper(stream: BiStream) -> anyhow::Result<()> {
    info!(
        "New stream from {} (dialer: {}, stream id: {}): Start echo loop",
        stream.peer_id(),
        if stream.is_dial() { "us" } else { "them" },
        stream.stream_id()
    );

    let (r, mut w) = stream.split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next().await {
        let line = line?;
        w.write_all(line.to_uppercase().as_bytes()).await?;
        w.write_all(b"\n").await?;
    }
    Ok(())
}

async fn stream_loop(message: String, stream: BiStream) -> anyhow::Result<()> {
    info!(
        "New stream from {} (dialer: {}, stream id: {}): Start send/recv loop",
        stream.peer_id(),
        if stream.is_dial() { "us" } else { "them" },
        stream.stream_id()
    );

    let label = format!("{}", stream);
    let (r, mut w) = stream.split();

    // Send a message every second.
    let task_send = tokio::spawn({
        let label = label.clone();
        async move {
            for i in 0..u32::MAX {
                let message = format!("{message} #{i}\n");
                println!("[{label}] send: {}", &message[..(message.len() - 1)]);
                if let Err(err) = w.write_all(message.as_bytes()).await {
                    println!("[{label}] send error: {err}");
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    // Print all incoming messages.
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next().await {
        let line = line?;
        println!("[{label}] recv: {line}");
    }
    println!("[{label}] EOF");

    task_send.await?;
    Ok(())
}

/// Libp2p behaviour for the node.
#[derive(NetworkBehaviour)]
pub struct NodeBehaviour {
    pub(crate) identify: identify::Behaviour,
    pub(crate) bistream: bistream::Behaviour,
}

impl NodeBehaviour {
    pub fn new(local_key: &Keypair) -> anyhow::Result<Self> {
        Ok(Self {
            bistream: bistream::Behaviour::new(PROTOCOL_NAME.into()),
            identify: identify::Behaviour::new(identify::Config::new(
                "ipfs/0.1.0".into(),
                local_key.public(),
            )),
        })
    }
}

pub struct Node {
    pub swarm: Swarm<NodeBehaviour>,
    pub verbose: bool,
}

impl Node {
    pub fn new(key_seed: Option<String>) -> anyhow::Result<Self> {
        let keypair = if let Some(seed) = key_seed {
            generate_ed25519_from_seed(seed.as_bytes())
        } else {
            Keypair::generate_ed25519()
        };

        let transport = build_transport(&keypair);
        let peer_id = keypair.public().to_peer_id();
        println!("local peer id: {peer_id}");
        let behaviour = NodeBehaviour::new(&keypair)?;
        let swarm = SwarmBuilder::with_executor(transport, behaviour, peer_id, Tokio).build();
        Ok(Self {
            swarm,
            verbose: false,
        })
    }

    pub fn bistreams(&mut self) -> bistream::Endpoint {
        self.swarm.behaviour_mut().bistream.managed()
    }

    pub async fn drive(&mut self) {
        loop {
            let event = self.swarm.next().await.expect("the swarm never dies");
            if self.verbose {
                println!("event: {:?}", event);
            }
        }
    }
}

struct Tokio;
impl Executor for Tokio {
    fn exec(&self, fut: std::pin::Pin<Box<dyn futures::Future<Output = ()> + Send>>) {
        tokio::task::spawn(fut);
    }
}

fn build_transport(keypair: &Keypair) -> Boxed<(PeerId, StreamMuxerBox)> {
    // TCP
    let tcp_transport = tcp::tokio::Transport::new(Default::default());

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
        yamux_config.set_window_update_mode(yamux::WindowUpdateMode::on_read());
        yamux_config
    };

    tcp_transport
        .upgrade(core::upgrade::Version::V1Lazy)
        .authenticate(auth_config)
        .multiplex(muxer_config)
        .timeout(Duration::from_secs(30))
        .boxed()
}

fn generate_ed25519_from_seed(seed_bytes: &[u8]) -> Keypair {
    let mut bytes = [0u8; 32];
    let len = seed_bytes.len().min(32);
    bytes[..len].copy_from_slice(&seed_bytes[..len]);

    let secret_key = ed25519::SecretKey::from_bytes(&mut bytes)
        .expect("this returns `Err` only if the length is wrong; the length is correct; qed");
    Keypair::Ed25519(secret_key.into())
}
