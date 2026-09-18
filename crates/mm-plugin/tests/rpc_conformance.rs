//! The generated RPC layer against Go, and against itself.
//!
//! **Rust host, Go plugin.** `reference/dump/plugingen plugin` is a real plugin served by
//! `plugin.ClientMain`. Its hooks are plugintest's mock, and each answers with the oracle's
//! `Z_<Hook>Returns`. Its `OnActivate` calls every generated API method with the oracle's
//! `Z_<Method>Args`. The Rust host launches it through `goplugin` and serves the API from a fake
//! that answers with `Z_<Method>Returns` and records what it received. It calls every generated
//! hook through `HooksClient` with `Z_<Hook>Args`. Then both sides are compared with the oracle:
//! - what Go received as hook arguments, and what Rust received as API arguments;
//! - what Rust received as hook returns: the fixture after Go's `encodableError`, as the plugin
//!   recorded it;
//! - what Go received as API returns.
//!
//! **Rust with Rust.** `register_hooks` behind `HooksClient`, and `register_api` behind
//! `ApiClient`, round-trip every method. A plugin that implements one hook is skipped for every
//! other, and a hook it claims but does not provide fails with Go's not-implemented error.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_netrpc::Server;
use goplugin::yamux::{Config, Session};
use goplugin::{Client, ClientConfig, Dispensed, HandshakeConfig, MuxBroker, PluginCommand};
use mm_plugin::rpc::{
    ApiClient, Hooks, HooksClient, NotImplemented, PluginApi, PluginApiStreams, hooks_server,
    register_api,
};
use mm_plugin::wire::plugin::{
    Z_ChannelMemberWillBeAddedArgs, Z_MessageWillBePostedArgs, Z_MessageWillBeUpdatedArgs,
    Z_MessagesWillBeConsumedArgs, Z_MessagesWillBeConsumedWithContextArgs,
    Z_TeamMemberWillBeAddedArgs,
};
use serde_json::{Map, Value as Json};

mod common;
use common::*;

/// A writer the test can read back, for the replacement file.
#[derive(Clone)]
struct SharedWriter(Arc<tokio::sync::Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for SharedWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut guard = std::pin::pin!(self.0.lock());
        match guard.as_mut().poll(cx) {
            std::task::Poll::Ready(mut held) => {
                held.extend_from_slice(buf);
                std::task::Poll::Ready(Ok(buf.len()))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(120), f)
        .await
        .expect("test timed out")
}

/// Answers every generated method with its `Z_<Method>Returns` fixture and records how the
/// arguments it received render.
#[derive(Default)]
struct Fake {
    received: Mutex<BTreeMap<String, Json>>,
    implemented: Vec<String>,
}

impl Fake {
    fn answer<A: gobwire::Encode, R: gobwire::Decode + Default + Send + 'static>(
        &self,
        name: &str,
        returns: &str,
        args: &A,
    ) -> R {
        self.received
            .lock()
            .unwrap()
            .insert(name.to_owned(), render_typed(args));
        fixture(returns)
    }

    /// Read a lent stream to the end and record what arrived.
    async fn record_stream(&self, name: &str, mut data: mm_plugin::io_rpc::RemoteReader) {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        let read = data.read_to_end(&mut bytes).await;
        let mut digest = stream_digest(&bytes);
        if let Err(e) = read {
            digest["error"] = Json::String(e.to_string());
        }
        self.received
            .lock()
            .unwrap()
            .insert(format!("{name}.stream"), digest);
    }

    fn received(&self) -> BTreeMap<String, Json> {
        self.received.lock().unwrap().clone()
    }
}

/// Implements every API method with a generated client, and the log methods, which have none.
///
/// `LoadPluginConfiguration` is deliberately left to the trait default, so the host's
/// hand-written server answers `null` as Go's does.
macro_rules! fake_api {
    ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
        impl PluginApi for Fake {
            $(
                async fn $method(&self, args: $args) -> Result<$returns, NotImplemented> {
                    Ok(self.answer($name, $returns_name, &args))
                }
            )*

            async fn log_debug(
                &self,
                args: mm_plugin::wire::plugin::Z_LogDebugArgs,
            ) -> Result<mm_plugin::wire::plugin::Z_LogDebugReturns, NotImplemented> {
                Ok(self.answer("LogDebug", "Z_LogDebugReturns", &args))
            }

            async fn log_info(
                &self,
                args: mm_plugin::wire::plugin::Z_LogInfoArgs,
            ) -> Result<mm_plugin::wire::plugin::Z_LogInfoReturns, NotImplemented> {
                Ok(self.answer("LogInfo", "Z_LogInfoReturns", &args))
            }

            async fn log_warn(
                &self,
                args: mm_plugin::wire::plugin::Z_LogWarnArgs,
            ) -> Result<mm_plugin::wire::plugin::Z_LogWarnReturns, NotImplemented> {
                Ok(self.answer("LogWarn", "Z_LogWarnReturns", &args))
            }

            async fn log_error(
                &self,
                args: mm_plugin::wire::plugin::Z_LogErrorArgs,
            ) -> Result<mm_plugin::wire::plugin::Z_LogErrorReturns, NotImplemented> {
                Ok(self.answer("LogError", "Z_LogErrorReturns", &args))
            }

            async fn log_audit_rec(
                &self,
                args: mm_plugin::wire::plugin::Z_LogAuditRecArgs,
            ) -> Result<mm_plugin::wire::plugin::Z_LogAuditRecReturns, NotImplemented> {
                Ok(self.answer("LogAuditRec", "Z_LogAuditRecReturns", &args))
            }

            async fn log_audit_rec_with_level(
                &self,
                args: mm_plugin::wire::plugin::Z_LogAuditRecWithLevelArgs,
            ) -> Result<mm_plugin::wire::plugin::Z_LogAuditRecWithLevelReturns, NotImplemented>
            {
                Ok(self.answer(
                    "LogAuditRecWithLevel",
                    "Z_LogAuditRecWithLevelReturns",
                    &args,
                ))
            }
        }
    };
}
mm_plugin::for_each_api_call!(fake_api);

/// Reads each lent stream to the end and answers with the method's fixture, as the Go host does.
impl PluginApiStreams for Fake {
    async fn upload_data(
        &self,
        _: Option<Box<mm_plugin::wire::model::UploadSession>>,
        data: mm_plugin::io_rpc::RemoteReader,
    ) -> Result<mm_plugin::wire::plugin::Z_UploadDataReturns, NotImplemented> {
        self.record_stream("UploadData", data).await;
        Ok(fixture("Z_UploadDataReturns"))
    }

    async fn install_plugin(
        &self,
        bundle: mm_plugin::io_rpc::RemoteReader,
        _: bool,
    ) -> Result<mm_plugin::wire::plugin::Z_InstallPluginReturns, NotImplemented> {
        self.record_stream("InstallPlugin", bundle).await;
        Ok(fixture("Z_InstallPluginReturns"))
    }

    async fn receive_shared_channel_attachment_sync_msg(
        &self,
        _: String,
        _: String,
        _: Option<Box<mm_plugin::wire::model::FileInfo>>,
        data: mm_plugin::io_rpc::RemoteReader,
    ) -> Result<
        mm_plugin::wire::plugin::Z_ReceiveSharedChannelAttachmentSyncMsgReturns,
        NotImplemented,
    > {
        self.record_stream("ReceiveSharedChannelAttachmentSyncMsg", data)
            .await;
        Ok(fixture("Z_ReceiveSharedChannelAttachmentSyncMsgReturns"))
    }
}

/// Answers the plugin's outward HTTP call with what both suites expect, and records the request.
impl mm_plugin::rpc::PluginApiHttp for Fake {
    async fn plugin_http(
        &self,
        request: Option<Box<mm_plugin::wire::plugin::HTTPRequestSubset>>,
        body: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    ) -> Result<mm_plugin::rpc::HttpResponse, NotImplemented> {
        use tokio::io::AsyncReadExt as _;
        let mut body = body;
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)
            .await
            .expect("the request body");
        let mut received = self.received.lock().unwrap();
        received.insert("PluginHTTP".into(), render_typed(&request));
        received.insert("PluginHTTP.stream".into(), stream_digest(&bytes));
        drop(received);

        let (status_code, header, body) = outward_response();
        Ok(mm_plugin::rpc::HttpResponse {
            status_code,
            header,
            body: Box::new(std::io::Cursor::new(body)),
        })
    }
}

/// A database the conformance plugins can query: fixed answers, and a record of what was asked.
#[derive(Default)]
struct FakeDriver {
    asked: Mutex<Vec<String>>,
}

impl mm_plugin::rpc::Driver for FakeDriver {
    async fn conn(
        &self,
        is_master: bool,
    ) -> Result<mm_plugin::wire::plugin::Z_DbStrErrReturn, NotImplemented> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("Conn({is_master})"));
        Ok(mm_plugin::wire::plugin::Z_DbStrErrReturn {
            a: "conn-1".into(),
            b: None,
        })
    }

    async fn conn_ping(
        &self,
        conn_id: String,
    ) -> Result<mm_plugin::wire::plugin::Z_DbErrReturn, NotImplemented> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("ConnPing({conn_id})"));
        // The sentinel `driver.ErrBadConn`, which must still be one on the far side.
        Ok(mm_plugin::wire::plugin::Z_DbErrReturn {
            a: mm_plugin::error::encodable_error(Some(&mm_plugin::error::PluginError::Sentinel(
                mm_plugin::error::Sentinel::BadConn,
            ))),
        })
    }

    async fn conn_query(
        &self,
        args: mm_plugin::wire::plugin::Z_DbConnArgs,
    ) -> Result<mm_plugin::wire::plugin::Z_DbStrErrReturn, NotImplemented> {
        let named: Vec<String> = args
            .c
            .iter()
            .map(|v| format!("{}={:?}", v.name, v.value.as_ref().map(|i| i.name.clone())))
            .collect();
        self.asked.lock().unwrap().push(format!(
            "ConnQuery({}, {}, [{}])",
            args.a,
            args.b,
            named.join(", ")
        ));
        Ok(mm_plugin::wire::plugin::Z_DbStrErrReturn {
            a: "rows-1".into(),
            b: None,
        })
    }

    async fn rows_columns(
        &self,
        rows_id: String,
    ) -> Result<mm_plugin::wire::plugin::Z_DbStrSliceReturn, NotImplemented> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("RowsColumns({rows_id})"));
        Ok(mm_plugin::wire::plugin::Z_DbStrSliceReturn {
            a: vec!["id".into(), "name".into()],
        })
    }
}

macro_rules! fake_hooks {
    ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
        impl Hooks for Fake {
            fn implemented(&self) -> Vec<String> {
                self.implemented.clone()
            }
            $(
                async fn $method(&self, args: $args) -> Result<$returns, NotImplemented> {
                    Ok(self.answer($name, $returns_name, &args))
                }
            )*
        }
    };
}
mm_plugin::for_each_hook!(fake_hooks);

macro_rules! names {
    ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
        &[$($name),*]
    };
}
/// Every hook a plugin serves, and every API method a host serves.
const HOOKS: &[&str] = mm_plugin::for_each_hook!(names);
const API: &[&str] = mm_plugin::for_each_api_method!(names);

/// The API methods both conformance plugins call with their fixture arguments: everything with a
/// generated client. The hand-written ones are checked by name below; `LogAuditRec` and
/// `LogAuditRecWithLevel` have no Rust client yet (see `rpc/handwritten.rs`).
const API_CALLED: &[&str] = mm_plugin::for_each_api_call!(names);

/// The hooks whose client seeds the answer with the value the caller passed, so a partial reply
/// merges into it (client_rpc.go).
const MERGING_HOOKS: [&str; 4] = [
    "MessageWillBePosted",
    "MessageWillBeUpdated",
    "ChannelMemberWillBeAdded",
    "TeamMemberWillBeAdded",
];

/// What the conformance plugins send to the log methods, as the Rust fake sees it: Go stringifies
/// `("key", 42, true)` with `%+v` before they cross (stringifier.go).
fn logged_args() -> Json {
    serde_json::json!({
        "A": "a logged line",
        "B": [
            {"$iface": "string", "value": "key"},
            {"$iface": "string", "value": "42"},
            {"$iface": "string", "value": "true"},
        ],
    })
}

/// Every API method this suite does not call with its fixture arguments, and why. A new
/// hand-written method lands here as a failure until it is either called or listed.
/// The API methods that lend the host a reader rather than taking fixture arguments.
const API_STREAMS: [&str; 3] = [
    "UploadData",
    "InstallPlugin",
    "ReceiveSharedChannelAttachmentSyncMsg",
];

const API_NOT_CALLED: [&str; 6] = [
    // Called with a lent stream, and checked by what arrived on it.
    "UploadData",
    "InstallPlugin",
    "ReceiveSharedChannelAttachmentSyncMsg",
    // Called, but checked on their own: their arguments are the record after its JSON round trip.
    "LogAuditRec",
    "LogAuditRecWithLevel",
    // Called, but its answer is not a fixture.
    "LoadPluginConfiguration",
];

#[test]
fn rpc_every_api_method_is_called_or_named() {
    let called: Vec<&str> = API_CALLED
        .iter()
        .copied()
        .chain(["LogDebug", "LogInfo", "LogWarn", "LogError"])
        .collect();
    for name in API {
        assert!(
            called.contains(name) || API_NOT_CALLED.contains(name),
            "{name} is served but never called, and is not in API_NOT_CALLED"
        );
    }
}

/// Call every hook with its arguments fixture; the rendered returns by name. The hooks whose
/// clients Go writes by hand are called by name, since their signatures differ.
async fn call_every_hook(client: &HooksClient) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    macro_rules! call {
        ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
            $(
                let returns = client.$method(fixture::<$args>($args_name)).await;
                out.insert($name.to_owned(), render_typed(&returns));
            )*
        };
    }
    mm_plugin::for_each_hook_call!(call);

    macro_rules! call_by_hand {
        ($(($method:ident, $name:literal, $args:ty),)*) => {
            $(
                let returns = client.$method(fixture::<$args>(concat!($name, "Args"))).await;
                out.insert($name[2..].to_owned(), render_typed(&returns));
            )*
        };
    }
    call_by_hand! {
        (message_will_be_posted, "Z_MessageWillBePosted", Z_MessageWillBePostedArgs),
        (message_will_be_updated, "Z_MessageWillBeUpdated", Z_MessageWillBeUpdatedArgs),
        (messages_will_be_consumed, "Z_MessagesWillBeConsumed", Z_MessagesWillBeConsumedArgs),
        (messages_will_be_consumed_with_context, "Z_MessagesWillBeConsumedWithContext", Z_MessagesWillBeConsumedWithContextArgs),
        (channel_member_will_be_added, "Z_ChannelMemberWillBeAdded", Z_ChannelMemberWillBeAddedArgs),
        (team_member_will_be_added, "Z_TeamMemberWillBeAdded", Z_TeamMemberWillBeAddedArgs),
    }
    out
}

/// Call every API method whose client is generated, with its `Z_<Method>Args` fixture.
async fn call_every_api_method(client: &ApiClient) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    macro_rules! call {
        ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
            $(
                let returns = client.$method(fixture::<$args>($args_name)).await;
                out.insert($name.to_owned(), render_typed(&returns));
            )*
        };
    }
    mm_plugin::for_each_api_call!(call);
    out
}

fn handshake() -> HandshakeConfig {
    // api.go, `handshake`.
    HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "MATTERMOST_PLUGIN".into(),
        magic_cookie_value: "Securely message teams, anywhere.".into(),
    }
}

fn read_transcript(path: &PathBuf) -> Vec<Map<String, Json>> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_rust_host_drives_the_go_plugin() {
    let expected = expected();
    let dir = scratch("rpc-go-plugin");
    let transcript = dir.join("transcript.jsonl");

    let mut cmd = PluginCommand::new(plugingen())
        .arg("plugin")
        .arg(fixtures().join("gob"));
    cmd.env
        .push(("PLUGINGEN_TRANSCRIPT".into(), transcript.clone().into()));
    cmd.current_dir = Some(root().join("reference/dump"));
    let mut config = ClientConfig::new(handshake());
    config.cmd = Some(cmd);
    let plugin = within(Client::start(config)).await.unwrap();
    let hooks = HooksClient::new(within(plugin.dispense("hooks")).await.unwrap());

    let implemented = within(hooks.implemented()).await.unwrap();
    for name in HOOKS {
        assert!(
            implemented.iter().any(|n| n == name),
            "{name} not implemented"
        );
    }

    let api = Arc::new(Fake::default());
    let driver = Arc::new(FakeDriver::default());
    let activated = within(hooks.on_activate(&api, &driver)).await;
    assert_eq!(activated.a, None, "OnActivate returned an error");
    let returned = within(call_every_hook(&hooks)).await;

    // The hooks that serve HTTP: the plugin reads the body over one connection and answers over
    // the other, while the call is outstanding.
    let mut served = BTreeMap::new();
    for name in ["ServeHTTP", "ServeMetrics"] {
        let recorder = Recorder::default();
        let request = Some(Box::new(http_request()));
        let body = Some(std::io::Cursor::new(stream_payload()));
        if name == "ServeHTTP" {
            within(hooks.serve_http(None, request, body, recorder.clone())).await;
        } else {
            within(hooks.serve_metrics(None, request, body, recorder.clone())).await;
        }
        served.insert(name, recorder.response());
    }
    // What the Go plugin asked the database, in order.
    let asked = driver.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [
            "Conn(true)",
            "ConnPing(conn-1)",
            // Go registers an int64 under its own name, not `int`.
            "ConnQuery(conn-1, SELECT 1, [one=Some(\"int64\")])",
            "RowsColumns(rows-1)",
        ],
        "the plugin's database calls"
    );

    // The hook that lends a reader and a writer at once.
    let replacement = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let file_returns = within(hooks.file_will_be_uploaded(
        None,
        Some(Box::new(mm_plugin::wire::model::FileInfo {
            id: "fileinfo".into(),
            name: "upload.bin".into(),
            ..Default::default()
        })),
        std::io::Cursor::new(stream_payload()),
        SharedWriter(Arc::clone(&replacement)),
    ))
    .await;
    within(plugin.kill()).await;

    let mut failures = Vec::new();
    let mut go_hooks = BTreeMap::new();
    let mut go_api = BTreeMap::new();
    let mut go_config = None;
    let mut driver_tour = None;
    let mut outward = None;
    let mut activated = false;
    for entry in read_transcript(&transcript) {
        if let Some(Json::String(name)) = entry.get("hook") {
            // A later call replaces an earlier one: OnActivate itself calls OnConfigurationChange.
            go_hooks.insert(name.clone(), entry);
        } else if let Some(Json::String(name)) = entry.get("api") {
            match name.as_str() {
                "LoadPluginConfiguration" => go_config = entry.get("config").cloned(),
                // The outward HTTP call records the response it received, not gob returns.
                "PluginHTTP" => outward = Some(entry.clone()),
                _ => {
                    go_api.insert(name.clone(), entry["returns"].clone());
                }
            }
        } else if entry.get("driver") == Some(&Json::from("tour")) {
            driver_tour = Some(entry.clone());
        } else if entry.get("activated").is_some() {
            activated = true;
        }
    }
    assert!(
        activated,
        "the Go plugin's OnActivate did not finish its API tour"
    );

    for name in HOOKS {
        let Some(go) = go_hooks.get(*name) else {
            failures.push(format!("hook {name}: the Go plugin never saw the call"));
            continue;
        };
        let want_args = &expected[&format!("Z_{name}Args")];
        if &go["args"] != want_args {
            failures.push(format!(
                "hook {name}: Go received\n{}\nexpected\n{want_args}",
                go["args"]
            ));
        }
        // A merging hook's answer is not what the plugin sent: it is that decoded into the value
        // the host passed, which the assertions below check on their own.
        if !MERGING_HOOKS.contains(name) && returned[*name] != go["returns"] {
            failures.push(format!(
                "hook {name}: Rust received\n{}\nGo sent\n{}",
                returned[*name], go["returns"]
            ));
        }
    }
    let received = api.received();
    for name in API_CALLED {
        let want_args = &expected[&format!("Z_{name}Args")];
        match received.get(*name) {
            Some(got) if got == want_args => {}
            Some(got) => failures.push(format!(
                "api {name}: Rust received\n{got}\nexpected\n{want_args}"
            )),
            None => failures.push(format!("api {name}: the Rust server never saw the call")),
        }
        let want_returns = &expected[&format!("Z_{name}Returns")];
        match go_api.get(*name) {
            Some(got) if got == want_returns => {}
            Some(got) => failures.push(format!(
                "api {name}: Go received\n{got}\nexpected\n{want_returns}"
            )),
            None => failures.push(format!("api {name}: the Go plugin recorded no returns")),
        }
    }

    // The API methods whose clients Go writes by hand.
    for name in ["LogDebug", "LogInfo", "LogWarn", "LogError"] {
        match received.get(name) {
            Some(got) if got == &logged_args() => {}
            other => failures.push(format!("api {name}: Rust received {other:?}")),
        }
    }
    // Each lent stream arrived whole, through io_rpc's varint framing, and the plugin got the
    // method's fixture back.
    let want_stream = stream_digest(&stream_payload());
    for name in API_STREAMS {
        match received.get(&format!("{name}.stream")) {
            Some(got) if got == &want_stream => {}
            other => failures.push(format!("api {name}: the stream arrived as {other:?}")),
        }
        let want_returns = &expected[&format!("Z_{name}Returns")];
        match go_api.get(name) {
            Some(got) if got == want_returns => {}
            other => failures.push(format!("api {name}: the Go plugin recorded {other:?}")),
        }
    }

    // What the Go plugin wrote back, and what it read of the request.
    let want_response = http_response("POST", CONFORMANCE_URL, &stream_payload());
    let want_stream = stream_digest(&stream_payload());
    for name in ["ServeHTTP", "ServeMetrics"] {
        assert_eq!(served[name], want_response, "{name}: what the plugin wrote");
        match go_hooks.get(name) {
            Some(e) => {
                assert_eq!(e["stream"], want_stream, "{name}: the request body Go read");
                assert_eq!(
                    e["args"],
                    render_typed(&http_request()),
                    "{name}: the request Go received"
                );
            }
            None => failures.push(format!("hook {name}: the Go plugin never saw the call")),
        }
    }

    // The plugin's outward HTTP call reached the host's fake, and its answer came back whole.
    let (status, header, body) = outward_response();
    let outward = outward.expect("the Go plugin made no outward HTTP call");
    assert_eq!(outward["status"], status);
    assert_eq!(outward["header"], serde_json::json!(header));
    assert_eq!(outward["body"], String::from_utf8_lossy(&body).as_ref());
    assert_eq!(
        received.get("PluginHTTP.stream"),
        Some(&stream_digest(&stream_payload())),
        "the request body the host read"
    );

    // The replacement the plugin wrote arrived whole, and the hook answered with the info.
    assert_eq!(
        String::from_utf8(replacement.lock().await.clone()).unwrap(),
        replacement_file(&stream_payload()),
        "the replacement file"
    );
    assert_eq!(
        file_returns.a.map(|info| info.id),
        Some("fileinfo".to_owned())
    );
    assert_eq!(
        go_hooks["FileWillBeUploaded"]["stream"],
        stream_digest(&stream_payload()),
        "the file the plugin read"
    );

    // Go's decodableError turned the sentinel this host sent back into the real
    // `driver.ErrBadConn`, which is what its own sql driver retries on (client_rpc.go).
    let tour = driver_tour.expect("the Go plugin made no database calls");
    assert_eq!(tour["conn"], Json::from("conn-1"));
    assert_eq!(tour["columns"], serde_json::json!(["id", "name"]));
    assert_eq!(
        tour["ping_is_bad_conn"],
        Json::Bool(true),
        "the sentinel did not survive the round trip"
    );

    // The audit record crossed in its gob-safe form: the JSON round trip turned its integers
    // into floats and its structs into objects keyed by their `json:` tags (audit.go).
    for name in ["LogAuditRec", "LogAuditRecWithLevel"] {
        let want = &expected[&format!("Z_{name}Args.safe")];
        match received.get(name) {
            Some(got) if got == want => {}
            Some(got) => failures.push(format!(
                "api {name}: Rust received\n{got}\nexpected\n{want}"
            )),
            None => failures.push(format!("api {name}: the Rust server never saw the call")),
        }
    }

    // The fake implements no LoadPluginConfiguration, so the host answers `null` rather than the
    // not-implemented error every other method answers with (client_rpc.go).
    assert_eq!(
        go_config,
        Some(Json::Null),
        "the Go plugin's LoadPluginConfiguration"
    );

    // The plugin answered MessageWillBePosted with a post carrying one field. Every other field
    // of the answer must come from the post the host sent.
    assert_eq!(
        returned["MessageWillBePosted"],
        merged_post("edited by the conformance plugin"),
        "the merged post"
    );
    // The member hooks answered with their whole fixture, which still merges: a field the fixture
    // leaves zero is not sent, so the value the host passed survives in its place.
    for (name, merged) in [
        ("ChannelMemberWillBeAdded", merged_member()),
        ("TeamMemberWillBeAdded", merged_team_member()),
    ] {
        assert_eq!(
            returned[name], merged,
            "{name} merges into what the host sent"
        );
    }
    // MessageWillBeUpdated replaces rather than merges (client_rpc.go).
    assert_eq!(
        returned["MessageWillBeUpdated"], go_hooks["MessageWillBeUpdated"]["returns"],
        "MessageWillBeUpdated takes the plugin's answer as it is"
    );
    report(&failures);
}

/// The `ChannelMemberWillBeAdded` answer a merging client must produce: the plugin's whole
/// fixture, decoded into the member the host sent.
fn merged_member() -> Json {
    let sent: Z_ChannelMemberWillBeAddedArgs = fixture("Z_ChannelMemberWillBeAddedArgs");
    let seed = mm_plugin::wire::plugin::Z_ChannelMemberWillBeAddedReturns {
        a: sent.b,
        b: String::new(),
    };
    render_typed(&fixture_into("Z_ChannelMemberWillBeAddedReturns", seed))
}

/// The same for `TeamMemberWillBeAdded`.
fn merged_team_member() -> Json {
    let sent: Z_TeamMemberWillBeAddedArgs = fixture("Z_TeamMemberWillBeAddedArgs");
    let seed = mm_plugin::wire::plugin::Z_TeamMemberWillBeAddedReturns {
        a: sent.b,
        b: String::new(),
    };
    render_typed(&fixture_into("Z_TeamMemberWillBeAddedReturns", seed))
}

/// The `MessageWillBePosted` answer a merging client must produce: the post the host sent, with
/// only `Message` replaced.
fn merged_post(message: &str) -> Json {
    let sent: Z_MessageWillBePostedArgs = fixture("Z_MessageWillBePostedArgs");
    let mut post = sent.b.expect("the fixture has a post");
    post.message = message.to_owned();
    render_typed(&mm_plugin::wire::plugin::Z_MessageWillBePostedReturns {
        a: Some(post),
        b: String::new(),
    })
}

/// A connected host and plugin over an in-memory yamux session, with the host serving the API.
/// Both sides get a broker, because the streaming methods lend a reader over one.
async fn rust_api_pair<A: PluginApi + PluginApiStreams + mm_plugin::rpc::PluginApiHttp>(
    api: Arc<A>,
) -> ApiClient {
    let (a, b) = tokio::io::duplex(1 << 20);
    let host = Session::server(a, Config::default()).unwrap();
    let plugin = Session::client(b, Config::default()).unwrap();

    let stream = plugin.open().await.unwrap();
    let (plugin_broker, run) = MuxBroker::new(plugin.clone());
    tokio::spawn(run);

    tokio::spawn(async move {
        let served = host.accept().await.unwrap();
        let (host_broker, run) = MuxBroker::new(host.clone());
        tokio::spawn(run);
        let mut server = Server::new();
        register_api(&mut server, &api, &host_broker);
        let _ = Arc::new(server).serve(served).await;
        drop((host, host_broker));
    });
    ApiClient::new(go_netrpc::Client::new(stream), plugin_broker)
}

/// A connected host and plugin over an in-memory yamux session, with the plugin serving `hooks`.
/// As in go-plugin, each side's broker starts only after the hooks stream is open: a running broker
/// claims every stream the session accepts.
async fn rust_pair<H: Hooks>(hooks: Arc<H>) -> HooksClient {
    let (a, b) = tokio::io::duplex(1 << 20);
    let host = Session::client(a, Config::default()).unwrap();
    let plugin = Session::server(b, Config::default()).unwrap();

    let server = hooks_server(&hooks);
    tokio::spawn(async move {
        let stream = plugin.accept().await.unwrap();
        let (broker, run) = MuxBroker::new(plugin.clone());
        tokio::spawn(run);
        let _ = Arc::new(server).serve(stream).await;
        // The session lives as long as the connection is served.
        drop((plugin, broker));
    });
    let stream = host.open().await.unwrap();
    let (host_broker, run) = MuxBroker::new(host.clone());
    tokio::spawn(run);
    HooksClient::new(Dispensed {
        client: go_netrpc::Client::new(stream),
        broker: host_broker,
    })
}

/// A host and a plugin serving the full plugin server, which the HTTP hooks need: they reach
/// back over the broker for the response writer and the request body.
async fn rust_plugin_pair<P: mm_plugin::rpc::Plugin>(plugin: Arc<P>) -> HooksClient {
    let (a, b) = tokio::io::duplex(1 << 20);
    let host = Session::client(a, Config::default()).unwrap();
    let served = Session::server(b, Config::default()).unwrap();

    tokio::spawn(async move {
        let stream = served.accept().await.unwrap();
        let (broker, run) = MuxBroker::new(served.clone());
        tokio::spawn(run);
        let server = mm_plugin::rpc::plugin_server(&plugin, broker.clone());
        let _ = Arc::new(server).serve(stream).await;
        drop((served, broker));
    });
    let stream = host.open().await.unwrap();
    let (host_broker, run) = MuxBroker::new(host.clone());
    tokio::spawn(run);
    HooksClient::new(Dispensed {
        client: go_netrpc::Client::new(stream),
        broker: host_broker,
    })
}

/// A plugin that serves no HTTP: it reports what `implemented` says, and records whether the
/// host called it in spite of that.
#[derive(Default)]
struct NoHttp {
    implemented: Vec<String>,
    called: std::sync::atomic::AtomicBool,
}

impl NoHttp {
    fn new(implemented: &[&str]) -> Self {
        Self {
            implemented: implemented.iter().map(|s| (*s).to_owned()).collect(),
            called: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn was_called(&self) -> bool {
        self.called.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Hooks for NoHttp {
    fn implemented(&self) -> Vec<String> {
        self.implemented.clone()
    }
}
impl mm_plugin::rpc::HooksFileUpload for NoHttp {
    async fn file_will_be_uploaded(
        &self,
        _: Option<Box<mm_plugin::wire::plugin::Context>>,
        _: Option<Box<mm_plugin::wire::model::FileInfo>>,
        _: mm_plugin::io_rpc::RemoteReader,
        _: goplugin::yamux::Stream,
    ) -> Result<mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns, NotImplemented> {
        // Answers as a plugin without the hook does, but notes that the host called anyway.
        self.called.store(true, std::sync::atomic::Ordering::SeqCst);
        Err(NotImplemented)
    }
}
impl mm_plugin::rpc::Plugin for NoHttp {}
impl mm_plugin::rpc::HooksHttp for NoHttp {
    async fn serve_http(
        &self,
        _: Option<Box<mm_plugin::wire::plugin::Context>>,
        _: Option<Box<mm_plugin::wire::plugin::HTTPRequestSubset>>,
        _: Option<mm_plugin::io_rpc::RemoteReader>,
        mut writer: mm_plugin::http::RemoteResponseWriter,
    ) -> Result<(), NotImplemented> {
        // What the trait default does, with a note that the plugin was reached.
        self.called.store(true, std::sync::atomic::Ordering::SeqCst);
        writer.not_found().await;
        Ok(())
    }
}

/// An unserved request answers `404 page not found`, from whichever side notices first: the host
/// skips a hook the plugin never reported, and the plugin's own default answers the rest
/// (client_rpc.go, `hooksRPCServer.ServeHTTP`).
#[tokio::test]
async fn rpc_http_an_unserved_request_is_404() {
    // The host knows the plugin does not implement it, and never calls.
    let plugin = Arc::new(NoHttp::new(&[]));
    let client = rust_plugin_pair(Arc::clone(&plugin)).await;
    within(client.implemented()).await.unwrap();
    let recorder = Recorder::default();
    within(client.serve_http(
        None,
        Some(Box::new(http_request())),
        None::<std::io::Cursor<Vec<u8>>>,
        recorder.clone(),
    ))
    .await;
    assert_eq!(recorder.response(), http_not_found(), "the host's own 404");
    assert!(
        !plugin.was_called(),
        "the host called an unimplemented hook"
    );

    // The plugin reports it but has no handler, so its own answer comes over the connection.
    let plugin = Arc::new(NoHttp::new(&["ServeHTTP"]));
    let client = rust_plugin_pair(Arc::clone(&plugin)).await;
    within(client.implemented()).await.unwrap();
    let recorder = Recorder::default();
    within(client.serve_http(
        None,
        Some(Box::new(http_request())),
        None::<std::io::Cursor<Vec<u8>>>,
        recorder.clone(),
    ))
    .await;
    assert_eq!(recorder.response(), http_not_found(), "the plugin's 404");
    assert!(plugin.was_called(), "the plugin was not reached");
}

/// A header set without a status still reaches the host: every write pushes the plugin's copy of
/// the map first (http.go, `Write`).
#[tokio::test]
async fn rpc_http_a_header_set_without_a_status_still_arrives() {
    struct HeaderOnly;
    impl Hooks for HeaderOnly {
        fn implemented(&self) -> Vec<String> {
            vec!["ServeHTTP".into()]
        }
    }
    impl mm_plugin::rpc::HooksFileUpload for HeaderOnly {}
    impl mm_plugin::rpc::Plugin for HeaderOnly {}
    impl mm_plugin::rpc::HooksHttp for HeaderOnly {
        async fn serve_http(
            &self,
            _: Option<Box<mm_plugin::wire::plugin::Context>>,
            _: Option<Box<mm_plugin::wire::plugin::HTTPRequestSubset>>,
            _: Option<mm_plugin::io_rpc::RemoteReader>,
            mut writer: mm_plugin::http::RemoteResponseWriter,
        ) -> Result<(), NotImplemented> {
            writer
                .header()
                .await
                .insert("X-Set-Locally".into(), vec!["yes".into()]);
            writer.write(b"no status").await.unwrap();
            Ok(())
        }
    }

    let client = rust_plugin_pair(Arc::new(HeaderOnly)).await;
    within(client.implemented()).await.unwrap();
    let recorder = Recorder::default();
    within(client.serve_http(
        None,
        Some(Box::new(http_request())),
        None::<std::io::Cursor<Vec<u8>>>,
        recorder.clone(),
    ))
    .await;
    let response = recorder.response();
    assert_eq!(
        response["header"]["X-Set-Locally"],
        serde_json::json!(["yes"])
    );
    assert_eq!(response["body"], "no status");
}

/// A status Go's own server would panic on is refused, and the body still goes through
/// (http.go, `WriteHeader`).
#[tokio::test]
async fn rpc_http_an_invalid_status_is_refused() {
    struct Invalid;
    impl Hooks for Invalid {
        fn implemented(&self) -> Vec<String> {
            vec!["ServeHTTP".into()]
        }
    }
    impl mm_plugin::rpc::HooksFileUpload for Invalid {}
    impl mm_plugin::rpc::Plugin for Invalid {}
    impl mm_plugin::rpc::HooksHttp for Invalid {
        async fn serve_http(
            &self,
            _: Option<Box<mm_plugin::wire::plugin::Context>>,
            _: Option<Box<mm_plugin::wire::plugin::HTTPRequestSubset>>,
            _: Option<mm_plugin::io_rpc::RemoteReader>,
            mut writer: mm_plugin::http::RemoteResponseWriter,
        ) -> Result<(), NotImplemented> {
            writer.write_header(1000).await;
            writer.write(b"body anyway").await.unwrap();
            Ok(())
        }
    }

    let client = rust_plugin_pair(Arc::new(Invalid)).await;
    within(client.implemented()).await.unwrap();
    let recorder = Recorder::default();
    within(client.serve_http(
        None,
        Some(Box::new(http_request())),
        None::<std::io::Cursor<Vec<u8>>>,
        recorder.clone(),
    ))
    .await;
    let response = recorder.response();
    assert_eq!(response["status"], 200, "the invalid status was not kept");
    assert_eq!(response["body"], "body anyway");
}

/// A plugin that rewrites an uploaded file with a large replacement, so the host's copy is still
/// in flight when the hook answers.
struct Rewriter;
impl Hooks for Rewriter {
    fn implemented(&self) -> Vec<String> {
        vec!["FileWillBeUploaded".into()]
    }
}
impl mm_plugin::rpc::HooksHttp for Rewriter {}
impl mm_plugin::rpc::Plugin for Rewriter {}
impl mm_plugin::rpc::HooksFileUpload for Rewriter {
    async fn file_will_be_uploaded(
        &self,
        _: Option<Box<mm_plugin::wire::plugin::Context>>,
        info: Option<Box<mm_plugin::wire::model::FileInfo>>,
        mut file: mm_plugin::io_rpc::RemoteReader,
        output: goplugin::yamux::Stream,
    ) -> Result<mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns, NotImplemented> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut uploaded = Vec::new();
        file.read_to_end(&mut uploaded).await.unwrap();
        // Answer first and write after, which is the case the host's wait exists for: Go's copy
        // is still running when the call returns.
        tokio::spawn(async move {
            let mut output = output;
            tokio::time::sleep(Duration::from_millis(50)).await;
            output.write_all(&vec![b'r'; 1 << 20]).await.unwrap();
            output.shutdown().await.unwrap();
        });
        Ok(mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns {
            a: info,
            b: format!("read {}", uploaded.len()),
        })
    }
}

/// The host waits for the replacement copy before the hook answers, so the whole file is there
/// (client_rpc.go, "Ensure the io.Copy from the replacementFileConnection above completes").
#[tokio::test]
async fn rpc_a_replacement_file_is_complete_when_the_hook_answers() {
    let client = rust_plugin_pair(Arc::new(Rewriter)).await;
    within(client.implemented()).await.unwrap();
    let replacement = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let returns = within(client.file_will_be_uploaded(
        None,
        Some(Box::new(mm_plugin::wire::model::FileInfo {
            id: "given".into(),
            ..Default::default()
        })),
        std::io::Cursor::new(stream_payload()),
        SharedWriter(Arc::clone(&replacement)),
    ))
    .await;
    assert_eq!(returns.b, format!("read {}", stream_payload().len()));
    assert_eq!(
        replacement.lock().await.len(),
        1 << 20,
        "the replacement was still in flight when the hook answered"
    );
}

/// A plugin that does not implement the hook is not called, and the file info the caller passed
/// is the answer (client_rpc.go).
#[tokio::test]
async fn rpc_an_unimplemented_file_hook_answers_with_the_info_it_was_given() {
    let plugin = Arc::new(NoHttp::new(&[]));
    let client = rust_plugin_pair(Arc::clone(&plugin)).await;
    within(client.implemented()).await.unwrap();
    let replacement = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let returns = within(client.file_will_be_uploaded(
        None,
        Some(Box::new(mm_plugin::wire::model::FileInfo {
            id: "given".into(),
            ..Default::default()
        })),
        std::io::Cursor::new(stream_payload()),
        SharedWriter(Arc::clone(&replacement)),
    ))
    .await;
    assert_eq!(returns.a.map(|info| info.id), Some("given".to_owned()));
    assert_eq!(returns.b, "");
    assert!(replacement.lock().await.is_empty());
    assert!(
        !plugin.was_called(),
        "the host called an unimplemented hook"
    );
}

/// The plugin falls back to the buffered call only for Go's "can't find method" error, and gives
/// up on any other (client_rpc.go, `PluginHTTP`).
#[tokio::test]
async fn rpc_the_outward_http_call_falls_back_only_when_the_method_is_missing() {
    // A host from before the streaming shape: only `Plugin.PluginHTTP` exists, so the streaming
    // call comes back with net/rpc's "can't find method".
    let (a, b) = tokio::io::duplex(1 << 20);
    let host = Session::server(a, Config::default()).unwrap();
    let plugin_session = Session::client(b, Config::default()).unwrap();
    let stream = plugin_session.open().await.unwrap();
    let (plugin_broker, run) = MuxBroker::new(plugin_session.clone());
    tokio::spawn(run);
    tokio::spawn(async move {
        let served = host.accept().await.unwrap();
        let mut old_host = Server::new();
        old_host.register(
            "Plugin.PluginHTTP",
            |_: mm_plugin::wire::plugin::Z_PluginHTTPArgs| async {
                Ok::<_, go_netrpc::ServiceError>(mm_plugin::wire::plugin::Z_PluginHTTPReturns {
                    response: Some(Box::new(mm_plugin::wire::http::Response {
                        status_code: 204,
                        ..Default::default()
                    })),
                    response_body: b"buffered".to_vec(),
                })
            },
        );
        let _ = Arc::new(old_host).serve(served).await;
        drop(host);
    });
    let api = ApiClient::new(go_netrpc::Client::new(stream), plugin_broker);
    let response = within(api.plugin_http(
        Some(Box::new(http_request())),
        None::<std::io::Cursor<Vec<u8>>>,
    ))
    .await
    .expect("the buffered fallback answered");
    assert_eq!(response.status_code, 204);
    let mut body = Vec::new();
    {
        use tokio::io::AsyncReadExt as _;
        let mut response = response;
        response.body.read_to_end(&mut body).await.unwrap();
    }
    assert_eq!(body, b"buffered");

    // A host whose streaming method fails for another reason, while its buffered one would
    // answer: Go gives up rather than retrying, so the answer is none.
    let (a, b) = tokio::io::duplex(1 << 20);
    let host = Session::server(a, Config::default()).unwrap();
    let plugin_session = Session::client(b, Config::default()).unwrap();
    let stream = plugin_session.open().await.unwrap();
    let (plugin_broker, run) = MuxBroker::new(plugin_session.clone());
    tokio::spawn(run);
    tokio::spawn(async move {
        let served = host.accept().await.unwrap();
        let mut broken = Server::new();
        broken.register(
            "Plugin.PluginHTTPStream",
            |_: mm_plugin::wire::plugin::Z_PluginHTTPStreamArgs| async {
                Err::<mm_plugin::wire::plugin::Z_PluginHTTPStreamReturns, _>(
                    go_netrpc::ServiceError("the host is out of cheese".into()),
                )
            },
        );
        broken.register(
            "Plugin.PluginHTTP",
            |_: mm_plugin::wire::plugin::Z_PluginHTTPArgs| async {
                Ok::<_, go_netrpc::ServiceError>(mm_plugin::wire::plugin::Z_PluginHTTPReturns {
                    response: Some(Box::new(mm_plugin::wire::http::Response {
                        status_code: 204,
                        ..Default::default()
                    })),
                    response_body: b"buffered".to_vec(),
                })
            },
        );
        let _ = Arc::new(broken).serve(served).await;
        drop(host);
    });
    let api = ApiClient::new(go_netrpc::Client::new(stream), plugin_broker);
    assert!(
        within(api.plugin_http(
            Some(Box::new(http_request())),
            None::<std::io::Cursor<Vec<u8>>>
        ))
        .await
        .is_none(),
        "only a missing method may be retried as buffered"
    );
}

/// A host that implements nothing at all.
struct Nothing;
impl PluginApi for Nothing {}
impl PluginApiStreams for Nothing {}
impl mm_plugin::rpc::PluginApiHttp for Nothing {}

#[tokio::test]
async fn rpc_rust_hooks_round_trip_every_hook() {
    let expected = expected();
    let fake = Arc::new(Fake {
        implemented: HOOKS.iter().map(|s| (*s).to_owned()).collect(),
        ..Fake::default()
    });
    let client = rust_pair(Arc::clone(&fake)).await;
    assert_eq!(
        within(client.implemented()).await.unwrap().len(),
        HOOKS.len()
    );
    let returned = within(call_every_hook(&client)).await;

    let mut failures = Vec::new();
    let received = fake.received();
    for name in HOOKS {
        let args = &expected[&format!("Z_{name}Args")];
        if received.get(*name) != Some(args) {
            failures.push(format!(
                "hook {name}: the plugin received {:?}",
                received.get(*name)
            ));
        }
        if MERGING_HOOKS.contains(name) {
            continue; // rpc_a_merging_hook_keeps_the_fields_the_plugin_left_out.
        }
        let returns = &expected[&format!("Z_{name}Returns")];
        if &returned[*name] != returns {
            failures.push(format!(
                "hook {name}: the host received\n{}\nexpected\n{returns}",
                returned[*name]
            ));
        }
    }
    report(&failures);
}

#[tokio::test]
async fn rpc_rust_api_round_trips_every_method() {
    let expected = expected();
    let fake = Arc::new(Fake::default());
    let client = rust_api_pair(Arc::clone(&fake)).await;
    let returned = within(call_every_api_method(&client)).await;

    let mut failures = Vec::new();
    let received = fake.received();
    for name in API_CALLED {
        let args = &expected[&format!("Z_{name}Args")];
        if received.get(*name) != Some(args) {
            failures.push(format!(
                "api {name}: the host received {:?}",
                received.get(*name)
            ));
        }
        let returns = &expected[&format!("Z_{name}Returns")];
        if &returned[*name] != returns {
            failures.push(format!(
                "api {name}: the plugin received\n{}\nexpected\n{returns}",
                returned[*name]
            ));
        }
    }

    // The clients Go writes by hand.
    let pairs = ["key".to_owned(), "42".to_owned(), "true".to_owned()];
    client.log_debug("a logged line", &pairs).await;
    client.log_info("a logged line", &pairs).await;
    client.log_warn("a logged line", &pairs).await;
    client.log_error("a logged line", &pairs).await;
    let received = fake.received();
    for name in ["LogDebug", "LogInfo", "LogWarn", "LogError"] {
        match received.get(name) {
            Some(got) if got == &logged_args() => {}
            other => failures.push(format!("api {name}: the host received {other:?}")),
        }
    }
    // The fake answers no configuration, so the host's hand-written server sends `null`.
    assert_eq!(within(client.load_plugin_configuration()).await, b"null");
    report(&failures);
}

/// A plugin that answers `MessageWillBePosted` with one field set keeps every other field of the
/// post the host sent, because the client decodes the answer into it (client_rpc.go).
#[tokio::test]
async fn rpc_a_merging_hook_keeps_the_fields_the_plugin_left_out() {
    struct Partial;
    impl Hooks for Partial {
        fn implemented(&self) -> Vec<String> {
            vec!["MessageWillBePosted".into()]
        }
        async fn message_will_be_posted(
            &self,
            _: Z_MessageWillBePostedArgs,
        ) -> Result<mm_plugin::wire::plugin::Z_MessageWillBePostedReturns, NotImplemented> {
            Ok(mm_plugin::wire::plugin::Z_MessageWillBePostedReturns {
                a: Some(Box::new(mm_plugin::wire::model::Post {
                    message: "edited by the plugin".into(),
                    ..Default::default()
                })),
                b: String::new(),
            })
        }
    }

    let client = rust_pair(Arc::new(Partial)).await;
    let args: Z_MessageWillBePostedArgs = fixture("Z_MessageWillBePostedArgs");

    // Before Implemented, the hook is skipped and the answer is the post the host passed.
    let skipped = within(client.message_will_be_posted(args.clone())).await;
    assert_eq!(
        render_typed(&skipped),
        merged_post(&args.b.as_ref().unwrap().message)
    );

    within(client.implemented()).await.unwrap();
    let merged = within(client.message_will_be_posted(args.clone())).await;
    assert_eq!(render_typed(&merged), merged_post("edited by the plugin"));

    // The WithRPCErr companion does not seed: it answers with only what the plugin sent.
    let (returns, err) = within(client.message_will_be_posted_with_rpc_err(args)).await;
    assert!(err.is_none());
    assert_eq!(
        returns.a.map(|p| p.message),
        Some("edited by the plugin".to_owned())
    );
    assert_eq!(returns.b, "");

    // MessageWillBeUpdated, which this plugin does not implement, answers with the NEW post —
    // its first post argument, not the old one (client_rpc.go).
    let updated: Z_MessageWillBeUpdatedArgs = fixture("Z_MessageWillBeUpdatedArgs");
    let answer = within(client.message_will_be_updated(updated.clone())).await;
    assert_eq!(answer.a, updated.b, "the new post is the default answer");
    assert_ne!(updated.b, updated.c, "the fixture's posts differ");
    assert_eq!(answer.b, "");

    // Its WithRPCErr companion keeps no default at all.
    let (answer, err) = within(client.message_will_be_updated_with_rpc_err(updated)).await;
    assert!(err.is_none());
    assert_eq!(answer, Default::default());
}

/// Implements `OnDeactivate` and nothing else, but claims `OnInstall` too.
struct OneHook {
    calls: Mutex<Vec<&'static str>>,
}

impl Hooks for OneHook {
    fn implemented(&self) -> Vec<String> {
        vec![
            "OnDeactivate".into(),
            "OnInstall".into(),
            "MessageWillBePosted".into(),
            "NotAHook".into(),
        ]
    }

    async fn on_deactivate(
        &self,
        _: mm_plugin::wire::plugin::Z_OnDeactivateArgs,
    ) -> Result<mm_plugin::wire::plugin::Z_OnDeactivateReturns, NotImplemented> {
        self.calls.lock().unwrap().push("OnDeactivate");
        Ok(fixture("Z_OnDeactivateReturns"))
    }

    async fn user_has_been_created(
        &self,
        _: mm_plugin::wire::plugin::Z_UserHasBeenCreatedArgs,
    ) -> Result<mm_plugin::wire::plugin::Z_UserHasBeenCreatedReturns, NotImplemented> {
        self.calls.lock().unwrap().push("UserHasBeenCreated");
        Ok(Default::default())
    }
}

#[tokio::test]
async fn rpc_unimplemented_hooks_are_skipped_and_unprovided_ones_fail_as_in_go() {
    use mm_plugin::rpc::hook_id;
    use mm_plugin::wire::plugin::{Z_OnDeactivateArgs, Z_OnInstallArgs, Z_UserHasBeenCreatedArgs};

    let plugin = Arc::new(OneHook {
        calls: Mutex::new(Vec::new()),
    });
    let client = rust_pair(Arc::clone(&plugin)).await;

    // Nothing counts as implemented before the plugin is asked.
    assert!(!client.implements(hook_id::ON_DEACTIVATE));
    assert_eq!(
        within(client.on_deactivate(Z_OnDeactivateArgs {})).await,
        Default::default()
    );
    assert!(plugin.calls.lock().unwrap().is_empty());

    // A name without a hook id is reported but marks nothing.
    let names = within(client.implemented()).await.unwrap();
    assert_eq!(
        names,
        [
            "OnDeactivate",
            "OnInstall",
            "MessageWillBePosted",
            "NotAHook"
        ]
    );
    assert!(client.implements(hook_id::ON_DEACTIVATE));
    assert!(client.implements(hook_id::ON_INSTALL));
    assert!(!client.implements(hook_id::USER_HAS_BEEN_CREATED));
    assert!(!client.implements(hook_id::TOTAL_HOOKS));

    // Implemented: called, and its returns arrive.
    let returns = within(client.on_deactivate(Z_OnDeactivateArgs {})).await;
    assert_eq!(render_typed(&returns), expected()["Z_OnDeactivateReturns"]);

    // Provided but not reported: never called.
    let skipped =
        within(client.user_has_been_created_with_rpc_err(Z_UserHasBeenCreatedArgs::default()))
            .await;
    assert!(skipped.1.is_none());
    assert_eq!(*plugin.calls.lock().unwrap(), ["OnDeactivate"]);

    // A hand-written hook's message is Go's, which is neither capitalised nor stopped like the
    // generated one (client_rpc.go).
    let (_, err) =
        within(client.message_will_be_posted_with_rpc_err(fixture("Z_MessageWillBePostedArgs")))
            .await;
    match err {
        Some(go_netrpc::Error::Server(msg)) => {
            assert_eq!(msg, "hook MessageWillBePosted called but not implemented");
        }
        other => panic!("expected the not-implemented server error, got {other:?}"),
    }

    // Reported but not provided: Go's server error, zero returns.
    let (returns, err) = within(client.on_install_with_rpc_err(Z_OnInstallArgs::default())).await;
    assert_eq!(returns, Default::default());
    match err {
        Some(go_netrpc::Error::Server(msg)) => {
            assert_eq!(msg, "Hook OnInstall called but not implemented.");
        }
        other => panic!("expected the not-implemented server error, got {other:?}"),
    }
    assert_eq!(
        within(client.on_install(Z_OnInstallArgs::default())).await,
        Default::default()
    );
}

#[tokio::test]
async fn rpc_an_unprovided_api_method_fails_as_in_go() {
    let api = rust_api_pair(Arc::new(Nothing)).await;
    let err = within(
        api.client()
            .call::<_, mm_plugin::wire::plugin::Z_GetUserReturns>(
                "Plugin.GetUser",
                &mm_plugin::wire::plugin::Z_GetUserArgs { a: "u".into() },
            ),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "API GetUser called but not implemented.");

    // A streaming method's message is Go's, which carries no full stop (client_rpc.go).
    let returns = within(api.install_plugin(&b"bundle"[..], true)).await;
    assert_eq!(returns, Default::default());

    // The plugin's client logs the failure and answers zero values.
    let returns =
        within(api.get_user(mm_plugin::wire::plugin::Z_GetUserArgs { a: "u".into() })).await;
    assert_eq!(returns, Default::default());
}
