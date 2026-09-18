//! go-plugin's `MuxBroker` (mux_broker.go): extra streams on a plugin connection, rendezvoused by
//! a numeric id.
//!
//! One side reserves an id with [`MuxBroker::next_id`], tells the other side the id in an RPC,
//! and waits in [`MuxBroker::accept`]; the other side calls [`MuxBroker::dial`]. The dialer opens a
//! stream and writes the id as a little-endian `u32`; the acceptor writes the same id back as the
//! acknowledgement. Mattermost uses this for everything beyond the first RPC connection: the API
//! and database servers a plugin reaches back to, and every HTTP request and response body.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::yamux::{Session, Stream};

/// How long a stream waits for its accept, and an accept for its stream (mux_broker.go).
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrokerError {
    #[error("timeout waiting for accept")]
    AcceptTimeout,
    #[error("bad ack: {got} (expected {want})")]
    BadAck { got: u32, want: u32 },
    #[error(transparent)]
    Yamux(#[from] crate::yamux::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct Pending {
    tx: mpsc::Sender<Stream>,
    rx: Option<mpsc::Receiver<Stream>>,
}

/// Brokers streams on one yamux session. Cheap to clone.
#[derive(Clone)]
pub struct MuxBroker {
    inner: Arc<Inner>,
}

struct Inner {
    session: Session,
    next_id: AtomicU32,
    streams: Mutex<HashMap<u32, Pending>>,
}

impl MuxBroker {
    /// A broker on `session`, and the task that must run for it to receive streams
    /// (mux_broker.go, `Run`). Start it only after any streams accepted by other means — go-plugin
    /// accepts the control, stdout and stderr streams first.
    pub fn new(session: Session) -> (Self, impl std::future::Future<Output = ()> + Send + 'static) {
        let broker = Self {
            inner: Arc::new(Inner {
                session,
                next_id: AtomicU32::new(0),
                streams: Mutex::new(HashMap::new()),
            }),
        };
        let run = broker.clone().run();
        (broker, run)
    }

    pub fn session(&self) -> &Session {
        &self.inner.session
    }

    /// A fresh id, starting at 1 (`atomic.AddUint32(&m.nextId, 1)`).
    pub fn next_id(&self) -> u32 {
        self.inner
            .next_id
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    /// The pending slot for `id`, created if needed (mux_broker.go, `getStream`).
    fn slot(&self, id: u32) -> (mpsc::Sender<Stream>, Option<mpsc::Receiver<Stream>>) {
        let mut streams = self
            .inner
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let p = streams.entry(id).or_insert_with(|| {
            let (tx, rx) = mpsc::channel(1);
            Pending { tx, rx: Some(rx) }
        });
        (p.tx.clone(), p.rx.take())
    }

    /// The sending half of `id`'s slot, leaving the receiving half for `accept`.
    fn sender(&self, id: u32) -> mpsc::Sender<Stream> {
        let mut streams = self
            .inner
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        streams
            .entry(id)
            .or_insert_with(|| {
                let (tx, rx) = mpsc::channel(1);
                Pending { tx, rx: Some(rx) }
            })
            .tx
            .clone()
    }

    fn forget(&self, id: u32) {
        self.inner
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// Wait up to [`ACCEPT_TIMEOUT`] for the stream dialled to `id`, then acknowledge it.
    pub async fn accept(&self, id: u32) -> Result<Stream, BrokerError> {
        let (_, rx) = self.slot(id);
        // A second accept on the same id is Go's documented misuse; it times out here.
        let Some(mut rx) = rx else {
            tokio::time::sleep(ACCEPT_TIMEOUT).await;
            return Err(BrokerError::AcceptTimeout);
        };
        let stream = tokio::time::timeout(ACCEPT_TIMEOUT, rx.recv()).await;
        self.forget(id);
        let mut stream = match stream {
            Ok(Some(s)) => s,
            _ => return Err(BrokerError::AcceptTimeout),
        };
        stream.write_all(&id.to_le_bytes()).await?;
        Ok(stream)
    }

    /// Open a stream to the peer's accept of `id` (mux_broker.go, `Dial`).
    pub async fn dial(&self, id: u32) -> Result<Stream, BrokerError> {
        let mut stream = self.inner.session.open().await?;
        stream.write_all(&id.to_le_bytes()).await?;
        let mut ack = [0u8; 4];
        stream.read_exact(&mut ack).await?;
        let got = u32::from_le_bytes(ack);
        if got != id {
            return Err(BrokerError::BadAck { got, want: id });
        }
        Ok(stream)
    }

    /// Accept `id` and serve net/rpc on it until it closes (mux_broker.go, `AcceptAndServe`).
    /// Methods are registered as `"Plugin.<Method>"`, the name go-plugin always uses.
    pub async fn accept_and_serve(&self, id: u32, server: Arc<go_netrpc::Server>) {
        match self.accept(id).await {
            Ok(stream) => {
                if let Err(e) = server.serve(stream).await {
                    tracing::debug!(id, error = %e, "go-plugin: brokered connection ended");
                }
            }
            Err(e) => tracing::error!(id, error = %e, "plugin: plugin acceptAndServe error"),
        }
    }

    /// mux_broker.go, `Run`: route every incoming stream by the id it opens with.
    async fn run(self) {
        loop {
            let Ok(mut stream) = self.inner.session.accept().await else {
                return;
            };
            let broker = self.clone();
            tokio::spawn(async move {
                let mut id = [0u8; 4];
                if stream.read_exact(&mut id).await.is_err() {
                    return;
                }
                let id = u32::from_le_bytes(id);
                // A stream may arrive before its accept: park it without claiming the receiver.
                let tx = broker.sender(id);
                // Go drops a second stream for the same id; so does a full channel here.
                if tx.try_send(stream).is_err() {
                    return;
                }
                // mux_broker.go, `timeoutWait`: an unclaimed stream is closed and forgotten.
                tokio::time::sleep(ACCEPT_TIMEOUT).await;
                let unclaimed = {
                    let mut streams = broker
                        .inner
                        .streams
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    match streams.get_mut(&id) {
                        Some(p) if p.rx.is_some() => streams.remove(&id),
                        _ => None,
                    }
                };
                drop(unclaimed);
            });
        }
    }
}
