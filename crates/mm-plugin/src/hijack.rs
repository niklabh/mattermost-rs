//! A hijacked HTTP response, across the process boundary (hijack.go).
//!
//! A plugin that takes over the connection under `ServeHTTP` (a websocket upgrade, say) calls
//! `HijackResponse` on the response writer's connection. From then on, the host holds the raw
//! connection and the plugin drives it over the same net/rpc service:
//!
//! - `HjConnRead`, `HjConnWrite`, `HjConnClose` and the three deadline setters go to the
//!   connection itself;
//! - `HjConnRWRead` and `HjConnRWWrite` go to the `bufio.ReadWriter` Go's `Hijack` hands back:
//!   the server's own 4096-byte reader, which may already hold bytes the client sent past the
//!   request, and a fresh 4096-byte writer over the connection.
//!
//! Three Go behaviours are ported because they decide which bytes reach the client:
//!
//! 1. **The host's writer is never flushed.** Go's `hijackedConnRW` has no flush RPC, so bytes the
//!    plugin writes through the buffered writer stay in the host's buffer until a write overflows
//!    it. A plugin that writes less than 4096 bytes that way and closes loses them; a larger
//!    write goes straight through (`bufio.Writer.Write` with an empty buffer).
//! 2. **A raw read skips the buffered reader.** `HjConnRead` reads the connection, so bytes the
//!    server had already buffered are only reachable through `HjConnRWRead`.
//! 3. **The plugin buffers too.** Go wraps the two RW calls in a `bufio.ReadWriter` of its own, so
//!    a read asks the host for the plugin buffer's free space, not for what the caller wanted.
//!
//! Deliberate divergences, none of them on the wire:
//! - Go's I/O errors carry the socket's addresses (`read tcp a->b: i/o timeout`); a
//!   [`HijackedConn`] is any stream, so the host answers the bare cause (`i/o timeout`,
//!   `use of closed network connection`).
//! - Go leaves a connection the plugin never closed to the garbage collector; the host here closes
//!   it when the response writer's connection ends, which Go's plugin side does as `ServeHTTP`
//!   returns.
//! - A raw read is capped at 64 KiB per call. Go allocates whatever size the plugin asks for; a
//!   short read is what the caller of `Read` must handle anyway.

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use go_netrpc::{Server, ServiceError};
use gobwire::GoTime;
use goplugin::rpc::Empty;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::http::{RemoteResponseWriter, ResponseWriter};

/// Go's `ErrNotHijacked`.
pub const ERR_NOT_HIJACKED: &str = "response is not hijacked";
/// Go's `ErrAlreadyHijacked`.
pub const ERR_ALREADY_HIJACKED: &str = "response was already hijacked";
/// Go's `ErrCannotHijack`.
pub const ERR_CANNOT_HIJACK: &str = "response cannot be hijacked";

/// `bufio`'s default size, which both Go's server reader and the plugin's wrappers use.
const BUF_SIZE: usize = 4096;
/// The most a single raw read hands back (see the module docs).
const MAX_RAW_READ: usize = 64 * 1024;

const CLOSED: &str = "use of closed network connection";
const TIMEOUT: &str = "i/o timeout";

/// A stream a host can hand over when a plugin hijacks a response.
pub trait HijackConn: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> HijackConn for T {}

/// What [`ResponseWriter::hijack`] hands over: Go's `(net.Conn, *bufio.ReadWriter)`.
pub struct Hijacked {
    /// The client's connection.
    pub conn: Box<dyn HijackConn>,
    /// Bytes the server read from `conn` past the request and has not consumed. They are what the
    /// plugin's first buffered reads return, as Go's `bufio.Reader` holds them.
    pub buffered: Vec<u8>,
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, CLOSED)
}

fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, TIMEOUT)
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "EOF")
}

/// An error kept for later, as `bufio` keeps `b.err`: `io::Error` is not `Clone`, and what
/// crosses the wire is only its text.
fn again(e: &io::Error) -> io::Error {
    io::Error::new(e.kind(), e.to_string())
}

// ---------------------------------------------------------------------------------------------
// bufio, as far as the two sides use it
// ---------------------------------------------------------------------------------------------

/// The unbuffered read under a [`GoBufReader`].
trait RawRead: Send {
    fn raw_read(&mut self, p: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
}

/// The unbuffered write under a [`GoBufWriter`]. `Ok(n)` with `n < p.len()` is a short write.
trait RawWrite: Send {
    fn raw_write(&mut self, p: &[u8]) -> impl Future<Output = io::Result<usize>> + Send;
}

/// Go's `bufio.Reader`: `Read`, and `ReadBytes` over `fill`.
struct GoBufReader {
    buf: Vec<u8>,
    r: usize,
    w: usize,
    err: Option<io::Error>,
}

impl GoBufReader {
    /// A reader of `size` bytes, already holding `buffered` (which may be larger).
    fn new(size: usize, buffered: &[u8]) -> Self {
        let mut buf = vec![0; size.max(buffered.len())];
        buf[..buffered.len()].copy_from_slice(buffered);
        Self {
            buf,
            r: 0,
            w: buffered.len(),
            err: None,
        }
    }

    fn buffered(&self) -> usize {
        self.w - self.r
    }

    /// bufio.go, `Read`: hand back what is buffered; with nothing buffered, read straight into
    /// `p` when it is at least a buffer long, and otherwise fill the buffer with one read.
    async fn read<R: RawRead>(&mut self, raw: &mut R, p: &mut [u8]) -> io::Result<usize> {
        if p.is_empty() {
            if self.buffered() > 0 {
                return Ok(0);
            }
            return self.err.take().map_or(Ok(0), Err);
        }
        if self.r == self.w {
            if let Some(e) = self.err.take() {
                return Err(e);
            }
            if p.len() >= self.buf.len() {
                return raw.raw_read(p).await;
            }
            self.r = 0;
            self.w = 0;
            match raw.raw_read(&mut self.buf).await {
                Ok(0) => return Ok(0),
                Ok(n) => self.w = n,
                Err(e) => return Err(e),
            }
        }
        let n = p.len().min(self.buffered());
        p[..n].copy_from_slice(&self.buf[self.r..self.r + n]);
        self.r += n;
        Ok(n)
    }

    /// bufio.go, `fill`: slide the unread bytes down and read once into the free space.
    async fn fill<R: RawRead>(&mut self, raw: &mut R) {
        if self.r > 0 {
            self.buf.copy_within(self.r..self.w, 0);
            self.w -= self.r;
            self.r = 0;
        }
        // Go gives up after 100 empty reads with io.ErrNoProgress.
        for _ in 0..100 {
            let w = self.w;
            match raw.raw_read(&mut self.buf[w..]).await {
                Ok(0) => {}
                Ok(n) => {
                    self.w += n;
                    return;
                }
                Err(e) => {
                    self.err = Some(e);
                    return;
                }
            }
        }
        self.err = Some(io::Error::other(
            "multiple Read calls return no data or error",
        ));
    }

    /// bufio.go, `ReadBytes`: everything up to and including `delim`. At the end of the stream
    /// it answers what it has, as `std::io::BufRead::read_until` does; Go answers that with the
    /// error beside it.
    async fn read_until<R: RawRead>(&mut self, raw: &mut R, delim: u8) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            if let Some(i) = self.buf[self.r..self.w].iter().position(|&b| b == delim) {
                line.extend_from_slice(&self.buf[self.r..=self.r + i]);
                self.r += i + 1;
                return Ok(line);
            }
            if let Some(e) = self.err.take() {
                line.extend_from_slice(&self.buf[self.r..self.w]);
                self.r = self.w;
                return match e.kind() {
                    io::ErrorKind::UnexpectedEof => Ok(line),
                    _ => Err(e),
                };
            }
            if self.buffered() == self.buf.len() {
                // ErrBufferFull: ReadBytes collects the full buffer and carries on.
                line.extend_from_slice(&self.buf[self.r..self.w]);
                self.r = self.w;
            }
            self.fill(raw).await;
        }
    }
}

/// Go's `bufio.Writer`: `Write` and `Flush`, with its sticky error.
struct GoBufWriter {
    buf: Vec<u8>,
    size: usize,
    err: Option<io::Error>,
}

impl GoBufWriter {
    fn new(size: usize) -> Self {
        Self {
            buf: Vec::with_capacity(size),
            size,
            err: None,
        }
    }

    fn available(&self) -> usize {
        self.size - self.buf.len()
    }

    /// bufio.go, `Write`: while `p` does not fit, write it straight through if the buffer is
    /// empty, else top the buffer up and flush it; then buffer the rest.
    async fn write<W: RawWrite>(&mut self, raw: &mut W, mut p: &[u8]) -> io::Result<usize> {
        let mut nn = 0;
        while p.len() > self.available() && self.err.is_none() {
            let n = if self.buf.is_empty() {
                match raw.raw_write(p).await {
                    Ok(n) => n,
                    Err(e) => {
                        self.err = Some(e);
                        0
                    }
                }
            } else {
                let n = self.available();
                self.buf.extend_from_slice(&p[..n]);
                let _ = self.flush(raw).await;
                n
            };
            nn += n;
            p = &p[n..];
        }
        if let Some(e) = &self.err {
            return Err(again(e));
        }
        self.buf.extend_from_slice(p);
        Ok(nn + p.len())
    }

    /// bufio.go, `Flush`: a short write keeps the unwritten tail and becomes the sticky error.
    async fn flush<W: RawWrite>(&mut self, raw: &mut W) -> io::Result<()> {
        if let Some(e) = &self.err {
            return Err(again(e));
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        let (n, err) = match raw.raw_write(&self.buf).await {
            Ok(n) if n < self.buf.len() => (n, Some(io::Error::from(io::ErrorKind::WriteZero))),
            Ok(n) => (n, None),
            Err(e) => (0, Some(e)),
        };
        self.buf.drain(..n);
        match err {
            Some(e) => {
                self.err = Some(again(&e));
                Err(e)
            }
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The host side: httpResponseWriterRPCServer's Hj* methods
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Deadlines {
    read: Option<Instant>,
    write: Option<Instant>,
}

/// Go's `hijackedResponse`: the connection, split so a pending read does not hold up a write
/// (Go serves each call on its own goroutine), and the server's buffered reader and writer.
struct HijackedResponse {
    read: tokio::sync::Mutex<ReadSide>,
    write: tokio::sync::Mutex<WriteSide>,
    deadlines: watch::Sender<Deadlines>,
    closed: watch::Sender<bool>,
}

struct ReadSide {
    conn: ConnIo<ReadHalf<Box<dyn HijackConn>>>,
    bufr: GoBufReader,
}

struct WriteSide {
    conn: ConnIo<WriteHalf<Box<dyn HijackConn>>>,
    bufw: GoBufWriter,
}

/// One half of the connection, whose reads or writes honour the deadlines and a close from
/// another call while they wait, as a Go `net.Conn`'s do.
struct ConnIo<H> {
    half: Option<H>,
    deadlines: watch::Receiver<Deadlines>,
    closed: watch::Receiver<bool>,
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

impl<H> ConnIo<H> {
    fn new(half: H, deadlines: &watch::Sender<Deadlines>, closed: &watch::Sender<bool>) -> Self {
        Self {
            half: Some(half),
            deadlines: deadlines.subscribe(),
            closed: closed.subscribe(),
        }
    }

    /// The deadline to wait under, or the error Go answers before touching the connection.
    fn check(&mut self, pick: fn(&Deadlines) -> Option<Instant>) -> io::Result<Option<Instant>> {
        if *self.closed.borrow_and_update() || self.half.is_none() {
            return Err(closed());
        }
        let deadline = pick(&self.deadlines.borrow_and_update());
        if deadline.is_some_and(|d| d <= Instant::now()) {
            return Err(timeout());
        }
        Ok(deadline)
    }
}

impl<H: AsyncRead + Unpin + Send> RawRead for ConnIo<H> {
    async fn raw_read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        loop {
            let deadline = self.check(|d| d.read)?;
            let Some(half) = self.half.as_mut() else {
                return Err(closed());
            };
            tokio::select! {
                read = half.read(p) => {
                    // Go's net.Conn answers the end of the stream as io.EOF, not as 0.
                    return match read {
                        Ok(0) if !p.is_empty() => Err(eof()),
                        other => other,
                    };
                }
                () = until(deadline) => return Err(timeout()),
                changed = self.deadlines.changed() => changed.map_err(|_| closed())?,
                changed = self.closed.changed() => changed.map_err(|_| closed())?,
            }
        }
    }
}

impl<H: AsyncWrite + Unpin + Send> RawWrite for ConnIo<H> {
    async fn raw_write(&mut self, p: &[u8]) -> io::Result<usize> {
        loop {
            let deadline = self.check(|d| d.write)?;
            let Some(half) = self.half.as_mut() else {
                return Err(closed());
            };
            tokio::select! {
                written = half.write_all(p) => return written.map(|()| p.len()),
                () = until(deadline) => return Err(timeout()),
                changed = self.deadlines.changed() => changed.map_err(|_| closed())?,
                changed = self.closed.changed() => changed.map_err(|_| closed())?,
            }
        }
    }
}

impl HijackedResponse {
    fn new(hijacked: Hijacked) -> Self {
        let (read, write) = tokio::io::split(hijacked.conn);
        let deadlines = watch::Sender::new(Deadlines::default());
        let closed = watch::Sender::new(false);
        Self {
            read: tokio::sync::Mutex::new(ReadSide {
                conn: ConnIo::new(read, &deadlines, &closed),
                bufr: GoBufReader::new(BUF_SIZE, &hijacked.buffered),
            }),
            write: tokio::sync::Mutex::new(WriteSide {
                conn: ConnIo::new(write, &deadlines, &closed),
                bufw: GoBufWriter::new(BUF_SIZE),
            }),
            deadlines,
            closed,
        }
    }

    /// `HjConnRWRead`: a read of `len` bytes through the server's buffered reader.
    async fn rw_read(&self, len: usize) -> io::Result<Vec<u8>> {
        let mut side = self.read.lock().await;
        let ReadSide { conn, bufr } = &mut *side;
        let mut data = vec![0; len];
        let n = bufr.read(conn, &mut data).await?;
        data.truncate(n);
        Ok(data)
    }

    /// `HjConnRWWrite`: a write through the server's buffered writer, which nothing flushes.
    async fn rw_write(&self, b: &[u8]) -> io::Result<usize> {
        let mut side = self.write.lock().await;
        let WriteSide { conn, bufw } = &mut *side;
        bufw.write(conn, b).await
    }

    /// `HjConnRead`: a read of the connection itself, past whatever the server buffered.
    async fn conn_read(&self, size: usize) -> io::Result<Vec<u8>> {
        let mut side = self.read.lock().await;
        let mut data = vec![0; size.min(MAX_RAW_READ)];
        let n = side.conn.raw_read(&mut data).await?;
        data.truncate(n);
        Ok(data)
    }

    /// `HjConnWrite`.
    async fn conn_write(&self, b: &[u8]) -> io::Result<usize> {
        let mut side = self.write.lock().await;
        side.conn.raw_write(b).await
    }

    /// `HjConnClose`: a second close fails, and a call waiting on the connection gives up.
    async fn close(&self) -> io::Result<()> {
        if self.closed.send_replace(true) {
            return Err(closed());
        }
        // Dropping both halves drops the stream, which is the close; a waiting call has seen the
        // flag and let go of its half's lock.
        self.write.lock().await.conn.half.take();
        self.read.lock().await.conn.half.take();
        Ok(())
    }

    fn set_deadline(&self, t: &GoTime, read: bool, write: bool) {
        let at = instant(t);
        self.deadlines.send_modify(|d| {
            if read {
                d.read = at;
            }
            if write {
                d.write = at;
            }
        });
    }
}

/// A Go deadline as a tokio instant: the zero time is no deadline, and a time already past
/// is due now.
fn instant(t: &GoTime) -> Option<Instant> {
    if t.sec == 0 && t.nsec == 0 {
        return None;
    }
    let unix = t.unix();
    let nanos = u32::try_from(t.nsec).unwrap_or(0);
    let at = match u64::try_from(unix) {
        Ok(secs) => UNIX_EPOCH + Duration::new(secs, nanos),
        Err(_) => UNIX_EPOCH,
    };
    let now = Instant::now();
    Some(match at.duration_since(SystemTime::now()) {
        Ok(ahead) => now + ahead,
        Err(_) => now,
    })
}

/// The slot a response writer's hijack lives in once `HijackResponse` succeeds.
type Slot = Arc<Mutex<Option<Arc<HijackedResponse>>>>;

fn hijacked(slot: &Slot) -> Result<Arc<HijackedResponse>, ServiceError> {
    slot.lock()
        .map_err(|_| ServiceError("the response writer is poisoned".into()))?
        .clone()
        .ok_or_else(|| ServiceError(ERR_NOT_HIJACKED.into()))
}

fn service(e: io::Error) -> ServiceError {
    ServiceError(e.to_string())
}

/// Register the hijack methods on a response writer's server (hijack.go,
/// `httpResponseWriterRPCServer`).
pub(crate) fn register<W: ResponseWriter>(server: &mut Server, writer: &Arc<Mutex<W>>) {
    let slot: Slot = Arc::default();

    let (w, s) = (Arc::clone(writer), Arc::clone(&slot));
    server.register("Plugin.HijackResponse", move |_: Empty| {
        let result = hijack(&w, &s);
        async move { result.map(|()| Empty {}) }
    });

    let s = Arc::clone(&slot);
    server.register("Plugin.HjConnRWRead", move |b: Vec<u8>| {
        let s = Arc::clone(&s);
        async move { hijacked(&s)?.rw_read(b.len()).await.map_err(service) }
    });

    let s = Arc::clone(&slot);
    server.register("Plugin.HjConnRWWrite", move |b: Vec<u8>| {
        let s = Arc::clone(&s);
        async move {
            let n = hijacked(&s)?.rw_write(&b).await.map_err(service)?;
            Ok::<_, ServiceError>(n as i64)
        }
    });

    let s = Arc::clone(&slot);
    server.register("Plugin.HjConnRead", move |size: i64| {
        let s = Arc::clone(&s);
        async move {
            let hjr = hijacked(&s)?;
            // Go slices a buffer by `size`, and a negative one panics its whole server.
            let size = usize::try_from(size)
                .map_err(|_| ServiceError(format!("slice bounds out of range [:{size}]")))?;
            hjr.conn_read(size).await.map_err(service)
        }
    });

    let s = Arc::clone(&slot);
    server.register("Plugin.HjConnWrite", move |b: Vec<u8>| {
        let s = Arc::clone(&s);
        async move {
            let n = hijacked(&s)?.conn_write(&b).await.map_err(service)?;
            Ok::<_, ServiceError>(n as i64)
        }
    });

    let s = Arc::clone(&slot);
    server.register("Plugin.HjConnClose", move |_: Empty| {
        let s = Arc::clone(&s);
        async move {
            hijacked(&s)?.close().await.map_err(service)?;
            Ok::<_, ServiceError>(Empty {})
        }
    });

    for (name, read, write) in [
        ("Plugin.HjConnSetDeadline", true, true),
        ("Plugin.HjConnSetReadDeadline", true, false),
        ("Plugin.HjConnSetWriteDeadline", false, true),
    ] {
        let s = Arc::clone(&slot);
        server.register(name, move |t: GoTime| {
            let result = hijacked(&s).map(|hjr| hjr.set_deadline(&t, read, write));
            async move { result.map(|()| Empty {}) }
        });
    }
}

/// hijack.go, `HijackResponse`.
fn hijack<W: ResponseWriter>(writer: &Mutex<W>, slot: &Slot) -> Result<(), ServiceError> {
    let poisoned = |_| ServiceError("the response writer is poisoned".into());
    let mut slot = slot.lock().map_err(poisoned)?;
    if slot.is_some() {
        return Err(ServiceError(ERR_ALREADY_HIJACKED.into()));
    }
    let taken = writer
        .lock()
        .map_err(|_| ServiceError("the response writer is poisoned".into()))?
        .hijack()
        .ok_or_else(|| ServiceError(ERR_CANNOT_HIJACK.into()))?
        .map_err(service)?;
    *slot = Some(Arc::new(HijackedResponse::new(taken)));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The plugin side: hijackedConn, hijackedConnRW and httpResponseWriterRPCClient.Hijack
// ---------------------------------------------------------------------------------------------

/// A net/rpc failure as the `io::Error` a reader or writer answers.
///
/// Go's plugin sees the host's `io.EOF` as `rpc.ServerError("EOF")`, which is not `io.EOF`; a
/// Rust reader has no way to say "end" except `Ok(0)`, so the callers below answer that.
fn io_error(e: go_netrpc::Error) -> io::Error {
    match e {
        go_netrpc::Error::Server(message) if message == "EOF" => eof(),
        other => io::Error::other(other.to_string()),
    }
}

fn end_as_zero(r: io::Result<usize>) -> io::Result<usize> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
        other => other,
    }
}

impl RemoteResponseWriter {
    /// Go's `http.Hijacker`: take over the client's connection.
    ///
    /// Answers the connection itself and a buffered reader-writer over the host's own buffers,
    /// as Go's `Hijack` does. Both live on this writer's connection, which the SDK closes when
    /// `serve_http` returns, so a hijacked connection is only usable inside the hook.
    pub async fn hijack(&self) -> Result<(HijackedConn, HijackedReadWriter), go_netrpc::Error> {
        self.client()
            .call::<_, Empty>("Plugin.HijackResponse", &Empty {})
            .await?;
        let conn = HijackedConn {
            client: self.client().clone(),
        };
        let rw = HijackedReadWriter {
            raw: RemoteRw {
                client: self.client().clone(),
            },
            reader: GoBufReader::new(BUF_SIZE, &[]),
            writer: GoBufWriter::new(BUF_SIZE),
        };
        Ok((conn, rw))
    }
}

/// The client's connection, held by the host (hijack.go, `hijackedConn`).
pub struct HijackedConn {
    client: go_netrpc::Client,
}

impl HijackedConn {
    /// Read what the connection has, up to `buf.len()`, bypassing the host's buffered reader.
    /// `Ok(0)` is the end of the stream.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let data: io::Result<Vec<u8>> = self
            .client
            .call("Plugin.HjConnRead", &(buf.len() as i64))
            .await
            .map_err(io_error);
        end_as_zero(data.map(|data| {
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            n
        }))
    }

    pub async fn write(&self, b: &[u8]) -> io::Result<usize> {
        let n: i64 = self
            .client
            .call("Plugin.HjConnWrite", &b.to_vec())
            .await
            .map_err(io_error)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    pub async fn close(&self) -> io::Result<()> {
        self.client
            .call::<_, Empty>("Plugin.HjConnClose", &Empty {})
            .await
            .map(|_| ())
            .map_err(io_error)
    }

    /// Both deadlines. Go's zero `Time` (`GoTime::default()`) clears them.
    pub async fn set_deadline(&self, t: GoTime) -> io::Result<()> {
        self.deadline("Plugin.HjConnSetDeadline", t).await
    }

    pub async fn set_read_deadline(&self, t: GoTime) -> io::Result<()> {
        self.deadline("Plugin.HjConnSetReadDeadline", t).await
    }

    pub async fn set_write_deadline(&self, t: GoTime) -> io::Result<()> {
        self.deadline("Plugin.HjConnSetWriteDeadline", t).await
    }

    async fn deadline(&self, method: &str, t: GoTime) -> io::Result<()> {
        self.client
            .call::<_, Empty>(method, &t)
            .await
            .map(|_| ())
            .map_err(io_error)
    }
}

/// hijack.go, `hijackedConnRW`: the host's buffered reader and writer, one call each.
struct RemoteRw {
    client: go_netrpc::Client,
}

impl RawRead for RemoteRw {
    async fn raw_read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        // Go sends the buffer itself, whose length is all the host uses.
        let data: Vec<u8> = self
            .client
            .call("Plugin.HjConnRWRead", &p.to_vec())
            .await
            .map_err(io_error)?;
        let n = data.len().min(p.len());
        p[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

impl RawWrite for RemoteRw {
    async fn raw_write(&mut self, p: &[u8]) -> io::Result<usize> {
        let n: i64 = self
            .client
            .call("Plugin.HjConnRWWrite", &p.to_vec())
            .await
            .map_err(io_error)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// The `*bufio.ReadWriter` Go's `Hijack` answers: 4096-byte buffers on this side, over the host's
/// buffered reader and writer.
///
/// [`HijackedReadWriter::flush`] only empties *this* side's buffer. The host's writer is never
/// flushed (see the module docs), so bytes that must reach the client go through
/// [`HijackedConn::write`] or in writes of more than 4096 bytes.
pub struct HijackedReadWriter {
    raw: RemoteRw,
    reader: GoBufReader,
    writer: GoBufWriter,
}

impl HijackedReadWriter {
    /// `Ok(0)` is the end of the stream.
    pub async fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        end_as_zero(self.reader.read(&mut self.raw, p).await)
    }

    /// Go's `ReadBytes`: up to and including `delim`, or what is left at the end of the stream.
    pub async fn read_until(&mut self, delim: u8) -> io::Result<Vec<u8>> {
        self.reader.read_until(&mut self.raw, delim).await
    }

    pub async fn write(&mut self, p: &[u8]) -> io::Result<usize> {
        self.writer.write(&mut self.raw, p).await
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.writer.flush(&mut self.raw).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw side that answers scripted reads and records every call's length.
    #[derive(Default)]
    struct Script {
        reads: Vec<io::Result<Vec<u8>>>,
        asked: Vec<usize>,
        written: Vec<Vec<u8>>,
        short: bool,
    }

    impl RawRead for Script {
        async fn raw_read(&mut self, p: &mut [u8]) -> io::Result<usize> {
            self.asked.push(p.len());
            if self.reads.is_empty() {
                return Err(eof());
            }
            let data = self.reads.remove(0)?;
            p[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        }
    }

    impl RawWrite for Script {
        async fn raw_write(&mut self, p: &[u8]) -> io::Result<usize> {
            self.written.push(p.to_vec());
            Ok(if self.short { p.len() / 2 } else { p.len() })
        }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Buffered bytes come back first, then a small read fills the whole buffer and a large one
    /// goes straight to the connection (bufio.go, `Read`).
    #[test]
    fn read_serves_the_buffer_then_fills_or_bypasses() {
        block_on(async {
            let mut raw = Script {
                reads: vec![Ok(b"abcdef".to_vec()), Ok(b"direct".to_vec())],
                ..Default::default()
            };
            let mut r = GoBufReader::new(8, b"xy");
            let mut p = [0; 4];
            assert_eq!(r.read(&mut raw, &mut p).await.unwrap(), 2);
            assert_eq!(&p[..2], b"xy");
            assert!(raw.asked.is_empty(), "buffered bytes needed no read");

            assert_eq!(r.read(&mut raw, &mut p).await.unwrap(), 4);
            assert_eq!(&p, b"abcd");
            assert_eq!(raw.asked, [8], "a small read fills the whole buffer");
            assert_eq!(r.read(&mut raw, &mut p).await.unwrap(), 2);
            assert_eq!(&p[..2], b"ef");

            let mut big = [0; 8];
            assert_eq!(r.read(&mut raw, &mut big).await.unwrap(), 6);
            assert_eq!(&big[..6], b"direct");
            assert_eq!(raw.asked, [8, 8], "a read of a buffer's length bypasses it");

            let e = r.read(&mut raw, &mut p).await.unwrap_err();
            assert_eq!(e.to_string(), "EOF");
        });
    }

    /// An empty read answers 0 while bytes are buffered, and only then the kept error.
    #[test]
    fn empty_read_reports_the_kept_error_only_when_drained() {
        block_on(async {
            let mut raw = Script::default();
            let mut r = GoBufReader::new(8, b"z");
            r.err = Some(eof());
            assert_eq!(r.read(&mut raw, &mut []).await.unwrap(), 0);
            let mut p = [0; 1];
            assert_eq!(r.read(&mut raw, &mut p).await.unwrap(), 1);
            assert!(r.read(&mut raw, &mut []).await.is_err());
            assert_eq!(r.read(&mut raw, &mut []).await.unwrap(), 0, "taken once");
        });
    }

    /// `ReadBytes` reads into the free space only, keeps a line longer than the buffer, and
    /// answers the tail at the end of the stream.
    #[test]
    fn read_until_fills_the_free_space_and_spans_buffers() {
        block_on(async {
            let mut raw = Script {
                reads: vec![
                    Ok(b"ab".to_vec()),
                    Ok(b"cdef".to_vec()),
                    Ok(b"gh\nij".to_vec()),
                ],
                ..Default::default()
            };
            let mut r = GoBufReader::new(6, &[]);
            assert_eq!(r.read_until(&mut raw, b'\n').await.unwrap(), b"abcdefgh\n");
            assert_eq!(raw.asked, [6, 4, 6], "fill asks for the free space");
            assert_eq!(r.read_until(&mut raw, b'\n').await.unwrap(), b"ij");
        });
    }

    /// A write larger than the buffer goes straight through when the buffer is empty; a small one
    /// stays buffered until something flushes it.
    #[test]
    fn write_bypasses_an_empty_buffer_and_holds_small_writes() {
        block_on(async {
            let mut raw = Script::default();
            let mut w = GoBufWriter::new(4);
            assert_eq!(w.write(&mut raw, b"abcdef").await.unwrap(), 6);
            assert_eq!(raw.written, [b"abcdef".to_vec()]);
            assert_eq!(w.write(&mut raw, b"gh").await.unwrap(), 2);
            assert_eq!(raw.written.len(), 1, "a small write is held");
            assert_eq!(w.write(&mut raw, b"ijk").await.unwrap(), 3);
            assert_eq!(
                raw.written[1], b"ghij",
                "the buffer is topped up, then flushed"
            );
            w.flush(&mut raw).await.unwrap();
            assert_eq!(raw.written[2], b"k");
        });
    }

    /// A short write keeps the unwritten tail and makes every later call fail.
    #[test]
    fn short_flush_is_sticky() {
        block_on(async {
            let mut raw = Script {
                short: true,
                ..Default::default()
            };
            let mut w = GoBufWriter::new(8);
            w.write(&mut raw, b"abcd").await.unwrap();
            assert!(w.flush(&mut raw).await.is_err());
            assert_eq!(w.buf, b"cd");
            assert!(w.write(&mut raw, b"e").await.is_err());
            assert!(w.flush(&mut raw).await.is_err());
        });
    }

    fn over(conn: tokio::io::DuplexStream, buffered: &[u8]) -> Arc<HijackedResponse> {
        Arc::new(HijackedResponse::new(Hijacked {
            conn: Box::new(conn),
            buffered: buffered.to_vec(),
        }))
    }

    /// A deadline already past fails the read even with bytes waiting, as Go's poller checks it
    /// before reading; clearing it with the zero time reads them.
    #[test]
    fn a_past_deadline_fails_a_read_that_could_succeed() {
        block_on(async {
            let (conn, mut client) = tokio::io::duplex(64);
            client.write_all(b"data").await.unwrap();
            let hjr = over(conn, b"");
            let past = GoTime::from_unix(1, 0, gobwire::Zone::Utc);
            for _ in 0..20 {
                hjr.set_deadline(&past, true, false);
                let e = hjr.conn_read(8).await.unwrap_err();
                assert_eq!(e.to_string(), TIMEOUT);
            }
            hjr.set_deadline(&past, false, true);
            assert_eq!(
                hjr.conn_read(8).await.unwrap_err().to_string(),
                TIMEOUT,
                "write only"
            );
            hjr.set_deadline(&GoTime::default(), true, false);
            assert_eq!(hjr.conn_read(8).await.unwrap(), b"data");
            assert_eq!(hjr.conn_write(b"x").await.unwrap_err().to_string(), TIMEOUT);
            hjr.set_deadline(&GoTime::default(), true, true);
            assert_eq!(hjr.conn_write(b"x").await.unwrap(), 1);
        });
    }

    /// The end of the stream is Go's `io.EOF`, whose text is what crosses; and the server's
    /// buffered bytes are the buffered reader's, not the raw read's.
    #[test]
    fn the_end_is_eof_and_a_raw_read_skips_the_buffer() {
        block_on(async {
            let (conn, mut client) = tokio::io::duplex(64);
            client.write_all(b"raw").await.unwrap();
            drop(client);
            let hjr = over(conn, b"buffered");
            assert_eq!(hjr.conn_read(8).await.unwrap(), b"raw");
            assert_eq!(hjr.conn_read(8).await.unwrap_err().to_string(), "EOF");
            assert_eq!(hjr.rw_read(BUF_SIZE).await.unwrap(), b"buffered");
            assert_eq!(hjr.rw_read(BUF_SIZE).await.unwrap_err().to_string(), "EOF");
        });
    }

    /// A close wakes a read that is waiting, and a second close fails.
    #[test]
    fn close_wakes_a_waiting_read_and_happens_once() {
        block_on(async {
            let (conn, _client) = tokio::io::duplex(64);
            let hjr = over(conn, b"");
            let waiting = tokio::spawn({
                let hjr = Arc::clone(&hjr);
                async move { hjr.conn_read(8).await }
            });
            tokio::task::yield_now().await;
            hjr.close().await.unwrap();
            let e = waiting.await.unwrap().unwrap_err();
            assert_eq!(e.to_string(), CLOSED);
            assert_eq!(hjr.close().await.unwrap_err().to_string(), CLOSED);
            assert_eq!(hjr.conn_write(b"x").await.unwrap_err().to_string(), CLOSED);
        });
    }

    #[test]
    fn deadlines_zero_is_none_and_past_is_now() {
        assert!(instant(&GoTime::default()).is_none());
        let past = instant(&GoTime::from_unix(1, 0, gobwire::Zone::Utc)).unwrap();
        assert!(past <= Instant::now());
        let far = instant(&GoTime::from_unix(4_000_000_000, 0, gobwire::Zone::Utc));
        assert!(far.unwrap() > Instant::now() + Duration::from_secs(3600));
    }
}
