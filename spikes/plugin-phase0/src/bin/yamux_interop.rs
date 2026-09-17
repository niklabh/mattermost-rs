//! Spike 1: the Rust `yamux` crate against hashicorp/yamux v0.1.2 (`reference/dump/spike/yamuxpeer`).
//!
//!     yamux_interop server <socket>   Rust accepts; the Go peer runs `client`
//!     yamux_interop client <socket>   Rust opens; the Go peer runs `server`
//!
//! The client opens STREAMS (300) concurrent streams of SIZE (64 KiB) bytes and checks the echo,
//! then one BIG (64 MiB) stream, then idles IDLE (35 s, past hashicorp's 30 s keepalive) and
//! repeats the first round. Identical parameters to the Go peer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{AsyncReadExt, AsyncWriteExt};
use plugin_phase0::mux::{Mode, Mux, Stream};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::compat::TokioAsyncReadCompatExt;

fn env(name: &str, def: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(def)
}

fn pattern(stream: usize, size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| ((i * 31 + stream * 7) % 251) as u8)
        .collect()
}

/// Drive one stream's reads and writes from a single task and a single waker.
///
/// PHASE 0 FINDING: `yamux::Stream` (0.14) must not be split across tasks. `poll_read` sends
/// window updates through the same `futures::mpsc::Sender` that `poll_write` uses, and that
/// sender parks exactly one waker, so whichever half polls `poll_ready` second erases the other's
/// wakeup. With `futures::io::split` + two tasks the writer stalls forever with credit left,
/// Rust-to-Rust as well as against hashicorp. `SPLIT=1` restores the split version to show it.
async fn pump(mut s: Stream, mut out: Vec<u8>, echo: bool, i: usize) -> Result<Vec<u8>> {
    use futures::{AsyncRead, AsyncWrite};
    use std::pin::Pin;
    use std::task::Poll;
    let mut got = Vec::new();
    let (mut sent, mut closed, mut eof) = (0usize, false, false);
    let mut buf = vec![0u8; 32 * 1024];
    std::future::poll_fn(|cx| -> Poll<Result<()>> {
        loop {
            let mut progress = false;
            if !eof {
                match Pin::new(&mut s).poll_read(cx, &mut buf) {
                    Poll::Ready(Ok(0)) => {
                        eof = true;
                        progress = true;
                    }
                    Poll::Ready(Ok(n)) => {
                        READ.fetch_add(n, Ordering::Relaxed);
                        if echo {
                            out.extend_from_slice(&buf[..n]);
                        } else {
                            got.extend_from_slice(&buf[..n]);
                        }
                        progress = true;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(anyhow::anyhow!("read {i}: {e}")));
                    }
                    Poll::Pending => {}
                }
            }
            if sent < out.len() {
                match Pin::new(&mut s).poll_write(cx, &out[sent..]) {
                    Poll::Ready(Ok(n)) => {
                        sent += n;
                        WROTE.fetch_add(n, Ordering::Relaxed);
                        progress = true;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(anyhow::anyhow!("write {i}: {e}")));
                    }
                    Poll::Pending => {}
                }
            }
            // An echo closes once the peer has closed and everything is written back; a client
            // closes as soon as its payload is written.
            let done_writing = sent == out.len() && (!echo || eof);
            if done_writing && !closed {
                match Pin::new(&mut s).poll_close(cx) {
                    Poll::Ready(Ok(())) => {
                        closed = true;
                        progress = true;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(anyhow::anyhow!("close {i}: {e}")));
                    }
                    Poll::Pending => {}
                }
            }
            if closed && eof {
                return Poll::Ready(Ok(()));
            }
            if !progress {
                return Poll::Pending;
            }
        }
    })
    .await?;
    Ok(got)
}

async fn echo(s: Stream) -> Result<()> {
    if std::env::var("SPLIT").is_ok() {
        let (mut r, mut w) = s.split();
        futures::io::copy(&mut r, &mut w).await?;
        w.close().await?;
        return Ok(());
    }
    pump(s, Vec::new(), true, 0).await.map(|_| ())
}

static WROTE: AtomicUsize = AtomicUsize::new(0);
static READ: AtomicUsize = AtomicUsize::new(0);

async fn one(mux: Mux, i: usize, size: usize) -> Result<()> {
    let s = mux.open().await.with_context(|| format!("open {i}"))?;
    let want = pattern(i, size);
    let got = if std::env::var("SPLIT").is_ok() {
        let (mut r, mut w) = s.split();
        let writer = {
            let want = want.clone();
            tokio::spawn(async move {
                for chunk in want.chunks(8192) {
                    w.write_all(chunk).await?;
                    WROTE.fetch_add(chunk.len(), Ordering::Relaxed);
                }
                w.close().await?;
                anyhow::Ok(())
            })
        };
        let mut got = Vec::with_capacity(size);
        r.read_to_end(&mut got)
            .await
            .with_context(|| format!("read {i}"))?;
        writer.await??;
        got
    } else {
        pump(s, want.clone(), false, i).await?
    };
    if got != want {
        bail!("stream {i}: got {} bytes, want {}", got.len(), want.len());
    }
    Ok(())
}

async fn round(mux: &Mux, streams: usize, size: usize) -> Result<()> {
    let tasks: Vec<_> = (0..streams)
        .map(|i| tokio::spawn(one(mux.clone(), i, size)))
        .collect();
    for t in tasks {
        t.await??;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let (role, path) = (
        args.get(1).context("role")?.as_str(),
        args.get(2).context("socket")?,
    );
    match role {
        "server" => {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)?;
            let (conn, _) = listener.accept().await?;
            let (_mux, mut inbound, driver) = Mux::start(conn.compat(), Mode::Server);
            let mut n = 0usize;
            while let Some(s) = inbound.recv().await {
                n += 1;
                tokio::spawn(async move {
                    if let Err(e) = echo(s).await {
                        eprintln!("rust server: echo: {e:#}");
                    }
                });
            }
            let end = driver.await?;
            println!("rust server: session ended after {n} streams: {end:?}");
        }
        "client" => {
            if std::env::var("WATCH").is_ok() {
                tokio::spawn(async {
                    loop {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        eprintln!(
                            "watch: wrote {} read {}",
                            WROTE.load(Ordering::Relaxed),
                            READ.load(Ordering::Relaxed)
                        );
                    }
                });
            }
            let conn = UnixStream::connect(path).await?;
            let (mux, _inbound, _driver) = Mux::start(conn.compat(), Mode::Client);
            let (streams, size, idle, big) = (
                env("STREAMS", 300),
                env("SIZE", 1 << 16),
                env("IDLE", 35),
                env("BIG", 64 << 20),
            );
            let t = Instant::now();
            round(&mux, streams, size).await?;
            println!(
                "rust client: {streams} streams x {size} bytes ok in {:?}",
                t.elapsed()
            );
            let t = Instant::now();
            round(&mux, 1, big).await?;
            println!(
                "rust client: 1 stream x {big} bytes ok in {:?}",
                t.elapsed()
            );
            println!("rust client: idling {idle}s");
            tokio::time::sleep(Duration::from_secs(idle as u64)).await;
            round(&mux, streams, size).await.context("after idle")?;
            println!("rust client: after idle ok");
        }
        _ => bail!("role must be server or client"),
    }
    Ok(())
}
