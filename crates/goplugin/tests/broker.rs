//! The MuxBroker's rendezvous, in both orders, and its failures.

use std::time::Duration;

use goplugin::MuxBroker;
use goplugin::yamux::{Config, Session};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("test timed out")
}

fn pair() -> (MuxBroker, MuxBroker) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let config = Config {
        enable_keepalive: false,
        ..Config::default()
    };
    let (host, run_host) = MuxBroker::new(Session::client(a, config.clone()).unwrap());
    let (plugin, run_plugin) = MuxBroker::new(Session::server(b, config).unwrap());
    tokio::spawn(run_host);
    tokio::spawn(run_plugin);
    (host, plugin)
}

#[tokio::test]
async fn ids_start_at_one() {
    let (host, _) = pair();
    assert_eq!((host.next_id(), host.next_id()), (1, 2));
}

#[tokio::test]
async fn accept_before_dial_and_the_ack() {
    within(async {
        let (host, plugin) = pair();
        let id = host.next_id();
        let accepting = tokio::spawn({
            let host = host.clone();
            async move { host.accept(id).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut dialed = plugin.dial(id).await.unwrap();
        let mut accepted = accepting.await.unwrap().unwrap();
        dialed.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        accepted.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
    })
    .await
}

/// The dialer's stream can arrive before the acceptor is waiting; it is parked, not lost.
#[tokio::test]
async fn dial_before_accept() {
    within(async {
        let (host, plugin) = pair();
        let id = 7;
        let dialing = tokio::spawn({
            let plugin = plugin.clone();
            async move { plugin.dial(id).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!dialing.is_finished(), "no ack before the accept");
        let started = std::time::Instant::now();
        let _accepted = host.accept(id).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the parked stream was not delivered"
        );
        dialing.await.unwrap().unwrap();
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn an_accept_nobody_dials_times_out() {
    let (host, _plugin) = pair();
    let err = host.accept(3).await.unwrap_err();
    assert_eq!(err.to_string(), "timeout waiting for accept");
}

#[tokio::test]
async fn a_wrong_ack_is_refused() {
    within(async {
        let (a, b) = tokio::io::duplex(1 << 20);
        let config = Config {
            enable_keepalive: false,
            ..Config::default()
        };
        let (host, run) = MuxBroker::new(Session::client(a, config.clone()).unwrap());
        tokio::spawn(run);
        let raw = Session::server(b, config).unwrap();
        tokio::spawn(async move {
            let mut s = raw.accept().await.unwrap();
            let mut id = [0u8; 4];
            s.read_exact(&mut id).await.unwrap();
            s.write_all(&99u32.to_le_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let err = host.dial(5).await.unwrap_err();
        assert_eq!(err.to_string(), "bad ack: 99 (expected 5)");
    })
    .await
}
