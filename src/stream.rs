use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{
    io::{ReadHalf, WriteHalf},
    AsyncRead, AsyncReadExt, AsyncWrite,
};
use libp2p::{
    core::{muxing::SubstreamBox, ConnectedPoint, Negotiated},
    swarm::derive_prelude::ConnectionEstablished,
    PeerId,
};

#[derive(Debug)]
pub enum StreamOrigin {
    Outbound,
    Inbound,
}

impl<'a> From<ConnectionEstablished<'a>> for StreamOrigin {
    fn from(e: ConnectionEstablished<'a>) -> Self {
        match e.endpoint {
            ConnectedPoint::Dialer { .. } => Self::Outbound,
            ConnectedPoint::Listener { .. } => Self::Inbound,
        }
    }
}

#[derive(Debug)]
pub struct BiStream {
    pub(super) peer_id: PeerId,
    pub(super) origin: StreamOrigin,
    pub(super) stream_id: u64,
    pub(super) inner: Negotiated<SubstreamBox>,
}

impl fmt::Display for BiStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = match self.origin {
            StreamOrigin::Inbound => "accept",
            StreamOrigin::Outbound => "dial",
        };
        let peer_id = self.peer_id.to_base58();
        write!(
            f,
            "{}:{}…{}:{}",
            d,
            &peer_id[..2],
            &peer_id[peer_id.len() - 3..],
            self.stream_id
        )
    }
}

impl BiStream {
    pub fn stream_id(&self) -> u64 {
        self.stream_id
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn origin(&self) -> &StreamOrigin {
        &self.origin
    }

    pub fn is_accept(&self) -> bool {
        matches!(&self.origin, StreamOrigin::Inbound)
    }

    pub fn is_dial(&self) -> bool {
        matches!(&self.origin, StreamOrigin::Outbound)
    }

    pub fn split(self) -> (ReadHalf<Self>, WriteHalf<Self>) {
        let (rh, wh) = AsyncReadExt::split(self);
        (rh, wh)
    }
}

impl AsyncRead for BiStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read_vectored(cx, bufs)
    }
}

impl AsyncWrite for BiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}
