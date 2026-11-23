//! Return zone for an Android receiver.
//!
//! A process on Android cannot read the pointer of other apps, so it cannot
//! detect that the pointer reached an edge. A companion app draws a thin
//! overlay on each edge that faces a sending machine and reports when the
//! pointer pushes against it. This backend turns that report into
//! [`CaptureEvent::Begin`], which hands control back to the sender.
//!
//! Protocol, one ASCII datagram per message on `127.0.0.1:RETURN_ZONE_PORT`:
//!
//! - app sends `watch`; the backend answers `edges <pos>...` now and after
//!   every change, for example `edges left` or `edges` when none is active.
//! - app sends `return <pos>`; the backend begins capture at that edge.
//!
//! Any local app may send these. The worst outcome is that the pointer
//! returns to the sending machine, so no authentication is used.

use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;

use super::{Capture, CaptureError, CaptureEvent, Position};

pub const RETURN_ZONE_PORT: u16 = 4243;

pub struct AndroidInputCapture {
    socket: UdpSocket,
    edges: BTreeSet<Position>,
    /// the companion app that last sent `watch`
    watcher: Option<SocketAddr>,
}

#[derive(Debug, PartialEq)]
enum Request {
    Watch,
    Return(Position),
}

fn parse_position(name: &str) -> Option<Position> {
    match name {
        "left" => Some(Position::Left),
        "right" => Some(Position::Right),
        "top" => Some(Position::Top),
        "bottom" => Some(Position::Bottom),
        _ => None,
    }
}

fn parse_request(datagram: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(datagram).ok()?.trim();
    match text.split_once(' ') {
        None if text == "watch" => Some(Request::Watch),
        Some(("return", pos)) => parse_position(pos.trim()).map(Request::Return),
        _ => None,
    }
}

fn edges_message(edges: &BTreeSet<Position>) -> String {
    edges.iter().fold("edges".to_owned(), |mut message, pos| {
        message.push(' ');
        message.push_str(&pos.to_string());
        message
    })
}

impl AndroidInputCapture {
    pub fn new() -> io::Result<Self> {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, RETURN_ZONE_PORT))?;
        socket.set_nonblocking(true)?;
        log::info!("return zone listening on 127.0.0.1:{RETURN_ZONE_PORT}");
        Ok(Self {
            socket: UdpSocket::from_std(socket)?,
            edges: BTreeSet::new(),
            watcher: None,
        })
    }

    async fn notify(&self) {
        if let Some(watcher) = self.watcher {
            let message = edges_message(&self.edges);
            if let Err(e) = self.socket.send_to(message.as_bytes(), watcher).await {
                log::debug!("return zone app unreachable: {e}");
            }
        }
    }
}

#[async_trait]
impl Capture for AndroidInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        if self.edges.insert(pos) {
            self.notify().await;
        }
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        if self.edges.remove(&pos) {
            self.notify().await;
        }
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.edges.clear();
        self.notify().await;
        Ok(())
    }
}

impl Stream for AndroidInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut buf = [0u8; 64];
        loop {
            let mut datagram = ReadBuf::new(&mut buf);
            let sender = match self.socket.poll_recv_from(cx, &mut datagram) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(sender)) => sender,
                // a reply to a closed app port reports an error here; keep listening
                Poll::Ready(Err(e)) => {
                    log::debug!("return zone receive: {e}");
                    continue;
                }
            };
            match parse_request(datagram.filled()) {
                Some(Request::Watch) => {
                    self.watcher = Some(sender);
                    let message = edges_message(&self.edges);
                    if let Err(e) = self.socket.try_send_to(message.as_bytes(), sender) {
                        log::debug!("return zone app unreachable: {e}");
                    }
                }
                Some(Request::Return(pos)) if self.edges.contains(&pos) => {
                    log::info!("return zone: pointer left at {pos}");
                    return Poll::Ready(Some(Ok((pos, CaptureEvent::Begin))));
                }
                _ => log::debug!("return zone: ignored datagram from {sender}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        assert_eq!(parse_request(b"watch"), Some(Request::Watch));
        assert_eq!(parse_request(b"watch\n"), Some(Request::Watch));
        assert_eq!(
            parse_request(b"return left"),
            Some(Request::Return(Position::Left))
        );
        assert_eq!(parse_request(b"return middle"), None);
        assert_eq!(parse_request(b"returnleft"), None);
        assert_eq!(parse_request(&[0xff, 0x00]), None);
    }

    #[test]
    fn formats_edges() {
        assert_eq!(edges_message(&BTreeSet::new()), "edges");
        let edges = BTreeSet::from([Position::Top, Position::Left]);
        assert_eq!(edges_message(&edges), "edges left top");
    }
}
