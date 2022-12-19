//! Optional managed mode for the bistream behavior.

use std::{
    collections::{HashMap, VecDeque},
    future::poll_fn,
    task::{ready, Context, Poll}, sync::{Mutex, Arc},
};

use futures::{Future, FutureExt};
use libp2p::{
    swarm::dial_opts::{DialOpts, PeerCondition},
    Multiaddr, PeerId,
};
use smallvec::SmallVec;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender},
    oneshot,
};
use tracing::warn;

use crate::{BiStream, StreamOrigin};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Channel closed")]
    ChannelClosed,
    #[error("Failed to dial peer")]
    DialFailed,
}

pub struct DialRequest {
    peer_id: PeerId,
    addrs: SmallVec<[Multiaddr; 8]>,
    reply: oneshot::Sender<Result<Connection>>,
}

pub struct OpenRequest {
    peer_id: PeerId,
    reply: oneshot::Sender<Result<BiStream>>,
}

pub struct Endpoint {
    dial_tx: Sender<DialRequest>,
    conn_rx: Receiver<Connection>,
}

impl Endpoint {
    pub async fn connect(&self, peer_id: PeerId) -> Result<Connection> {
        self.connect_with_addrs(peer_id, Default::default()).await
    }
    pub async fn connect_with_addrs(
        &self,
        peer_id: PeerId,
        addrs: SmallVec<[Multiaddr; 8]>,
    ) -> Result<Connection> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = DialRequest {
            peer_id,
            addrs,
            reply: reply_tx,
        };
        self.dial_tx.send(req).map_err(|_| Error::ChannelClosed)?;
        let res = reply_rx.await.map_err(|_err| Error::ChannelClosed)?;
        res
    }

    pub async fn accept(&mut self) -> Result<Connection> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<Result<Connection>> {
        self.conn_rx
            .poll_recv(cx)
            .map(|res| res.ok_or(Error::ChannelClosed))
    }
}

#[derive(Debug, Clone)]
pub struct Connection {
    peer_id: PeerId,
    accept_rx: Arc<Mutex<Receiver<BiStream>>>,
    open_tx: Sender<OpenRequest>,
}

impl Connection {
    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub async fn accept_stream(&self) -> Result<BiStream> {
        poll_fn(|cx| self.poll_accept_stream(cx)).await
    }

    pub fn poll_accept_stream(&self, cx: &mut Context<'_>) -> Poll<Result<BiStream>> {
        let res = ready!(self.accept_rx.lock().unwrap().poll_recv(cx));
        let res = res.ok_or(Error::ChannelClosed)?;
        Poll::Ready(Ok(res))
    }

    pub fn open_stream(&self) -> impl Future<Output = Result<BiStream>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = OpenRequest {
            peer_id: self.peer_id.clone(),
            reply: reply_tx
        };
        if let Err(err) = self.open_tx.send(req) {
            let _ = err.0.reply.send(Err(Error::ChannelClosed));
        }
        reply_rx.map(|res| match res {
            Ok(res) => res,
            Err(_err) => Err(Error::ChannelClosed)
        })
    }
}

#[derive(Debug)]
pub struct Manager {
    conn_tx: Sender<Connection>,

    dial_rx: Receiver<DialRequest>,

    open_tx: Sender<OpenRequest>,
    open_rx: Receiver<OpenRequest>,

    conn_dial: HashMap<PeerId, oneshot::Sender<Result<Connection>>>,
    conn_stream_outbound: HashMap<PeerId, VecDeque<oneshot::Sender<Result<BiStream>>>>,
    conn_stream_inbound: HashMap<PeerId, Sender<BiStream>>,
}

impl Manager {
    pub fn new() -> (Self, Endpoint) {
        let (dial_tx, dial_rx) = mpsc::unbounded_channel();
        let (conn_tx, conn_rx) = mpsc::unbounded_channel();
        let (open_tx, open_rx) = mpsc::unbounded_channel();
        let manager = Manager {
            conn_tx,
            dial_rx,
            open_tx,
            open_rx,
            conn_dial: HashMap::new(),
            conn_stream_outbound: HashMap::new(),
            conn_stream_inbound: HashMap::new(),
        };
        let endpoint = Endpoint { conn_rx, dial_tx };
        (manager, endpoint)
    }

    fn create_conn(&mut self, peer_id: PeerId, stream: Option<BiStream>) -> Connection {
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();
        let conn = Connection {
            open_tx: self.open_tx.clone(),
            accept_rx: Arc::new(Mutex::new(accept_rx)),
            peer_id: peer_id.clone(),
        };
        if let Some(stream) = stream {
            let _ = accept_tx.send(stream);
        }
        self.conn_stream_inbound.insert(peer_id, accept_tx);
        conn
    }

    pub fn handle_connection_incoming(&mut self, peer_id: PeerId) {
        let conn = self.create_conn(peer_id, None);
        let _ = self.conn_tx.send(conn);
    }

    pub fn handle_disconnect(&mut self, peer_id: &PeerId) {
        self.conn_stream_inbound.remove(&peer_id);
        self.conn_stream_outbound.remove(&peer_id);
    }

    pub fn handle_dial_failed(&mut self, peer_id: &PeerId) {
        if let Some(reply) = self.conn_dial.remove(peer_id) {
            let _ = reply.send(Err(Error::DialFailed));
        }
    }

    pub fn handle_stream(&mut self, stream: BiStream) {
        let peer_id = stream.peer_id();
        match stream.origin() {
            StreamOrigin::Inbound => match self.conn_stream_inbound.get_mut(&peer_id) {
                Some(sender) => {
                    let _ = sender.send(stream);
                }
                None => {
                    let conn = self.create_conn(stream.peer_id(), Some(stream));
                    let _ = self.conn_tx.send(conn.clone());
                }
            },
            StreamOrigin::Outbound => {
                if let Some(req) = self
                    .conn_stream_outbound
                    .get_mut(&peer_id)
                    .and_then(|reqs| reqs.pop_front())
                {
                    let _ = req.send(Ok(stream));
                } else {
                    if let Some(sender) = self.conn_dial.remove(&peer_id) {
                        let conn = self.create_conn(stream.peer_id, Some(stream));
                        let _ = sender.send(Ok(conn));
                    } else {
                        warn!("Stream opened without being requested");
                    }
                }
            }
        }
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ManagerEvent> {
        if let Poll::Ready(Some(req)) = self.dial_rx.poll_recv(cx) {
            self.conn_dial.insert(req.peer_id, req.reply);
            // let reqs = self.conn_stream_outbound.entry(req.peer_id).or_default();
            // reqs.push_back(req.reply);
            let dial_opts = DialOpts::peer_id(req.peer_id)
                .addresses(req.addrs.to_vec())
                .condition(PeerCondition::NotDialing)
                .build();
            return Poll::Ready(ManagerEvent::Dial(dial_opts));
        }

        if let Poll::Ready(Some(req)) = self.open_rx.poll_recv(cx) {
            let reqs = self
                .conn_stream_outbound
                .entry(req.peer_id.clone())
                .or_default();
            reqs.push_back(req.reply);
            return Poll::Ready(ManagerEvent::Open(req.peer_id));
        }
        Poll::Pending
    }
}

pub enum ManagerEvent {
    Open(PeerId),
    Dial(DialOpts),
}
