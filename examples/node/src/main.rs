use std::{
    collections::HashSet,
    str::FromStr,
    time::{Duration, Instant},
};

use clap::Parser;
use futures::{AsyncReadExt, AsyncWriteExt};
use libp2p::{Multiaddr, PeerId};
use libp2p_bistream::BiStream;
use pretty_bytes::converter::convert as fmtbytes;

use crate::node::{Config, Event, ListenConfig, Node, NodeController, NodeEvents};

mod behaviour;
mod node;
mod util;

#[derive(Parser)]
pub struct Args {
    #[clap(global = true, long)]
    seed: Option<String>,
    #[clap(subcommand)]
    mode: Mode,
}

#[derive(Parser)]
pub enum Mode {
    Listen {
        #[clap(short, long)]
        relays: Vec<String>,
    },
    ConnectPeer {
        peer_id: String,
        #[clap(short, long)]
        addr: Vec<String>,
    },
    ConnectAddr {
        addr: String,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = Config::new().with_key_seed(args.seed);
    let (events, mut node) = Node::start(config)?;
    let handle = tokio::spawn(event_loop(events, node.clone()));

    match args.mode {
        Mode::Listen { relays } => {
            println!("Starting in listening mode");
            let relays = if relays.is_empty() {
                None
            } else {
                Some(
                    relays
                        .iter()
                        .map(|a| a.parse())
                        .collect::<Result<Vec<_>, _>>()?,
                )
            };
            let config = ListenConfig::new().with_relays(relays);
            node.listen(config);
        }
        Mode::ConnectAddr { addr } => {
            let addr: Multiaddr = addr.parse()?;
            node.dial(vec![addr]);
        }
        Mode::ConnectPeer { addr, peer_id } => {
            let peer_id = PeerId::from_str(&peer_id)?;
            let addrs = addr
                .iter()
                .map(|a| a.parse())
                .collect::<Result<Vec<_>, _>>()?;
            eprintln!(
                "Connect to {peer_id} with addrs: {}",
                addrs
                    .iter()
                    .map(|x| format!("{x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            node.dial(addrs);
            eprintln!("Opening new stream...");
            let stream = node.open_bi(peer_id).await?;
            eprintln!(
                "New stream opened to {} with id {}",
                stream.peer_id(),
                stream.stream_id()
            );
            let name = format!("{}", stream);
            spawn_bench_loop(stream, name);
        }
    }

    match handle.await? {
        Ok(_) => {}
        Err(err) => eprintln!("Error occurred in p2p node: {}", err),
    }

    Ok(())
}

async fn event_loop(mut events: NodeEvents, node: NodeController) -> anyhow::Result<()> {
    let mut incoming_peers = HashSet::new();
    while let Some(ev) = events.next().await {
        match ev {
            Event::NewListenAddr(addr) => {
                eprintln!("Listening on: {addr}")
            }
            Event::ConnectionEstablished { peer_id, endpoint } => {
                eprintln!(
                    "Connected to {peer_id} on {} (dialer: {}, relayed: {})",
                    endpoint.get_remote_address(),
                    if endpoint.is_dialer() { "us" } else { "them" },
                    endpoint.is_relayed()
                );

                if endpoint.is_listener() && !incoming_peers.contains(&peer_id) {
                    incoming_peers.insert(peer_id.clone());
                    let node = node.clone();
                    tokio::spawn(async move {
                        match node.open_bi(peer_id).await {
                            Ok(stream) => {
                                let name = format!("{}", stream);
                                spawn_bench_loop(stream, name);
                            }
                            Err(err) => {
                                eprintln!("failed to open stream on inbound from {peer_id}: {err}");
                            }
                        };
                    });
                }
            }
            Event::IncomingBiStream(stream) => {
                let peer_id = stream.peer_id();
                println!(
                    "Incoming stream from {} with id {}",
                    peer_id,
                    stream.stream_id()
                );

                if stream.is_accept() && !incoming_peers.contains(&peer_id) {
                    incoming_peers.insert(peer_id.clone());
                    let node = node.clone();
                    let peer_id = peer_id.clone();
                    tokio::spawn(async move {
                        match node.open_bi(peer_id).await {
                            Ok(stream) => {
                                let name = format!("{}", stream);
                                spawn_bench_loop(stream, name);
                            }
                            Err(err) => {
                                eprintln!("failed to open stream on inbound from {peer_id}: {err}");
                            }
                        };
                    });
                }

                let name = format!("{}", stream);
                spawn_bench_loop(stream, name);
            }
            Event::NodeError(err) => {
                eprintln!("Node error: {:?}", err);
                break;
            }
        }
    }
    Ok(())
}

fn spawn_bench_loop(stream: BiStream, name: String) {
    let read_label = format!("{name}:r");
    let write_label = format!("{name}:w");
    let (mut reader, mut writer) = stream.split();
    tokio::spawn(async move {
        let mut read_buf = vec![0u8; 1024 * 64];
        let mut read_report = Reporter::new(read_label.clone());
        loop {
            match reader.read(&mut read_buf).await {
                Err(err) => {
                    eprintln!("[{read_label}] ERROR on read: {err:?}");
                    break;
                }
                Ok(0) => {
                    eprintln!("[{read_label}] EOF");
                    break;
                }
                Ok(n) => read_report.push(n),
            }
        }
    });
    tokio::spawn(async move {
        let message = vec![8u8; 1024 * 64];
        let mut write_report = Reporter::new(write_label.clone());
        loop {
            match writer.write_all(&message).await {
                Err(err) => {
                    eprintln!("[{write_label}] error: {err:?}");
                    break;
                }
                Ok(()) => write_report.push(message.len()),
            }
        }
    });
}

struct Reporter {
    name: String,
    total: usize,
    start: Instant,
    report_interval: Duration,
    last_report: Instant,
    current: usize,
}

impl Reporter {
    pub fn new(name: String) -> Self {
        Self {
            name,
            total: 0,
            start: Instant::now(),
            report_interval: Duration::from_secs(1),
            last_report: Instant::now(),
            current: 0,
        }
    }

    pub fn push(&mut self, n: usize) {
        self.total += n;
        self.current += n;
        if self.last_report.elapsed() > self.report_interval {
            self.report();
        }
    }

    pub fn report(&mut self) {
        let total = self.total as f64;
        let current = self.current as f64;
        let elapsed = self.start.elapsed();
        println!(
            "[{}]  speed: {:>9}/s  total: {:>9}  time: {:>6.2}s",
            self.name,
            fmtbytes(current / self.last_report.elapsed().as_secs_f64()),
            fmtbytes(total),
            elapsed.as_secs_f64(),
        );
        self.last_report = Instant::now();
        self.current = 0;
    }
}
