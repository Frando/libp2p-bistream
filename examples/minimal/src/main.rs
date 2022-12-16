use std::time::Duration;

use clap::Parser;
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    core::{self, muxing::StreamMuxerBox, transport::Boxed},
    identify,
    identity::{ed25519, Keypair},
    noise, ping,
    swarm::{Executor, NetworkBehaviour},
    swarm::{SwarmBuilder, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm, Transport,
};

use libp2p_bistream as bistream;

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
    node.set_verbose(args.verbose);

    if let Some(port) = args.listen_port {
        let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{port}").parse()?;
        node.swarm.listen_on(listen_addr)?;
    }
    if let Some(peer) = args.connect_peer {
        let peer_id: PeerId = peer.parse()?;
        node.swarm
            .behaviour_mut()
            .bistream
            .open_bi_with_addrs(&peer_id, args.connect_addr);
    }

    loop {
        let stream = node.drive_until_bistream().await;
        println!(
            "New stream from {} (dialer: {}, stream id: {})",
            stream.peer_id(),
            if stream.is_dial() { "us" } else { "them" },
            stream.stream_id()
        );
        let r_label = format!("{}:r", stream);
        let w_label = format!("{}:w", stream);
        let (mut r, mut w) = stream.split();
        tokio::spawn(async move {
            for i in 0..10 {
                if let Err(err) = w.write_all(format!("hello {i}").as_bytes()).await {
                    println!("[{w_label}] ERR: {err}");
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        tokio::spawn(async move {
            let mut read_buf = vec![0u8; 1024];
            loop {
                match r.read(&mut read_buf).await {
                    Ok(0) => {
                        println!("[{r_label}] recv EOF");
                        break;
                    }
                    Ok(n) => println!(
                        "[{r_label}] recv: {}",
                        std::str::from_utf8(&read_buf[..n]).expect("invalid utf8")
                    ),
                    Err(err) => {
                        println!("[{r_label}] recv ERR: {}", err);
                        break;
                    }
                }
            }
        });
    }
}

/// Libp2p behaviour for the node.
#[derive(NetworkBehaviour)]
pub struct NodeBehaviour {
    pub(crate) ping: ping::Behaviour,
    pub(crate) identify: identify::Behaviour,
    pub(crate) bistream: bistream::Behaviour,
}

impl NodeBehaviour {
    pub fn new(local_key: &Keypair) -> anyhow::Result<Self> {
        Ok(Self {
            bistream: bistream::Behaviour::new(),
            identify: identify::Behaviour::new(identify::Config::new(
                "ipfs/0.1.0".into(),
                local_key.public(),
            )),
            ping: ping::Behaviour::default(),
        })
    }
}

pub struct Node {
    pub swarm: Swarm<NodeBehaviour>,
    verbose: bool,
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
        let node = Self {
            swarm,
            verbose: false,
        };
        Ok(node)
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    pub async fn drive_until_bistream(&mut self) -> bistream::BiStream {
        use NodeBehaviourEvent::*;
        loop {
            let event = self.swarm.next().await.expect("the swarm never dies");
            if self.verbose {
                println!("event: {:?}", event);
            }
            match event {
                SwarmEvent::Behaviour(Bistream(bistream::Event::StreamReady(stream))) => {
                    return stream;
                }
                _ => {}
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
        yamux_config.set_max_buffer_size(16 * 1024 * 1024); // TODO: configurable
        yamux_config.set_receive_window_size(16 * 1024 * 1024); // TODO: configurable
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
