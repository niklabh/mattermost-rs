//! Host behaviour no plugin transcript can show.

use std::time::Duration;

use goplugin::{Client, ClientConfig, ClientError, HandshakeConfig, PluginAddr, ReattachConfig};

/// cmdrunner.ReattachFunc checks the process as well as the address: an address that still
/// answers is not enough when the pid is gone.
#[cfg(unix)]
#[tokio::test]
async fn reattaching_needs_a_live_process_not_just_a_listening_socket() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join(format!("reattach-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });

    let mut exited = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = exited.id();
    exited.wait().unwrap();

    let mut config = ClientConfig::new(HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "K".into(),
        magic_cookie_value: "V".into(),
    });
    config.reattach = Some(ReattachConfig {
        protocol: "netrpc".into(),
        protocol_version: 1,
        addr: PluginAddr::Unix(path.clone()),
        pid: dead_pid,
        test: false,
    });
    let result = tokio::time::timeout(Duration::from_secs(10), Client::start(config))
        .await
        .expect("reattach hung");
    assert!(
        matches!(result, Err(ClientError::ProcessNotFound)),
        "{:?}",
        result.err()
    );
    let _ = std::fs::remove_file(&path);
}
