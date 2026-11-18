//! Packet framing over a TCP (optionally TLS) socket.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream, ReadBuf};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

const MAX_PAYLOAD: usize = 0xFF_FFFF;

pub(crate) enum Socket {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    Tls(Box<tokio_rustls::client::TlsStream<Socket>>),
}

impl AsyncRead for Socket {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Socket::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            Socket::Unix(s) => Pin::new(s).poll_read(cx, buf),
            Socket::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Socket {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Socket::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            Socket::Unix(s) => Pin::new(s).poll_write(cx, buf),
            Socket::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Socket::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            Socket::Unix(s) => Pin::new(s).poll_flush(cx),
            Socket::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Socket::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            Socket::Unix(s) => Pin::new(s).poll_shutdown(cx),
            Socket::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

pub(crate) struct PacketStream {
    // `Option` so the socket can be taken out for the TLS upgrade.
    inner: Option<BufStream<Socket>>,
    pub(crate) seq: u8,
    pub(crate) is_tls: bool,
}

impl PacketStream {
    pub(crate) fn new(socket: Socket) -> Self {
        Self { inner: Some(BufStream::new(socket)), seq: 0, is_tls: false }
    }

    fn io(&mut self) -> &mut BufStream<Socket> {
        self.inner.as_mut().expect("stream used after failed TLS upgrade")
    }

    pub(crate) fn take_socket(&mut self) -> Socket {
        self.inner.take().expect("socket already taken").into_inner()
    }

    pub(crate) fn replace_socket(&mut self, socket: Socket) {
        self.inner = Some(BufStream::new(socket));
    }

    /// Start a new command: sequence ids restart at zero.
    pub(crate) fn reset_seq(&mut self) {
        self.seq = 0;
    }

    /// Buffer one logical packet (splitting at 16MB) without flushing.
    pub(crate) async fn write_packet(&mut self, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return self.write_frame(&[]).await;
        }
        let mut last_len = 0;
        for chunk in payload.chunks(MAX_PAYLOAD) {
            self.write_frame(chunk).await?;
            last_len = chunk.len();
        }
        // An exact multiple of 16MB is terminated by an empty frame.
        if last_len == MAX_PAYLOAD {
            self.write_frame(&[]).await?;
        }
        Ok(())
    }

    async fn write_frame(&mut self, chunk: &[u8]) -> Result<()> {
        let len = chunk.len() as u32;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let header = [len as u8, (len >> 8) as u8, (len >> 16) as u8, seq];
        let io = self.io();
        io.write_all(&header).await?;
        io.write_all(chunk).await?;
        Ok(())
    }

    pub(crate) async fn flush(&mut self) -> Result<()> {
        self.io().flush().await?;
        Ok(())
    }

    pub(crate) async fn send_packet(&mut self, payload: &[u8]) -> Result<()> {
        self.write_packet(payload).await?;
        self.flush().await
    }

    /// Read one logical packet, joining 16MB continuation frames.
    pub(crate) async fn recv_packet(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.io().read_exact(&mut header).await?;
            let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            if header[3] != self.seq {
                return Err(Error::protocol(format!(
                    "packet out of order: expected sequence id {}, got {}",
                    self.seq, header[3]
                )));
            }
            self.seq = self.seq.wrapping_add(1);
            let start = payload.len();
            payload.resize(start + len, 0);
            self.io().read_exact(&mut payload[start..]).await?;
            if len < MAX_PAYLOAD {
                return Ok(payload);
            }
        }
    }

    pub(crate) async fn shutdown(&mut self) -> Result<()> {
        if let Some(io) = self.inner.as_mut() {
            io.shutdown().await?;
        }
        Ok(())
    }
}
