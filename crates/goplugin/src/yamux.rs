//! A stream multiplexer wire- and behaviour-compatible with `hashicorp/yamux` v0.1.2, the one
//! go-plugin runs its net/rpc protocol over.
//!
//! Written against `session.go` and `stream.go` rather than the specification alone, because
//! go-plugin depends on details the specification leaves open:
//!
//! * **A stream's SYN is sent when it is opened**, not with its first write
//!   (`Session.OpenStream` → `sendWindowUpdate`). go-plugin's server accepts its control, stdout
//!   and stderr streams in the order their SYNs arrive, and the host never writes to the last two.
//! * **At most `accept_backlog` opened streams wait for an ACK at once** (`synCh`). The peer resets
//!   any stream that arrives while its accept backlog is full, so an opener without this limit
//!   loses streams under load.
//! * **A data frame may carry the whole send window**, however large the peer made it.
//! * **Window updates are sent from reads**, once the credit to return reaches half the maximum
//!   window.
//! * **`close` is a half-close** (FIN): the stream stays readable until the peer closes too, or
//!   until `stream_close_timeout` resets it.
//!
//! Unlike some Rust yamux implementations, a [`Stream`] may be split and its halves driven from
//! different tasks: reads and writes park separate wakers.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const PROTO_VERSION: u8 = 0;

const TYPE_DATA: u8 = 0;
const TYPE_WINDOW_UPDATE: u8 = 1;
const TYPE_PING: u8 = 2;
const TYPE_GO_AWAY: u8 = 3;

const FLAG_SYN: u16 = 1;
const FLAG_ACK: u16 = 2;
const FLAG_FIN: u16 = 4;
const FLAG_RST: u16 = 8;

const GO_AWAY_NORMAL: u32 = 0;
const GO_AWAY_PROTO_ERR: u32 = 1;
const GO_AWAY_INTERNAL_ERR: u32 = 2;

/// Every stream's window when it opens (const.go, `initialStreamWindow`).
pub const INITIAL_STREAM_WINDOW: u32 = 256 * 1024;

const HEADER_SIZE: usize = 12;

/// Session settings; the defaults are hashicorp/yamux's `DefaultConfig`.
#[derive(Debug, Clone)]
pub struct Config {
    /// How many incoming streams may wait to be accepted, and how many outgoing streams may wait
    /// for an ACK.
    pub accept_backlog: usize,
    pub enable_keepalive: bool,
    pub keepalive_interval: Duration,
    /// How long a write to the connection, or a ping, may take.
    pub connection_write_timeout: Duration,
    /// The receive window each stream grows back to. Must be at least [`INITIAL_STREAM_WINDOW`].
    pub max_stream_window: u32,
    /// A stream not ACKed in this time closes the whole session. `None` disables it.
    pub stream_open_timeout: Option<Duration>,
    /// A half-closed stream the peer does not close in this time is reset. `None` disables it.
    pub stream_close_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            accept_backlog: 256,
            enable_keepalive: true,
            keepalive_interval: Duration::from_secs(30),
            connection_write_timeout: Duration::from_secs(10),
            max_stream_window: INITIAL_STREAM_WINDOW,
            stream_open_timeout: Some(Duration::from_secs(75)),
            stream_close_timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// yamux errors (const.go), plus the transport's.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid protocol version")]
    InvalidVersion,
    #[error("invalid msg type")]
    InvalidMsgType,
    #[error("session shutdown")]
    SessionShutdown,
    #[error("streams exhausted")]
    StreamsExhausted,
    #[error("duplicate stream initiated")]
    DuplicateStream,
    #[error("recv window exceeded")]
    RecvWindowExceeded,
    #[error("i/o deadline reached")]
    Timeout,
    #[error("stream closed")]
    StreamClosed,
    #[error("unexpected flag")]
    UnexpectedFlag,
    #[error("remote end is not accepting connections")]
    RemoteGoAway,
    #[error("connection reset")]
    ConnectionReset,
    #[error("connection write timeout")]
    ConnectionWriteTimeout,
    #[error("keepalive timeout")]
    KeepAliveTimeout,
    /// The peer sent a GoAway with a protocol or internal error, or an unknown code.
    #[error("{0}")]
    GoAway(&'static str),
    #[error("invalid config: {0}")]
    Config(&'static str),
    #[error("{0}")]
    Io(Arc<io::Error>),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(Arc::new(e))
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match e {
            Error::ConnectionReset => io::ErrorKind::ConnectionReset,
            Error::StreamClosed => io::ErrorKind::BrokenPipe,
            Error::Timeout | Error::ConnectionWriteTimeout => io::ErrorKind::TimedOut,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, e)
    }
}

fn header(ty: u8, flags: u16, id: u32, len: u32) -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0] = PROTO_VERSION;
    h[1] = ty;
    h[2..4].copy_from_slice(&flags.to_be_bytes());
    h[4..8].copy_from_slice(&id.to_be_bytes());
    h[8..12].copy_from_slice(&len.to_be_bytes());
    h
}

struct Frame {
    header: [u8; HEADER_SIZE],
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Init,
    SynSent,
    SynReceived,
    Established,
    LocalClose,
    RemoteClose,
    Closed,
    Reset,
}

struct StreamState {
    state: State,
    recv_buf: VecDeque<u8>,
    recv_window: u32,
    send_window: u32,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    close_timer: Option<JoinHandle<()>>,
}

impl StreamState {
    /// stream.go, `notifyWaiting`.
    fn wake_all(&mut self) {
        if let Some(w) = self.read_waker.take() {
            w.wake();
        }
        if let Some(w) = self.write_waker.take() {
            w.wake();
        }
    }

    /// stream.go, `sendFlags`: the SYN or ACK a stream's first frame carries.
    fn send_flags(&mut self) -> u16 {
        match self.state {
            State::Init => {
                self.state = State::SynSent;
                FLAG_SYN
            }
            State::SynReceived => {
                self.state = State::Established;
                FLAG_ACK
            }
            _ => 0,
        }
    }
}

struct StreamShared {
    id: u32,
    st: Mutex<StreamState>,
}

impl StreamShared {
    fn new(id: u32, state: State) -> Arc<Self> {
        Arc::new(Self {
            id,
            st: Mutex::new(StreamState {
                state,
                recv_buf: VecDeque::new(),
                recv_window: INITIAL_STREAM_WINDOW,
                send_window: INITIAL_STREAM_WINDOW,
                read_waker: None,
                write_waker: None,
                close_timer: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, StreamState> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct Streams {
    map: HashMap<u32, Arc<StreamShared>>,
    /// Opened streams not yet ACKed, each holding one `syn` permit (session.go, `inflight`).
    inflight: HashMap<u32, OwnedSemaphorePermit>,
}

struct Inner {
    config: Config,
    next_id: AtomicU32,
    streams: Mutex<Streams>,
    syn: Arc<Semaphore>,
    accept_tx: mpsc::Sender<Arc<StreamShared>>,
    accept_rx: tokio::sync::Mutex<mpsc::Receiver<Arc<StreamShared>>>,
    send_tx: mpsc::UnboundedSender<Frame>,
    pings: Mutex<HashMap<u32, oneshot::Sender<()>>>,
    ping_id: AtomicU32,
    local_go_away: AtomicBool,
    remote_go_away: AtomicBool,
    shutdown_tx: watch::Sender<bool>,
    shutdown_err: Mutex<Option<Error>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn is_closed(&self) -> bool {
        *self.shutdown_tx.borrow()
    }

    fn shutdown_error(&self) -> Error {
        lock(&self.shutdown_err)
            .clone()
            .unwrap_or(Error::SessionShutdown)
    }

    fn send(&self, header: [u8; HEADER_SIZE], body: Vec<u8>) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::SessionShutdown);
        }
        self.send_tx
            .send(Frame { header, body })
            .map_err(|_| Error::SessionShutdown)
    }

    fn go_away_header(&self, reason: u32) -> [u8; HEADER_SIZE] {
        self.local_go_away.store(true, Ordering::SeqCst);
        header(TYPE_GO_AWAY, 0, 0, reason)
    }

    /// session.go, `exitErr` + `Close`: the first error wins; every stream is force-closed.
    fn close_with(&self, err: Error) {
        {
            let mut e = lock(&self.shutdown_err);
            if e.is_none() {
                *e = Some(err);
            }
        }
        if self.shutdown_tx.send_replace(true) {
            return;
        }
        let streams: Vec<_> = lock(&self.streams).map.values().cloned().collect();
        for s in streams {
            let mut st = s.lock();
            st.state = State::Closed;
            st.wake_all();
        }
        for t in lock(&self.tasks).drain(..) {
            t.abort();
        }
    }

    /// session.go, `establishStream`: the ACK arrived, release the SYN slot.
    fn establish(&self, id: u32) {
        lock(&self.streams).inflight.remove(&id);
    }

    /// session.go, `closeStream`.
    fn close_stream(&self, id: u32) {
        let mut streams = lock(&self.streams);
        streams.inflight.remove(&id);
        streams.map.remove(&id);
    }

    /// stream.go, `sendWindowUpdate`, with the stream already locked so the frame is queued in
    /// order with the stream's other frames.
    fn send_window_update(&self, s: &StreamShared, st: &mut StreamState) -> Result<(), Error> {
        let max = self.config.max_stream_window;
        let buf_len = u32::try_from(st.recv_buf.len()).unwrap_or(u32::MAX);
        let delta = max.wrapping_sub(buf_len).wrapping_sub(st.recv_window);
        let flags = st.send_flags();
        if delta < max / 2 && flags == 0 {
            return Ok(());
        }
        st.recv_window = st.recv_window.wrapping_add(delta);
        self.send(header(TYPE_WINDOW_UPDATE, flags, s.id, delta), Vec::new())
    }

    /// stream.go, `processFlags`. Returns whether the stream is finished and must be removed.
    fn process_flags(
        &self,
        s: &StreamShared,
        st: &mut StreamState,
        flags: u16,
    ) -> Result<bool, Error> {
        let mut close = false;
        if flags & FLAG_ACK != 0 {
            if st.state == State::SynSent {
                st.state = State::Established;
            }
            self.establish(s.id);
        }
        if flags & FLAG_FIN != 0 {
            match st.state {
                State::SynSent | State::SynReceived | State::Established => {
                    st.state = State::RemoteClose;
                    st.wake_all();
                }
                State::LocalClose => {
                    st.state = State::Closed;
                    close = true;
                    st.wake_all();
                }
                other => {
                    tracing::error!(state = ?other, "yamux: unexpected FIN flag");
                    return Err(Error::UnexpectedFlag);
                }
            }
        }
        if flags & FLAG_RST != 0 {
            st.state = State::Reset;
            close = true;
            st.wake_all();
        }
        if close && let Some(t) = st.close_timer.take() {
            t.abort();
        }
        Ok(close)
    }

    /// stream.go, `Close`.
    fn close_stream_locally(self: &Arc<Self>, s: &Arc<StreamShared>) {
        let mut st = s.lock();
        let finished = match st.state {
            State::Init | State::SynSent | State::SynReceived | State::Established => {
                st.state = State::LocalClose;
                false
            }
            State::RemoteClose => {
                st.state = State::Closed;
                true
            }
            State::LocalClose | State::Closed | State::Reset => return,
        };
        if let Some(t) = st.close_timer.take() {
            t.abort();
        }
        if !finished && let Some(timeout) = self.config.stream_close_timeout {
            let inner = Arc::downgrade(self);
            let stream = s.clone();
            st.close_timer = Some(tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if let Some(inner) = inner.upgrade() {
                    // stream.go, `closeTimeout`.
                    {
                        let mut st = stream.lock();
                        st.state = State::Closed;
                        st.close_timer = None;
                        st.wake_all();
                    }
                    inner.close_stream(stream.id);
                    let _ = inner.send(
                        header(TYPE_WINDOW_UPDATE, FLAG_RST, stream.id, 0),
                        Vec::new(),
                    );
                }
            }));
        }
        let flags = st.send_flags() | FLAG_FIN;
        let _ = self.send(header(TYPE_WINDOW_UPDATE, flags, s.id, 0), Vec::new());
        st.wake_all();
        drop(st);
        if finished {
            self.close_stream(s.id);
        }
    }

    /// session.go, `incomingStream`.
    fn incoming_stream(&self, id: u32) -> Result<(), Error> {
        if self.local_go_away.load(Ordering::SeqCst) {
            return self.send(header(TYPE_WINDOW_UPDATE, FLAG_RST, id, 0), Vec::new());
        }
        let stream = StreamShared::new(id, State::SynReceived);
        let mut streams = lock(&self.streams);
        if streams.map.contains_key(&id) {
            tracing::error!("yamux: duplicate stream declared");
            let _ = self.send(self.go_away_header(GO_AWAY_PROTO_ERR), Vec::new());
            return Err(Error::DuplicateStream);
        }
        streams.map.insert(id, stream.clone());
        match self.accept_tx.try_send(stream) {
            Ok(()) => Ok(()),
            Err(_) => {
                tracing::warn!("yamux: backlog exceeded, forcing connection reset");
                streams.map.remove(&id);
                drop(streams);
                self.send(header(TYPE_WINDOW_UPDATE, FLAG_RST, id, 0), Vec::new())
            }
        }
    }
}

/// Keeps a session alive: the session closes when the last [`Session`] and [`Stream`] handle is
/// dropped. The background tasks hold the session without keeping it alive.
struct Guard(Arc<Inner>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.close_with(Error::SessionShutdown);
    }
}

/// A yamux session over one connection. Cheap to clone.
#[derive(Clone)]
pub struct Session {
    guard: Arc<Guard>,
}

impl Session {
    /// The client side: opens odd-numbered streams.
    pub fn client<T>(io: T, config: Config) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::new(io, config, true)
    }

    /// The server side: opens even-numbered streams.
    pub fn server<T>(io: T, config: Config) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::new(io, config, false)
    }

    fn new<T>(io: T, config: Config, client: bool) -> Result<Self, Error>
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
    {
        // mux.go, VerifyConfig.
        if config.accept_backlog == 0 {
            return Err(Error::Config("backlog must be positive"));
        }
        if config.keepalive_interval.is_zero() {
            return Err(Error::Config("keep-alive interval must be positive"));
        }
        if config.max_stream_window < INITIAL_STREAM_WINDOW {
            return Err(Error::Config(
                "MaxStreamWindowSize must be larger than 262144",
            ));
        }
        let (read, write) = tokio::io::split(io);
        let (accept_tx, accept_rx) = mpsc::channel(config.accept_backlog);
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            syn: Arc::new(Semaphore::new(config.accept_backlog)),
            next_id: AtomicU32::new(if client { 1 } else { 2 }),
            streams: Mutex::new(Streams::default()),
            accept_tx,
            accept_rx: tokio::sync::Mutex::new(accept_rx),
            send_tx,
            pings: Mutex::new(HashMap::new()),
            ping_id: AtomicU32::new(0),
            local_go_away: AtomicBool::new(false),
            remote_go_away: AtomicBool::new(false),
            shutdown_tx,
            shutdown_err: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            config,
        });
        let weak = Arc::downgrade(&inner);
        // The writer is not among the tasks a close aborts: it drains what was queued first.
        tokio::spawn(send_loop(
            weak.clone(),
            write,
            send_rx,
            inner.shutdown_tx.subscribe(),
        ));
        let mut tasks = vec![tokio::spawn(recv_loop(weak.clone(), BufReader::new(read)))];
        if inner.config.enable_keepalive {
            tasks.push(tokio::spawn(keepalive(weak)));
        }
        lock(&inner.tasks).extend(tasks);
        Ok(Self {
            guard: Arc::new(Guard(inner)),
        })
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.guard.0
    }

    pub fn is_closed(&self) -> bool {
        self.inner().is_closed()
    }

    /// Streams currently open (including half-closed ones).
    pub fn num_streams(&self) -> usize {
        lock(&self.inner().streams).map.len()
    }

    /// Resolves when the session has shut down, with the reason.
    pub async fn closed(&self) -> Error {
        let mut rx = self.inner().shutdown_tx.subscribe();
        let _ = rx.wait_for(|closed| *closed).await;
        self.inner().shutdown_error()
    }

    /// session.go, `OpenStream`: send the SYN now, waiting first if `accept_backlog` streams are
    /// already waiting for their ACK.
    pub async fn open(&self) -> Result<Stream, Error> {
        let inner = self.inner();
        if inner.is_closed() {
            return Err(Error::SessionShutdown);
        }
        if inner.remote_go_away.load(Ordering::SeqCst) {
            return Err(Error::RemoteGoAway);
        }
        let mut shutdown = inner.shutdown_tx.subscribe();
        let permit = tokio::select! {
            p = inner.syn.clone().acquire_owned() => p.map_err(|_| Error::SessionShutdown)?,
            _ = shutdown.wait_for(|c| *c) => return Err(Error::SessionShutdown),
        };
        let id = loop {
            let id = inner.next_id.load(Ordering::SeqCst);
            if id >= u32::MAX - 1 {
                return Err(Error::StreamsExhausted);
            }
            if inner
                .next_id
                .compare_exchange(id, id + 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break id;
            }
        };
        let stream = StreamShared::new(id, State::Init);
        {
            let mut streams = lock(&inner.streams);
            streams.map.insert(id, stream.clone());
            streams.inflight.insert(id, permit);
        }
        if let Some(timeout) = inner.config.stream_open_timeout {
            let weak = Arc::downgrade(inner);
            let task = tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if let Some(inner) = weak.upgrade()
                    && lock(&inner.streams).inflight.contains_key(&id)
                {
                    tracing::error!(
                        stream = id,
                        "yamux: aborted stream open: i/o deadline reached"
                    );
                    inner.close_with(Error::Timeout);
                }
            });
            lock(&inner.tasks).push(task);
        }
        let sent = {
            let mut st = stream.lock();
            inner.send_window_update(&stream, &mut st)
        };
        if let Err(e) = sent {
            lock(&inner.streams).inflight.remove(&id);
            return Err(e);
        }
        Ok(Stream {
            guard: self.guard.clone(),
            shared: stream,
        })
    }

    /// session.go, `AcceptStream`: the next stream the peer opened, ACKed.
    pub async fn accept(&self) -> Result<Stream, Error> {
        let inner = self.inner();
        let mut shutdown = inner.shutdown_tx.subscribe();
        let stream = {
            let mut rx = inner.accept_rx.lock().await;
            tokio::select! {
                s = rx.recv() => s.ok_or_else(|| inner.shutdown_error())?,
                _ = shutdown.wait_for(|c| *c) => return Err(inner.shutdown_error()),
            }
        };
        {
            let mut st = stream.lock();
            inner.send_window_update(&stream, &mut st)?;
        }
        Ok(Stream {
            guard: self.guard.clone(),
            shared: stream,
        })
    }

    /// session.go, `Ping`: the round-trip time.
    pub async fn ping(&self) -> Result<Duration, Error> {
        ping(self.inner()).await
    }

    /// session.go, `GoAway`: refuse new streams from the peer.
    pub fn go_away(&self) -> Result<(), Error> {
        let inner = self.inner();
        inner.send(inner.go_away_header(GO_AWAY_NORMAL), Vec::new())
    }

    /// Close the session and every stream on it.
    pub fn close(&self) {
        self.inner().close_with(Error::SessionShutdown);
    }
}

async fn ping(inner: &Arc<Inner>) -> Result<Duration, Error> {
    let (tx, rx) = oneshot::channel();
    let id = inner.ping_id.fetch_add(1, Ordering::SeqCst);
    lock(&inner.pings).insert(id, tx);
    inner.send(header(TYPE_PING, FLAG_SYN, 0, id), Vec::new())?;
    let start = Instant::now();
    let mut shutdown = inner.shutdown_tx.subscribe();
    tokio::select! {
        r = rx => r.map(|()| start.elapsed()).map_err(|_| Error::SessionShutdown),
        _ = tokio::time::sleep(inner.config.connection_write_timeout) => {
            lock(&inner.pings).remove(&id);
            Err(Error::Timeout)
        }
        _ = shutdown.wait_for(|c| *c) => Err(Error::SessionShutdown),
    }
}

async fn keepalive(weak: std::sync::Weak<Inner>) {
    loop {
        let interval = match weak.upgrade() {
            Some(inner) => inner.config.keepalive_interval,
            None => return,
        };
        tokio::time::sleep(interval).await;
        let Some(inner) = weak.upgrade() else { return };
        match ping(&inner).await {
            Ok(_) => {}
            Err(Error::SessionShutdown) => return,
            Err(e) => {
                tracing::error!(error = %e, "yamux: keepalive failed");
                inner.close_with(Error::KeepAliveTimeout);
                return;
            }
        }
    }
}

async fn send_loop<W: AsyncWrite + Unpin + Send>(
    weak: std::sync::Weak<Inner>,
    mut w: W,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    mut shutdown: watch::Receiver<bool>,
) {
    let timeout = match weak.upgrade() {
        Some(inner) => inner.config.connection_write_timeout,
        None => return,
    };
    let result: Result<(), Error> = async {
        loop {
            // `biased`: frames already queued are written before a shutdown is noticed, so a
            // GoAway announcing the close, or the FIN of a stream dropped with the session, still
            // goes out. Nothing can be queued once the session is closed.
            let next = tokio::select! {
                biased;
                f = rx.recv() => f,
                _ = shutdown.wait_for(|c| *c) => None,
            };
            let Some(frame) = next else {
                let _ = tokio::time::timeout(timeout, w.flush()).await;
                return Ok(());
            };
            let write = async {
                w.write_all(&frame.header).await?;
                if !frame.body.is_empty() {
                    w.write_all(&frame.body).await?;
                }
                // Flush once the queue is drained, not per frame.
                if rx.is_empty() {
                    w.flush().await?;
                }
                Ok::<(), io::Error>(())
            };
            match tokio::time::timeout(timeout, write).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(Error::ConnectionWriteTimeout),
            }
        }
    }
    .await;
    let _ = w.shutdown().await;
    if let (Err(e), Some(inner)) = (result, weak.upgrade()) {
        tracing::error!(error = %e, "yamux: failed to write");
        inner.close_with(e);
    }
}

async fn recv_loop<R: AsyncRead + Unpin>(weak: std::sync::Weak<Inner>, mut r: R) {
    let result = recv_frames(&weak, &mut r).await;
    if let Some(inner) = weak.upgrade() {
        inner.close_with(result.err().unwrap_or(Error::SessionShutdown));
    }
}

async fn recv_frames<R: AsyncRead + Unpin>(
    weak: &std::sync::Weak<Inner>,
    r: &mut R,
) -> Result<(), Error> {
    let mut hdr = [0u8; HEADER_SIZE];
    loop {
        r.read_exact(&mut hdr).await?;
        let Some(inner) = weak.upgrade() else {
            return Ok(());
        };
        if hdr[0] != PROTO_VERSION {
            tracing::error!(version = hdr[0], "yamux: invalid protocol version");
            return Err(Error::InvalidVersion);
        }
        let ty = hdr[1];
        let flags = u16::from_be_bytes([hdr[2], hdr[3]]);
        let id = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let len = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        match ty {
            TYPE_DATA | TYPE_WINDOW_UPDATE => {
                handle_stream_message(&inner, r, ty, flags, id, len).await?
            }
            TYPE_PING => handle_ping(&inner, flags, len)?,
            TYPE_GO_AWAY => handle_go_away(&inner, len)?,
            _ => return Err(Error::InvalidMsgType),
        }
    }
}

/// session.go, `handleStreamMessage` + stream.go, `incrSendWindow` / `readData`.
async fn handle_stream_message<R: AsyncRead + Unpin>(
    inner: &Arc<Inner>,
    r: &mut R,
    ty: u8,
    flags: u16,
    id: u32,
    len: u32,
) -> Result<(), Error> {
    if flags & FLAG_SYN != 0 {
        inner.incoming_stream(id)?;
    }
    let stream = lock(&inner.streams).map.get(&id).cloned();
    let Some(stream) = stream else {
        if ty == TYPE_DATA && len > 0 {
            tracing::warn!(stream = id, "yamux: discarding data for stream");
            tokio::io::copy(&mut r.take(u64::from(len)), &mut tokio::io::sink()).await?;
        } else {
            tracing::warn!(
                stream = id,
                ty,
                flags,
                len,
                "yamux: frame for missing stream"
            );
        }
        return Ok(());
    };

    let proto_err = |e: Error| {
        let _ = inner.send(inner.go_away_header(GO_AWAY_PROTO_ERR), Vec::new());
        e
    };

    if ty == TYPE_WINDOW_UPDATE {
        let close = {
            let mut st = stream.lock();
            let close = inner
                .process_flags(&stream, &mut st, flags)
                .map_err(proto_err)?;
            st.send_window = st.send_window.wrapping_add(len);
            if let Some(w) = st.write_waker.take() {
                w.wake();
            }
            close
        };
        if close {
            inner.close_stream(id);
        }
        return Ok(());
    }

    let close = {
        let mut st = stream.lock();
        let close = inner
            .process_flags(&stream, &mut st, flags)
            .map_err(proto_err)?;
        if len > 0 && len > st.recv_window {
            tracing::error!(
                stream = id,
                remain = st.recv_window,
                recv = len,
                "yamux: receive window exceeded"
            );
            return Err(proto_err(Error::RecvWindowExceeded));
        }
        close
    };
    if close {
        inner.close_stream(id);
    }
    if len == 0 {
        return Ok(());
    }
    // Read the body without holding the stream: only this task shrinks the receive window, so the
    // check above still holds when the bytes are added.
    let mut body = Vec::with_capacity(len as usize);
    let got = r.take(u64::from(len)).read_to_end(&mut body).await?;
    if got != len as usize {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }
    let mut st = stream.lock();
    st.recv_buf.extend(body);
    st.recv_window -= len;
    if let Some(w) = st.read_waker.take() {
        w.wake();
    }
    Ok(())
}

/// session.go, `handlePing`.
fn handle_ping(inner: &Arc<Inner>, flags: u16, id: u32) -> Result<(), Error> {
    if flags & FLAG_SYN != 0 {
        if let Err(e) = inner.send(header(TYPE_PING, FLAG_ACK, 0, id), Vec::new()) {
            tracing::warn!(error = %e, "yamux: failed to send ping reply");
        }
        return Ok(());
    }
    if let Some(tx) = lock(&inner.pings).remove(&id) {
        let _ = tx.send(());
    }
    Ok(())
}

/// session.go, `handleGoAway`.
fn handle_go_away(inner: &Arc<Inner>, code: u32) -> Result<(), Error> {
    match code {
        GO_AWAY_NORMAL => {
            inner.remote_go_away.store(true, Ordering::SeqCst);
            Ok(())
        }
        GO_AWAY_PROTO_ERR => Err(Error::GoAway("yamux protocol error")),
        GO_AWAY_INTERNAL_ERR => Err(Error::GoAway("remote yamux internal error")),
        _ => Err(Error::GoAway("unexpected go away received")),
    }
}

/// One multiplexed stream. Implements tokio's `AsyncRead` and `AsyncWrite`; `shutdown` is a
/// half-close, and dropping the stream closes it the same way.
pub struct Stream {
    guard: Arc<Guard>,
    shared: Arc<StreamShared>,
}

impl Stream {
    pub fn id(&self) -> u32 {
        self.shared.id
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.guard.0
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("id", &self.shared.id)
            .finish_non_exhaustive()
    }
}

impl AsyncRead for Stream {
    /// stream.go, `Read`.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let inner = self.inner().clone();
        let s = &self.shared;
        let mut st = s.lock();
        match st.state {
            State::RemoteClose | State::Closed if st.recv_buf.is_empty() => {
                return Poll::Ready(Ok(()));
            }
            State::Reset => return Poll::Ready(Err(Error::ConnectionReset.into())),
            _ => {}
        }
        if st.recv_buf.is_empty() {
            st.read_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = st.recv_buf.len().min(buf.remaining());
        let (a, b) = st.recv_buf.as_slices();
        let from_a = a.len().min(n);
        buf.put_slice(&a[..from_a]);
        buf.put_slice(&b[..n - from_a]);
        st.recv_buf.drain(..n);
        match inner.send_window_update(s, &mut st) {
            Ok(()) | Err(Error::SessionShutdown) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(e.into())),
        }
    }
}

impl AsyncWrite for Stream {
    /// stream.go, `write`: up to the whole send window in one frame.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let inner = self.inner().clone();
        let s = &self.shared;
        let mut st = s.lock();
        match st.state {
            State::LocalClose | State::Closed => {
                return Poll::Ready(Err(Error::StreamClosed.into()));
            }
            State::Reset => return Poll::Ready(Err(Error::ConnectionReset.into())),
            _ => {}
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if st.send_window == 0 {
            st.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let flags = st.send_flags();
        let n = (st.send_window as usize).min(buf.len());
        if let Err(e) = inner.send(header(TYPE_DATA, flags, s.id, n as u32), buf[..n].to_vec()) {
            return Poll::Ready(Err(e.into()));
        }
        st.send_window -= n as u32;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// stream.go, `Close`: a half-close.
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = self.inner().clone();
        inner.close_stream_locally(&self.shared);
        Poll::Ready(Ok(()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let inner = self.inner().clone();
        if !inner.is_closed() {
            inner.close_stream_locally(&self.shared);
        }
    }
}
