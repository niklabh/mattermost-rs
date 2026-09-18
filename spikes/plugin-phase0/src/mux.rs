//! A tokio-friendly handle over `yamux::Connection`, whose API is poll-based: one task owns the
//! connection and serves open requests, and inbound streams arrive on a channel.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::task::Poll;

use anyhow::{Result, anyhow};
use futures::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

pub use yamux::{Mode, Stream};

type OpenReply = oneshot::Sender<Result<Stream, yamux::ConnectionError>>;

#[derive(Clone)]
pub struct Mux {
    open_tx: mpsc::UnboundedSender<OpenReply>,
}

impl Mux {
    pub fn start<T>(
        io: T,
        mode: Mode,
    ) -> (
        Mux,
        mpsc::UnboundedReceiver<Stream>,
        tokio::task::JoinHandle<Result<()>>,
    )
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut cfg = yamux::Config::default();
        // INTEROP (Phase 0 finding): libp2p yamux auto-tunes a stream's receive window above
        // 1 MiB, but rejects any frame body over 1 MiB (frame/io.rs, MAX_FRAME_BODY_LEN).
        // hashicorp/yamux sends up to the whole advertised window in one frame (stream.go,
        // write: `max = min(window, len(b))`), so a grown window kills the session with
        // "frame body is too large". Capping the connection budget at exactly
        // `max_num_streams * 256 KiB` leaves auto-tuning no headroom, pinning every window at
        // the spec default.
        let streams = env_usize("MUX_STREAMS", 1024);
        cfg.set_max_connection_receive_window(Some(streams * yamux::DEFAULT_CREDIT as usize));
        cfg.set_max_num_streams(streams);
        if std::env::var("MUX_UNCAPPED").is_ok() {
            cfg.set_max_connection_receive_window(None);
        }
        let mut conn = yamux::Connection::new(io, cfg, mode);
        let (open_tx, mut open_rx) = mpsc::unbounded_channel::<OpenReply>();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(async move {
            let mut waiting: VecDeque<OpenReply> = VecDeque::new();
            let mut opens_closed = false;
            poll_fn(|cx| {
                while !opens_closed {
                    match open_rx.poll_recv(cx) {
                        Poll::Ready(Some(tx)) => waiting.push_back(tx),
                        Poll::Ready(None) => opens_closed = true,
                        Poll::Pending => break,
                    }
                }
                while !waiting.is_empty() {
                    match conn.poll_new_outbound(cx) {
                        Poll::Ready(r) => {
                            if let Some(tx) = waiting.pop_front() {
                                let _ = tx.send(r);
                            }
                        }
                        Poll::Pending => break,
                    }
                }
                loop {
                    match conn.poll_next_inbound(cx) {
                        Poll::Ready(Some(Ok(s))) => {
                            let _ = in_tx.send(s);
                        }
                        Poll::Ready(Some(Err(e))) => {
                            return Poll::Ready(Err(anyhow!("yamux: {e}")));
                        }
                        Poll::Ready(None) => return Poll::Ready(Ok(())),
                        Poll::Pending => return Poll::Pending,
                    }
                }
            })
            .await
        });
        (Mux { open_tx }, in_rx, driver)
    }

    /// Open a stream. Like libp2p yamux generally, the SYN is not sent until the stream writes.
    pub async fn open(&self) -> Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.open_tx
            .send(tx)
            .map_err(|_| anyhow!("yamux driver gone"))?;
        Ok(rx.await??)
    }

    /// Open a stream and send its SYN immediately, for a stream the host never writes to.
    ///
    /// INTEROP (Phase 0 finding): libp2p yamux opens streams lazily — the SYN rides on the first
    /// frame the stream sends (connection/stream.rs:81). go-plugin's server blocks in
    /// `mux.Accept()` for control, stdout and stderr before serving anything
    /// (rpc_server.go:78-96), and the host never writes to the two std streams, so a lazy open
    /// deadlocks the handshake. An empty write sends a zero-length DATA frame carrying the SYN.
    ///
    /// Not the default: forcing the SYN on *every* open, with 300 concurrent opens, overflowed
    /// hashicorp's 256-stream accept backlog ("backlog exceeded, forcing connection reset",
    /// session.go:710) in 3 of 5 runs, where lazy SYN passed 5 of 5. libp2p's own
    /// MAX_ACK_BACKLOG of 256 did not prevent it; the mechanism was not established.
    pub async fn open_eager(&self) -> Result<Stream> {
        let mut s = self.open().await?;
        futures::AsyncWriteExt::write(&mut s, &[]).await?;
        Ok(s)
    }
}

fn env_usize(name: &str, def: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(def)
}
