use std::{
    str::FromStr,
    time::{Duration, Instant},
};

use clap::Parser;
use futures::{AsyncReadExt, AsyncWriteExt};
use libp2p::{Multiaddr, PeerId};
use libp2p_stream::{
    behaviour::bistream::{self, BiStream},
    Config, Event, Node, NodeController, NodeEvents, Ticket,
};
use pretty_bytes::converter::convert as fmtbytes;

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
    ConnectTicket {
        ticket: String,
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
                let addrs = relays
                    .iter()
                    .map(|a| a.parse())
                    .collect::<Result<Vec<_>, _>>()?;
                Some(addrs)
            };
            node.listen(relays);
        }
        Mode::ConnectTicket { ticket } => {
            println!("Starting in connect mode. Ticket: {}", ticket);
            let ticket = Ticket::from_string(&ticket)?;
            node.dial_ticket(ticket);
        }
        Mode::ConnectAddr { addr } => {
            let addr: Multiaddr = addr.parse()?;
            node.dial_addrs(vec![addr]);
        }
        Mode::ConnectPeer { addr, peer_id } => {
            let peer_id = PeerId::from_str(&peer_id)?;
            eprintln!("conect to {peer_id}");
            let addrs = addr
                .iter()
                .map(|a| a.parse())
                .collect::<Result<Vec<_>, _>>()?;
            node.dial_addrs(addrs);
            let stream = node.open_bi(peer_id).await?;
            eprintln!("OPENED VIA open_bi {}", stream);
            let name = format!("{}", stream);
            spawn_bench_loop2(stream, name);
        }
    }

    handle.await??;

    Ok(())
}

async fn event_loop(mut events: NodeEvents, _node: NodeController) -> anyhow::Result<()> {
    while let Some(ev) = events.next().await {
        eprintln!("MAIN EVENT {ev:?}");
        match ev {
            Event::TicketReady(ticket) => {
                eprintln!("Ticket ready!");
                for addr in ticket.addrs() {
                    eprintln!("  addr {addr}");
                }
            }
            Event::BiStream(bistream::Event::IncomingStream(stream)) => {
                println!("STREAM READY {stream:?}");
                let name = format!("{}", stream);
                spawn_bench_loop2(stream, name);
            }
            Event::NodeError(err) => {
                eprintln!("ERROR {:?}", err);
                break;
            }
        }
    }
    Ok(())
}

fn spawn_bench_loop2(stream: BiStream, name: String) {
    tokio::spawn(async move {
        let mut read_buf = vec![0u8; 1024 * 64];
        let message = vec![8u8; 1024 * 64];
        let mut read_report = Reporter::new(format!("{name}:read"));
        let mut write_report = Reporter::new(format!("{name}:write"));
        let (mut reader, mut writer) = stream.split();
        loop {
            tokio::select! {
                res = reader.read(&mut read_buf) => {
                    match res {
                        Err(err) => eprintln!("READ ERROR on {name}: {err:?}"),
                        Ok(0) => {
                            eprintln!("READ {name}: EOF");
                            break;
                        }
                        Ok(n) => read_report.push(n),
                    }

                },
                res = writer.write_all(&message) => {
                    match res {
                        Err(err) => {
                            eprintln!("WRITE ERROR on {name}: {err:?}");
                            break;
                        }
                        Ok(()) => write_report.push(message.len()),
                    }
                }
            }
        }
    });
}

// fn spawn_bench_loop(stream: BiStream, name: String) {
//     let (mut reader, mut writer) = stream.split();
//     // spawn read loop
//     let name_clone = name.clone();
//     tokio::spawn(async move {
//         let name = name_clone;
//         let mut buf = vec![0u8; 1024 * 64];
//         let mut report = Reporter::new(format!("{name}:read"));
//         eprintln!("START READ {name}");
//         loop {
//             match reader.read(&mut buf).await {
//                 Err(err) => eprintln!("READ ERROR on {name}: {err:?}"),
//                 Ok(0) => {
//                     eprintln!("READ {name}: EOF");
//                     break;
//                 }
//                 Ok(n) => report.push(n),
//             }
//         }
//     });
//     // spawn write loop
//     tokio::spawn(async move {
//         let buf = vec![3u8; 1024 * 64];
//         let mut report = Reporter::new(format!("{name}:write"));
//         eprintln!("START WRITE {name}");
//         loop {
//             match writer.write_all(&buf[..]).await {
//                 Err(err) => {
//                     eprintln!("WRITE ERROR on {name}: {err:?}");
//                     break;
//                 }
//                 Ok(()) => report.push(buf.len()),
//             }
//         }
//     });
// }
//
// fn spawn_stdio_loop(stream: BiStream) {
//     let (mut reader, mut writer) = stream.split();
//     // spawn read loop
//     tokio::spawn(async move {
//         let mut buf = vec![0u8; 1024 * 64];
//         let mut stdout = tokio::io::stdout();
//         while let Ok(n) = reader.read(&mut buf).await {
//             if let Err(err) = tokio::io::AsyncWriteExt::write_all(&mut stdout, &buf[..n]).await {
//                 eprintln!("ERROR: {:?}", err);
//                 break;
//             }
//         }
//     });
//     // spawn write loop
//     tokio::spawn(async move {
//         let mut buf = vec![0u8; 1024 * 64];
//         let mut stdin = BufReader::new(tokio::io::stdin());
//         while let Ok(n) = tokio::io::AsyncReadExt::read(&mut stdin, &mut buf).await {
//             if let Err(err) = writer.write_all(&buf[..n]).await {
//                 eprintln!("ERROR: {:?}", err);
//                 break;
//             }
//         }
//     });
// }

// let stream = node
//     .next_until(|ev| match ev {
//         Event::BiStream(bistream::Event::IncomingStream(stream)) => Some(stream),
//         _ => None
//     })
//     .await
//     .unwrap();
// let _: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
//     Ok(())
// let mut stream = info.stream;
// let peer_id = info.peer_id;
// let stream_id = info.stream_id;
// let origin = info.origin;
// let msg = format!("hello world to {peer_id} for {origin:?}:{stream_id}");
// eprintln!("SEND: {msg}");
// stream.write_all(msg.as_bytes()).await?;
// stream.flush().await?;
// eprintln!("DID SEND");
// let mut read_buf = vec![0u8; 1024 * 64];
// loop {
//     eprintln!("NOW READ");
//     let n = stream.read(&mut read_buf).await?;
//     if n == 0 {
//         eprintln!("READ EMPTY, break");
//         break Ok(());
//     }
//     eprintln!("RECV: {}", std::str::from_utf8(&read_buf[..n])?);
// }
// });
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
            "{} @ {:.2} total {} speed {}",
            self.name,
            elapsed.as_secs_f64(),
            fmtbytes(total),
            fmtbytes(current / self.last_report.elapsed().as_secs_f64())
        );
        self.last_report = Instant::now();
        self.current = 0;
    }
}
