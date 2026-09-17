//! Client and server behaviour a real Go peer cannot easily provoke, against a scripted peer
//! that writes raw gob: net/rpc/client.go's `input` and server.go's `ServeCodec` branch by
//! branch.

use std::time::Duration;

use go_netrpc::{Client, Error, Request, Response, Server, ServiceError};
use gobwire::{Decode, Decoder, Encode, Encoder, Gob, Progress};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// Every test runs under a deadline: a lost reply or a missed shutdown shows up as a hang, and a
/// hung test would stall a mutation batch instead of failing it.
async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(20), f)
        .await
        .expect("test timed out: a call never completed")
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Pair {
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: String,
}

/// The raw side of a connection.
struct Peer {
    io: DuplexStream,
    enc: Encoder,
    dec: Decoder,
}

impl Peer {
    async fn read<T: Decode + Default>(&mut self) -> Option<T> {
        loop {
            let mut first = [0u8; 1];
            if self.io.read(&mut first).await.unwrap() == 0 {
                return None;
            }
            let len = if first[0] < 0x80 {
                usize::from(first[0])
            } else {
                let w = usize::from(first[0].wrapping_neg());
                let mut b = [0u8; 8];
                self.io.read_exact(&mut b[..w]).await.unwrap();
                b[..w].iter().fold(0usize, |a, &x| a << 8 | usize::from(x))
            };
            let mut body = vec![0u8; len];
            self.io.read_exact(&mut body).await.unwrap();
            if self.dec.push_message(&body).unwrap() == Progress::Ready {
                return Some(self.dec.decode().unwrap());
            }
        }
    }

    async fn write<T: Encode + ?Sized>(&mut self, v: &T) {
        let bytes = self.enc.encode(v).unwrap();
        self.io.write_all(&bytes).await.unwrap();
    }

    async fn respond<T: Encode + ?Sized>(&mut self, seq: u64, error: &str, body: &T) {
        self.write(&Response {
            service_method: "X.Y".into(),
            seq,
            error: error.into(),
        })
        .await;
        self.write(body).await;
    }
}

fn pair() -> (Client, Peer) {
    let (a, b) = tokio::io::duplex(1 << 16);
    (
        Client::new(a),
        Peer {
            io: b,
            enc: Encoder::new(),
            dec: Decoder::new(),
        },
    )
}

#[tokio::test]
async fn sequence_numbers_start_at_zero_and_route_replies() {
    within(async move {
        let (client, mut peer) = pair();
        let c2 = client.clone();
        let first = tokio::spawn(async move { c2.call::<_, i64>("S.First", &1i64).await });
        let req: Request = peer.read().await.unwrap();
        assert_eq!(
            req,
            Request {
                service_method: "S.First".into(),
                seq: 0
            }
        );
        assert_eq!(peer.read::<i64>().await, Some(1));

        let c3 = client.clone();
        let second = tokio::spawn(async move { c3.call::<_, i64>("S.Second", &2i64).await });
        let req: Request = peer.read().await.unwrap();
        assert_eq!(req.seq, 1);
        peer.read::<i64>().await;

        // Answer out of order; a reply for a sequence nobody is waiting on is read and dropped.
        peer.respond(1, "", &20i64).await;
        peer.respond(
            77,
            "",
            &Pair {
                a: 1,
                b: "stray".into(),
            },
        )
        .await;
        peer.respond(0, "", &10i64).await;
        assert_eq!(second.await.unwrap().unwrap(), 20);
        assert_eq!(first.await.unwrap().unwrap(), 10);
    })
    .await
}

#[tokio::test]
async fn call_into_merges_the_reply_into_the_given_value() {
    within(async move {
        let (client, mut peer) = pair();
        let call = tokio::spawn(async move {
            client
                .call_into(
                    "S.M",
                    &0i64,
                    Pair {
                        a: 5,
                        b: "kept".into(),
                    },
                )
                .await
        });
        peer.read::<Request>().await;
        peer.read::<i64>().await;
        // `B` is empty, so gob omits it and the destination keeps "kept".
        peer.respond(
            0,
            "",
            &Pair {
                a: 9,
                b: String::new(),
            },
        )
        .await;
        assert_eq!(
            call.await.unwrap().unwrap(),
            Pair {
                a: 9,
                b: "kept".into()
            }
        );
    })
    .await
}

#[tokio::test]
async fn a_server_error_discards_the_body_and_keeps_the_connection() {
    within(async move {
        let (client, mut peer) = pair();
        let c = client.clone();
        let failing = tokio::spawn(async move { c.call::<_, Pair>("S.M", &0i64).await });
        peer.read::<Request>().await;
        peer.read::<i64>().await;
        // An error reply whose body is not even the requested type.
        peer.respond(0, "boom", &go_netrpc::InvalidRequest {}).await;
        assert!(matches!(failing.await.unwrap(), Err(Error::Server(m)) if m == "boom"));

        let c = client.clone();
        let ok = tokio::spawn(async move { c.call::<_, Pair>("S.M", &0i64).await });
        peer.read::<Request>().await;
        peer.read::<i64>().await;
        peer.respond(
            1,
            "",
            &Pair {
                a: 1,
                b: "b".into(),
            },
        )
        .await;
        assert_eq!(
            ok.await.unwrap().unwrap(),
            Pair {
                a: 1,
                b: "b".into()
            }
        );
    })
    .await
}

#[tokio::test]
async fn an_undecodable_reply_ends_the_connection() {
    within(async move {
        let (client, mut peer) = pair();
        let c = client.clone();
        let bad = tokio::spawn(async move { c.call::<_, Pair>("S.M", &0i64).await });
        let c = client.clone();
        let waiting = tokio::spawn(async move { c.call::<_, i64>("S.N", &0i64).await });
        for _ in 0..2 {
            peer.read::<Request>().await;
            peer.read::<i64>().await;
        }
        peer.respond(0, "", "a string, not a Pair").await;
        assert!(matches!(bad.await.unwrap(), Err(Error::ReadingBody(_))));
        // The other pending call fails with the connection, and later calls are refused.
        assert!(matches!(waiting.await.unwrap(), Err(Error::Gob(_))));
        assert!(matches!(
            client.call::<_, i64>("S.M", &0i64).await,
            Err(Error::Shutdown)
        ));
    })
    .await
}

#[tokio::test]
async fn the_peer_hanging_up_fails_pending_calls_with_unexpected_eof() {
    within(async move {
        let (client, mut peer) = pair();
        let c = client.clone();
        let pending = tokio::spawn(async move { c.call::<_, i64>("S.M", &0i64).await });
        peer.read::<Request>().await;
        peer.read::<i64>().await;
        drop(peer);
        assert!(matches!(pending.await.unwrap(), Err(Error::UnexpectedEof)));
        assert!(matches!(
            client.call::<_, i64>("S.M", &0i64).await,
            Err(Error::Shutdown)
        ));
    })
    .await
}

#[tokio::test]
async fn closing_fails_pending_calls_with_shutdown() {
    within(async move {
        let (client, mut peer) = pair();
        let c = client.clone();
        let pending = tokio::spawn(async move { c.call::<_, i64>("S.M", &0i64).await });
        peer.read::<Request>().await;
        peer.read::<i64>().await;
        client.close().await.unwrap();
        // The close reaches the peer as end of input; it closes too.
        assert!(peer.read::<Request>().await.is_none());
        drop(peer);
        assert!(matches!(pending.await.unwrap(), Err(Error::Shutdown)));
        assert!(matches!(client.close().await, Err(Error::Shutdown)));
        assert!(matches!(
            client.call::<_, i64>("S.M", &0i64).await,
            Err(Error::Shutdown)
        ));
    })
    .await
}

#[tokio::test]
async fn an_argument_that_cannot_be_encoded_leaves_the_stream_intact() {
    within(async move {
        let (client, mut peer) = pair();
        let err = client.call::<_, i64>("S.M", &None::<Pair>).await;
        assert!(matches!(err, Err(Error::Gob(_))), "{err:?}");
        let c = client.clone();
        let ok = tokio::spawn(async move {
            c.call::<_, i64>(
                "S.M",
                &Pair {
                    a: 3,
                    b: "x".into(),
                },
            )
            .await
        });
        // Nothing of the failed call reached the wire: the first request the peer sees is this one,
        // with its type definitions intact and the next sequence number.
        let req: Request = peer.read().await.unwrap();
        assert_eq!(req.seq, 1);
        assert_eq!(
            peer.read::<Pair>().await,
            Some(Pair {
                a: 3,
                b: "x".into()
            })
        );
        peer.respond(1, "", &4i64).await;
        assert_eq!(ok.await.unwrap().unwrap(), 4);
    })
    .await
}

fn server() -> std::sync::Arc<Server> {
    let mut s = Server::new();
    s.register("S.Slow", |ms: i64| async move {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        Ok::<_, ServiceError>(ms)
    });
    std::sync::Arc::new(s)
}

/// server.go, ServeCodec: `wg.Wait()` before `codec.Close()` — a client that sends its last
/// request and closes its write side still gets the answer.
#[tokio::test]
async fn the_server_answers_calls_in_flight_when_input_ends() {
    within(async move {
        let (a, b) = tokio::io::duplex(1 << 16);
        let served = tokio::spawn(server().serve(a));
        let mut peer = Peer {
            io: b,
            enc: Encoder::new(),
            dec: Decoder::new(),
        };
        peer.write(&Request {
            service_method: "S.Slow".into(),
            seq: 7,
        })
        .await;
        peer.write(&50i64).await;
        peer.io.shutdown().await.unwrap();
        let resp: Response = peer.read().await.unwrap();
        assert_eq!(
            resp,
            Response {
                service_method: "S.Slow".into(),
                seq: 7,
                error: String::new()
            }
        );
        assert_eq!(peer.read::<i64>().await, Some(50));
        assert!(
            peer.read::<Response>().await.is_none(),
            "server closes after the last reply"
        );
        served.await.unwrap().unwrap();
    })
    .await
}

/// A header that does not decode ends the connection (keepReading = false); an argument that
/// does not decode only fails its call (keepReading = true).
#[tokio::test]
async fn undecodable_headers_end_the_connection_but_bodies_do_not() {
    within(async move {
        let (a, b) = tokio::io::duplex(1 << 16);
        let served = tokio::spawn(server().serve(a));
        let mut peer = Peer {
            io: b,
            enc: Encoder::new(),
            dec: Decoder::new(),
        };
        peer.write(&Request {
            service_method: "S.Slow".into(),
            seq: 1,
        })
        .await;
        peer.write("not an int").await;
        let resp: Response = peer.read().await.unwrap();
        assert_eq!(resp.seq, 1);
        assert!(!resp.error.is_empty());
        assert_eq!(
            peer.read::<go_netrpc::InvalidRequest>().await,
            Some(go_netrpc::InvalidRequest {})
        );

        peer.write(&Request {
            service_method: "S.Slow".into(),
            seq: 2,
        })
        .await;
        peer.write(&1i64).await;
        assert_eq!(peer.read::<Response>().await.map(|r| r.seq), Some(2));
        assert_eq!(peer.read::<i64>().await, Some(1));

        // A header of the wrong type.
        peer.write(&Pair {
            a: 1,
            b: "not a Request".into(),
        })
        .await;
        assert!(served.await.unwrap().is_err());
    })
    .await
}
