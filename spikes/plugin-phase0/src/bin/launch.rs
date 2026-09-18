//! Spike 3: launch a real Mattermost plugin binary from Rust and talk to it the way the Go
//! server's supervisor does (public/plugin/supervisor.go, go-plugin client.go + rpc_client.go).
//!
//!     launch <plugin-executable>
//!
//! Handshake env → protocol line on stdout → unix socket → yamux client → control, stdout and
//! stderr streams → `Dispenser.Dispense("hooks")` → MuxBroker dial with the LE u32 id/ack →
//! `Plugin.Implemented`, `Plugin.UserWillLogIn`, `Plugin.OnDeactivate` → `Control.Ping`,
//! `Control.Quit` → process exit.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use futures::{AsyncReadExt, AsyncWriteExt};
use plugin_phase0::gob::{Ty, Val};
use plugin_phase0::mux::{Mode, Mux, Stream};
use plugin_phase0::netrpc::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::compat::TokioAsyncReadCompatExt;

/// public/plugin/api.go:1779
const COOKIE_KEY: &str = "MATTERMOST_PLUGIN";
const COOKIE_VALUE: &str = "Securely message teams, anywhere.";

fn empty(name: &'static str) -> (Ty, Val) {
    (Ty::Struct(name, vec![]), Val::Struct(vec![]))
}

/// Drain a std stream the plugin pipes its stdout/stderr into (rpc_server.go:103-104).
fn drain(mut s: Stream, label: &'static str) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 {
                break;
            }
            for line in String::from_utf8_lossy(&buf[..n]).lines() {
                println!("  [plugin {label}] {line}");
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let exe = std::env::args()
        .nth(1)
        .context("usage: launch <plugin-executable>")?;
    let t0 = Instant::now();

    // SecureConfig: SHA-256 of the executable (supervisor.go:66-74).
    let checksum = Sha256::digest(std::fs::read(&exe)?);
    println!(
        "checksum sha256:{}",
        checksum
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );

    // Handshake env (go-plugin client.go:639-642; MinPort/MaxPort defaults client.go:393-395).
    let mut child = tokio::process::Command::new(&exe)
        .env(COOKIE_KEY, COOKIE_VALUE)
        .env("PLUGIN_MIN_PORT", "10000")
        .env("PLUGIN_MAX_PORT", "25000")
        .env("PLUGIN_PROTOCOL_VERSIONS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().context("stdout")?).lines();
    let mut stderr = BufReader::new(child.stderr.take().context("stderr")?).lines();
    tokio::spawn(async move {
        while let Ok(Some(line)) = stderr.next_line().await {
            println!("  [plugin process stderr] {line}");
        }
    });

    // Protocol line: CORE|APP|network|addr|protocol|servercert (server.go:425-445), within the
    // supervisor's 3 s StartTimeout (supervisor.go:124).
    let line = tokio::time::timeout(Duration::from_secs(3), stdout.next_line())
        .await
        .context("plugin did not print its protocol line within 3s")??
        .context("plugin exited before its protocol line")?;
    println!("protocol line after {:?}: {line}", t0.elapsed());
    let parts: Vec<&str> = line.split('|').collect();
    ensure!(parts.len() >= 5, "short protocol line");
    ensure!(
        parts[0] == "1" && parts[1] == "1",
        "core/app protocol version {} / {}",
        parts[0],
        parts[1]
    );
    ensure!(
        parts[2] == "unix" && parts[4] == "netrpc",
        "unexpected transport {} / {}",
        parts[2],
        parts[4]
    );
    tokio::spawn(async move {
        while let Ok(Some(line)) = stdout.next_line().await {
            println!("  [plugin process stdout] {line}");
        }
    });

    let sock = UnixStream::connect(parts[3]).await?;
    let (mux, _inbound, _driver) = Mux::start(sock.compat(), Mode::Client);

    // rpc_client.go:73-89: control first, then stdout and stderr. The server assigns roles by
    // the order it *accepts* them (rpc_server.go:78-96), i.e. by SYN arrival — so all three SYNs
    // must go out eagerly and in this order. With a lazy control stream its SYN rides on the
    // first RPC, after both std streams' SYNs, and the server serves net/rpc on stdout: a hang.
    let control = mux.open_eager().await?;
    drain(mux.open_eager().await?, "stdout");
    drain(mux.open_eager().await?, "stderr");
    let mut control = Client::new(control);

    let reply = control
        .call(
            "Dispenser.Dispense",
            &Ty::String,
            &Val::String("hooks".into()),
        )
        .await?;
    let id = match reply {
        Ok(v) => v.as_u64().context("Dispense reply is not a uint")?,
        Err(e) => bail!("Dispense failed: {e}"),
    };
    println!("dispensed hooks on broker id {id}");

    // MuxBroker.Dial (mux_broker.go:98-122): open, write id LE, read the same id back.
    let mut hooks_stream = mux.open().await?;
    let id32 = u32::try_from(id)?;
    hooks_stream.write_all(&id32.to_le_bytes()).await?;
    let mut ack = [0u8; 4];
    hooks_stream.read_exact(&mut ack).await?;
    ensure!(u32::from_le_bytes(ack) == id32, "bad broker ack {:?}", ack);
    let mut hooks = Client::new(hooks_stream);

    let (ty, val) = empty("struct {}");
    let implemented = hooks.call("Plugin.Implemented", &ty, &val).await?;
    println!(
        "Plugin.Implemented -> {}",
        implemented
            .as_ref()
            .map_or_else(|e| e.clone(), Value::to_string)
    );

    // Z_UserWillLogInArgs{A *Context, B *model.User}. The User type we define carries only two
    // fields: gob matches fields by name, so the plugin fills Id and Username and zeroes the rest.
    let args_ty = Ty::Struct(
        "Z_UserWillLogInArgs",
        vec![
            (
                "A",
                Ty::Struct(
                    "Context",
                    vec![("SessionId", Ty::String), ("RequestId", Ty::String)],
                ),
            ),
            (
                "B",
                Ty::Struct("User", vec![("Id", Ty::String), ("Username", Ty::String)]),
            ),
        ],
    );
    let args = Val::Struct(vec![
        Some(Val::Struct(vec![
            Some(Val::String(String::new())),
            Some(Val::String("req-from-rust".into())),
        ])),
        Some(Val::Struct(vec![
            Some(Val::String("uid1".into())),
            Some(Val::String("rustacean".into())),
        ])),
    ]);
    let login = hooks.call("Plugin.UserWillLogIn", &args_ty, &args).await?;
    println!(
        "Plugin.UserWillLogIn -> {}",
        login.as_ref().map_or_else(|e| e.clone(), Value::to_string)
    );

    let (ty, val) = empty("Z_OnDeactivateArgs");
    let deactivate = hooks.call("Plugin.OnDeactivate", &ty, &val).await?;
    println!(
        "Plugin.OnDeactivate -> {}",
        deactivate
            .as_ref()
            .map_or_else(|e| e.clone(), Value::to_string)
    );

    // A hook the plugin does not implement: hooksRPCServer answers with an encoded error value.
    let (ty, val) = empty("Z_OnInstallArgs");
    let missing = hooks.call("Plugin.UserHasBeenCreated", &ty, &val).await?;
    println!(
        "Plugin.UserHasBeenCreated (not implemented) -> {}",
        missing
            .as_ref()
            .map_or_else(|e| e.clone(), Value::to_string)
    );

    let ping = control
        .call("Control.Ping", &Ty::Bool, &Val::Bool(true))
        .await?;
    println!("Control.Ping -> {ping:?}");
    let quit = control
        .call("Control.Quit", &Ty::Bool, &Val::Bool(true))
        .await?;
    println!("Control.Quit -> {quit:?}");
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .context("plugin did not exit after Quit")??;
    println!("plugin exited: {status} after {:?} total", t0.elapsed());

    ensure!(
        login.as_ref().ok().and_then(|v| v["A"].as_str())
            == Some("hello rustacean (uid1) req=req-from-rust"),
        "UserWillLogIn reply"
    );
    ensure!(
        deactivate
            .as_ref()
            .ok()
            .map(|v| v["A"]["$iface"] == "*plugin.ErrorString")
            .unwrap_or(false),
        "OnDeactivate reply"
    );
    println!("LAUNCH SPIKE OK");
    Ok(())
}
