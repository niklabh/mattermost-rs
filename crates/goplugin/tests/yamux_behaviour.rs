//! yamux behaviour a well-behaved Go peer never provokes, against a scripted peer writing raw
//! frames: session.go's and stream.go's error and timeout branches.

use std::time::Duration;

use goplugin::yamux::{Config, Error, INITIAL_STREAM_WINDOW, Session};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("test timed out")
}

const DATA: u8 = 0;
const WINDOW: u8 = 1;
const PING: u8 = 2;
const GO_AWAY: u8 = 3;
const SYN: u16 = 1;
const ACK: u16 = 2;
const FIN: u16 = 4;
const RST: u16 = 8;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Hdr {
    version: u8,
    ty: u8,
    flags: u16,
    id: u32,
    len: u32,
}

struct Raw {
    io: DuplexStream,
}

impl Raw {
    async fn send(&mut self, ty: u8, flags: u16, id: u32, len: u32, body: &[u8]) {
        let mut h = vec![0u8, ty];
        h.extend(flags.to_be_bytes());
        h.extend(id.to_be_bytes());
        h.extend(len.to_be_bytes());
        h.extend_from_slice(body);
        self.io.write_all(&h).await.unwrap();
    }

    /// The next frame, skipping pings the session's keepalive sends; `None` at end of input.
    async fn frame(&mut self) -> Option<(Hdr, Vec<u8>)> {
        loop {
            let mut h = [0u8; 12];
            self.io.read_exact(&mut h).await.ok()?;
            let hdr = Hdr {
                version: h[0],
                ty: h[1],
                flags: u16::from_be_bytes([h[2], h[3]]),
                id: u32::from_be_bytes(h[4..8].try_into().unwrap()),
                len: u32::from_be_bytes(h[8..12].try_into().unwrap()),
            };
            let mut body = vec![];
            if hdr.ty == DATA {
                body = vec![0u8; hdr.len as usize];
                self.io.read_exact(&mut body).await.ok()?;
            }
            if hdr.ty == PING && hdr.flags == SYN {
                continue;
            }
            return Some((hdr, body));
        }
    }
}

fn quiet() -> Config {
    Config {
        enable_keepalive: false,
        ..Config::default()
    }
}

fn pair(config: Config, client: bool) -> (Session, Raw) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let sess = if client {
        Session::client(a, config)
    } else {
        Session::server(a, config)
    }
    .unwrap();
    (sess, Raw { io: b })
}

#[tokio::test]
async fn clients_open_odd_streams_and_servers_even_with_an_eager_syn() {
    within(async {
        for (client, first) in [(true, 1u32), (false, 2)] {
            let (sess, mut raw) = pair(quiet(), client);
            let _a = sess.open().await.unwrap();
            let _b = sess.open().await.unwrap();
            // The SYN goes out on open, as a window update, before anything is written.
            let (h, _) = raw.frame().await.unwrap();
            assert_eq!(
                h,
                Hdr {
                    version: 0,
                    ty: WINDOW,
                    flags: SYN,
                    id: first,
                    len: 0
                }
            );
            let (h, _) = raw.frame().await.unwrap();
            assert_eq!(h.id, first + 2);
        }
    })
    .await
}

#[tokio::test]
async fn accepting_acks_and_writes_carry_the_whole_window() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!(
            h,
            Hdr {
                version: 0,
                ty: WINDOW,
                flags: ACK,
                id: 2,
                len: 0
            }
        );

        // A write larger than the window sends exactly the window, then waits for credit.
        let big = vec![7u8; INITIAL_STREAM_WINDOW as usize + 10];
        let writer = tokio::spawn(async move {
            s.write_all(&big).await.unwrap();
            s
        });
        let (h, body) = raw.frame().await.unwrap();
        assert_eq!(
            (h.ty, h.len, body.len()),
            (DATA, INITIAL_STREAM_WINDOW, INITIAL_STREAM_WINDOW as usize)
        );
        raw.send(WINDOW, 0, 2, 10, &[]).await;
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.len), (DATA, 10));
        writer.await.unwrap();
    })
    .await
}

#[tokio::test]
async fn reads_return_credit_once_half_the_window_is_consumed() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        raw.frame().await.unwrap(); // ACK
        let half = INITIAL_STREAM_WINDOW / 2;
        raw.send(DATA, 0, 2, half - 1, &vec![1u8; (half - 1) as usize])
            .await;
        let mut buf = vec![0u8; half as usize];
        let mut read = 0;
        while read < (half - 1) as usize {
            read += s.read(&mut buf[read..]).await.unwrap();
        }
        // Not yet half: no update. One more byte crosses it.
        raw.send(DATA, 0, 2, 1, &[1]).await;
        s.read_exact(&mut buf[..1]).await.unwrap();
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.flags, h.id, h.len), (WINDOW, 0, 2, half));
    })
    .await
}

#[tokio::test]
async fn exceeding_the_receive_window_is_a_protocol_error() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let _s = sess.accept().await.unwrap();
        raw.frame().await.unwrap();
        let over = INITIAL_STREAM_WINDOW + 1;
        raw.send(DATA, 0, 2, over, &vec![0u8; over as usize]).await;
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.len), (GO_AWAY, 1));
        assert!(matches!(sess.closed().await, Error::RecvWindowExceeded));
    })
    .await
}

#[tokio::test]
async fn half_closes_in_both_orders() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        raw.frame().await.unwrap();
        // The peer closes first: reads end, writes still work.
        raw.send(WINDOW, FIN, 2, 0, &[]).await;
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).await.unwrap();
        s.write_all(b"after").await.unwrap();
        let (h, body) = raw.frame().await.unwrap();
        assert_eq!((h.ty, body.as_slice()), (DATA, &b"after"[..]));
        s.shutdown().await.unwrap();
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.flags), (WINDOW, FIN));
        assert_eq!(sess.num_streams(), 0, "closed on both sides: removed");

        // This side closes first: writes fail, reads continue until the peer's FIN.
        raw.send(WINDOW, SYN, 4, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        raw.frame().await.unwrap();
        s.shutdown().await.unwrap();
        raw.frame().await.unwrap();
        assert!(s.write_all(b"x").await.is_err());
        raw.send(DATA, FIN, 4, 3, b"end").await;
        let mut got = Vec::new();
        s.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"end");
        assert_eq!(sess.num_streams(), 0);
    })
    .await
}

#[tokio::test]
async fn a_reset_fails_reads_and_writes() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        raw.frame().await.unwrap();
        raw.send(WINDOW, RST, 2, 0, &[]).await;
        let mut buf = [0u8; 4];
        let err = s.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
        assert!(s.write_all(b"x").await.is_err());
    })
    .await
}

#[tokio::test]
async fn incoming_streams_beyond_the_backlog_are_reset() {
    within(async {
        let (sess, mut raw) = pair(
            Config {
                accept_backlog: 3,
                ..quiet()
            },
            true,
        );
        for id in [2, 4, 6, 8, 10] {
            raw.send(WINDOW, SYN, id, 0, &[]).await;
        }
        let mut resets = Vec::new();
        for _ in 0..2 {
            let (h, _) = raw.frame().await.unwrap();
            assert_eq!(h.flags, RST);
            resets.push(h.id);
        }
        assert_eq!(resets, [8, 10]);
        // A reset stream is gone: its data is discarded, and only the three accepted remain.
        raw.send(DATA, 0, 8, 3, b"xyz").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sess.num_streams(), 3);
        for id in [2, 4, 6] {
            assert_eq!(sess.accept().await.unwrap().id(), id);
        }
    })
    .await
}

/// session.go, `synCh`: an opener waits once `accept_backlog` streams are un-ACKed, so it never
/// overruns the peer's backlog.
#[tokio::test]
async fn opening_waits_while_the_backlog_of_unacked_streams_is_full() {
    within(async {
        let (sess, mut raw) = pair(
            Config {
                accept_backlog: 2,
                ..quiet()
            },
            true,
        );
        let a = sess.open().await.unwrap();
        let _b = sess.open().await.unwrap();
        let third = tokio::spawn({
            let sess = sess.clone();
            async move { sess.open().await.map(|s| s.id()) }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!third.is_finished(), "a third open must wait for an ACK");
        raw.frame().await.unwrap();
        raw.frame().await.unwrap();
        raw.send(WINDOW, ACK, a.id(), 0, &[]).await;
        assert_eq!(third.await.unwrap().unwrap(), 5);
    })
    .await
}

#[tokio::test]
async fn protocol_violations_end_the_session() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.io
            .write_all(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        assert!(matches!(sess.closed().await, Error::InvalidVersion));

        let (sess, mut raw) = pair(quiet(), true);
        raw.send(9, 0, 0, 0, &[]).await;
        assert!(matches!(sess.closed().await, Error::InvalidMsgType));

        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        assert!(matches!(sess.closed().await, Error::DuplicateStream));

        let (sess, mut raw) = pair(quiet(), true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        raw.send(WINDOW, FIN, 2, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        let mut sink = Vec::new();
        s.read_to_end(&mut sink).await.unwrap();
        // A second FIN on a stream the peer already closed.
        raw.send(WINDOW, FIN, 2, 0, &[]).await;
        assert!(matches!(sess.closed().await, Error::UnexpectedFlag));

        for (code, text) in [
            (1, "yamux protocol error"),
            (2, "remote yamux internal error"),
            (7, "unexpected go away received"),
        ] {
            let (sess, mut raw) = pair(quiet(), true);
            raw.send(GO_AWAY, 0, 0, code, &[]).await;
            let err = sess.closed().await;
            assert_eq!(err.to_string(), text);
        }
    })
    .await
}

#[tokio::test]
async fn a_normal_go_away_refuses_opens_and_a_local_one_resets_incoming() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(GO_AWAY, 0, 0, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(sess.open().await, Err(Error::RemoteGoAway)));
        assert!(!sess.is_closed());

        sess.go_away().unwrap();
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.len), (GO_AWAY, 0));
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.flags, h.id), (WINDOW, RST, 2));
    })
    .await
}

#[tokio::test]
async fn pings_are_answered_and_timed() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(PING, SYN, 0, 42, &[]).await;
        // `frame` skips SYN pings; the ACK comes through.
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.flags, h.len), (PING, ACK, 42));

        let pinging = tokio::spawn({
            let sess = sess.clone();
            async move { sess.ping().await }
        });
        let mut h = [0u8; 12];
        raw.io.read_exact(&mut h).await.unwrap();
        assert_eq!((h[1], u16::from_be_bytes([h[2], h[3]])), (PING, SYN));
        let id = u32::from_be_bytes(h[8..12].try_into().unwrap());
        raw.send(PING, ACK, 0, id, &[]).await;
        assert!(pinging.await.unwrap().is_ok());
    })
    .await
}

#[tokio::test]
async fn an_unanswered_keepalive_ends_the_session() {
    within(async {
        let config = Config {
            keepalive_interval: Duration::from_millis(50),
            connection_write_timeout: Duration::from_millis(100),
            ..Config::default()
        };
        let (sess, _raw) = pair(config, true);
        assert!(matches!(sess.closed().await, Error::KeepAliveTimeout));
    })
    .await
}

#[tokio::test]
async fn a_stream_never_acked_ends_the_session() {
    within(async {
        let config = Config {
            stream_open_timeout: Some(Duration::from_millis(100)),
            ..quiet()
        };
        let (sess, _raw) = pair(config, true);
        let _s = sess.open().await.unwrap();
        assert!(matches!(sess.closed().await, Error::Timeout));

        // An ACK in time keeps it.
        let config = Config {
            stream_open_timeout: Some(Duration::from_millis(200)),
            ..quiet()
        };
        let (sess, mut raw) = pair(config, true);
        let s = sess.open().await.unwrap();
        raw.send(WINDOW, ACK, s.id(), 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!sess.is_closed());
    })
    .await
}

#[tokio::test]
async fn a_half_closed_stream_the_peer_never_closes_is_reset() {
    within(async {
        let config = Config {
            stream_close_timeout: Some(Duration::from_millis(100)),
            ..quiet()
        };
        let (sess, mut raw) = pair(config, true);
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        let mut s = sess.accept().await.unwrap();
        raw.frame().await.unwrap();
        s.shutdown().await.unwrap();
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!(h.flags, FIN);
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.flags, h.id), (WINDOW, RST, 2));
        assert_eq!(sess.num_streams(), 0);
    })
    .await
}

#[tokio::test]
async fn data_for_an_unknown_stream_is_discarded() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        raw.send(DATA, 0, 99, 5, b"stray").await;
        raw.send(WINDOW, SYN, 2, 0, &[]).await;
        raw.send(DATA, 0, 2, 2, b"ok").await;
        let mut s = sess.accept().await.unwrap();
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
        assert!(!sess.is_closed());
    })
    .await
}

#[tokio::test]
async fn dropping_every_handle_closes_the_connection() {
    within(async {
        let (sess, mut raw) = pair(quiet(), true);
        let s = sess.open().await.unwrap();
        raw.frame().await.unwrap();
        drop(sess);
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A stream still holds the session open.
        raw.send(PING, SYN, 0, 1, &[]).await;
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!((h.ty, h.flags), (PING, ACK));
        drop(s);
        // Its FIN, then the end of the connection.
        let (h, _) = raw.frame().await.unwrap();
        assert_eq!(h.flags, FIN);
        assert!(raw.frame().await.is_none());
    })
    .await
}
