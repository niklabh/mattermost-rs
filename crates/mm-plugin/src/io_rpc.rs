//! Reading a stream that lives in the other process (io_rpc.go).
//!
//! A brokered connection carries one `io.Reader`, pulled rather than pushed: the reading side
//! writes a varint saying how many bytes it wants and then reads up to that many, and the serving
//! side answers with exactly that many or closes the connection when the source runs out. So a
//! reader that stops early ends the transfer, and the count is the reader's buffer size, not the
//! source's.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use goplugin::yamux::Stream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// The most bytes one request may ask for, which is what Go's `serveIOReader` copies through its
/// 32 KiB buffer. A larger ask is answered in full by Go; this only bounds one round trip.
pub const CHUNK: usize = 32 * 1024;

/// Go's `binary.PutVarint`: zig-zag, then the unsigned varint of the result.
pub fn put_varint(value: i64, out: &mut [u8; 10]) -> usize {
    #[allow(clippy::cast_sign_loss)] // The zig-zag mapping is defined on the bits.
    let mut u = ((value << 1) ^ (value >> 63)) as u64;
    let mut n = 0;
    while u >= 0x80 {
        out[n] = (u as u8) | 0x80;
        u >>= 7;
        n += 1;
    }
    out[n] = u as u8;
    n + 1
}

/// Go's `binary.ReadVarint` over an async source. `None` at a clean end of the stream.
pub async fn read_varint<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<i64>> {
    let mut u: u64 = 0;
    for shift in (0..64).step_by(7) {
        let mut byte = [0u8; 1];
        match r.read_exact(&mut byte).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => return Ok(None),
            Err(e) => return Err(e),
        }
        let b = u64::from(byte[0]);
        if b < 0x80 {
            if shift == 63 && b > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "varint overflows",
                ));
            }
            u |= b << shift;
            #[allow(clippy::cast_possible_wrap)] // The zig-zag mapping is defined on the bits.
            return Ok(Some(((u >> 1) as i64) ^ -((u & 1) as i64)));
        }
        u |= (b & 0x7f) << shift;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "varint overflows",
    ))
}

/// Serve `reader` on a brokered connection until the far side stops asking or the source runs out
/// (io_rpc.go, `serveIOReader`).
///
/// Go answers a request for `n` bytes with exactly `n`, and treats a short answer as the end: it
/// stops serving and closes, which is how the far side sees EOF.
pub async fn serve_reader<R>(mut reader: R, mut conn: Stream) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let Some(want) = read_varint(&mut conn).await? else {
            break;
        };
        let Ok(want) = usize::try_from(want) else {
            break; // A negative ask is nonsense; Go's LimitReader yields nothing and it stops.
        };
        let mut left = want;
        while left > 0 {
            let mut buf = vec![0u8; left.min(CHUNK)];
            let read = reader.read(&mut buf).await?;
            if read == 0 {
                // The source is spent: fewer bytes than asked for, so the transfer ends here.
                conn.shutdown().await?;
                return Ok(());
            }
            conn.write_all(&buf[..read]).await?;
            left -= read;
        }
    }
    conn.shutdown().await
}

/// The far side's `io.Reader`, over a brokered connection (io_rpc.go, `remoteIOReader`).
///
/// Every read writes the varint first, exactly as Go does, so the two sides stay in step even
/// when the buffer sizes differ.
#[derive(Debug)]
pub struct RemoteReader {
    conn: Stream,
    /// The varint for the read in flight, and how much of it has gone out.
    request: Option<(usize, [u8; 10], usize)>,
}

impl RemoteReader {
    pub fn new(conn: Stream) -> Self {
        Self {
            conn,
            request: None,
        }
    }
}

impl AsyncRead for RemoteReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.request.is_none() {
            let mut varint = [0u8; 10];
            let len = put_varint(buf.remaining() as i64, &mut varint);
            this.request = Some((len, varint, 0));
        }
        // Finish asking before reading: a partial write would desynchronise the two sides.
        while let Some((len, varint, sent)) = this.request {
            if sent == len {
                break;
            }
            let wrote = ready!(Pin::new(&mut this.conn).poll_write(cx, &varint[sent..len]))?;
            if wrote == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            this.request = Some((len, varint, sent + wrote));
        }
        let read = Pin::new(&mut this.conn).poll_read(cx, buf);
        if read.is_ready() {
            this.request = None;
        }
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for value in [0i64, 1, -1, 63, 64, 127, 128, 300, -300, i64::MAX, i64::MIN] {
            let mut buf = [0u8; 10];
            let n = put_varint(value, &mut buf);
            let decoded = tokio_test(read_varint(&mut &buf[..n])).unwrap();
            assert_eq!(decoded, Some(value), "{value}");
        }
    }

    #[test]
    fn an_empty_source_is_a_clean_end() {
        assert_eq!(tokio_test(read_varint(&mut &b""[..])).unwrap(), None);
    }

    fn tokio_test<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
