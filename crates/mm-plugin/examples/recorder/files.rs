//! The hook recorder's files script: the plugin API's file, dialog, mail and inter-plugin HTTP
//! methods, each written down with what the host answered, for `parity::plugin_hooks`' files
//! tranche (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs from inside `ExecuteCommand` on `/hookrec files`, so that `OpenInteractiveDialog` has a
//! trigger id the host signed. Its uploads fire this plugin's own `FileWillBeUploaded` (the
//! `hookreplace`, `hookrename`, `hookrefuse` and `hookunimage` names choose the answer, as for a
//! REST upload), and its `PluginHTTP` calls reach this plugin's own `ServeHTTP` — both land in the
//! transcript beside the script's entry.
//!
//! # What the suite hands it
//!
//! Through the environment: this side's **tag**, which every name the script makes carries, and
//! the ids of five files the suite uploaded through main Go into the trigger's channel —
//! [`Planted`]. The channel, the user and the trigger id are the command's.
//!
//! With `HOOK_RECORDER_FILES` set the recorder also implements `ServeHTTP` ([`serve`]).

use go_netrpc::Client;
use mm_plugin::io_rpc::RemoteReader;
use mm_plugin::rpc::ApiClient;
use mm_plugin::wire::model::{
    CommandArgs, Dialog, DialogElement, GetFileInfosOptions, OpenDialogRequest, PostActionOptions,
};
use mm_plugin::wire::plugin::*;
use serde_json::{Value as Json, json};
use tokio::io::AsyncReadExt as _;

use crate::core::{MISSING, call};
use crate::render::render_typed;

/// The variable that switches `ServeHTTP` on.
pub const SWITCH: &str = "HOOK_RECORDER_FILES";

/// The most of any response body the script reads: every answer here is a few bytes, and a
/// runaway must not take the machine with it.
const BODY_CAP: u64 = 64 * 1024;

/// The most of a served request's body the recorder reads.
const SERVED_BODY_CAP: u64 = 16 * 1024 * 1024;
/// `/big`'s answer: this many writes of [`BIG_CHUNK`] bytes.
const BIG_CHUNKS: usize = 3;
const BIG_CHUNK: usize = 100_000;

/// FNV-1a, 64 bits, as hex: enough to tell two large bodies apart in a transcript.
fn fnv1a(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `ServeHTTP` is implemented under the files script's switch, or under
/// `HOOK_RECORDER_HTTP` alone (the client HTTP tranche).
pub fn serves_http() -> bool {
    enabled() || std::env::var_os("HOOK_RECORDER_HTTP").is_some()
}

pub fn enabled() -> bool {
    std::env::var_os(SWITCH).is_some()
}

/// A 3×2 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAIAAAASFvFNAAAAF0lEQVR4nGP4z8DAAMFcInInUowYGBgANh8EmGKCN2kAAAAASUVORK5CYII=";

/// A 4×2 JPEG whose EXIF orientation (6) turns it on its side: the row says 2×4.
const JPEG_ROTATED: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/4QAiRXhpZgAATU0AKgAAAAgAAQESAAMAAAABAAYAAAAAAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAACAAQDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwD5yooor88P1U//2Q==";

/// A PNG header alone, declaring 9000×9000 — past the default `MaxImageResolution`.
const PNG_HUGE: &str = "iVBORw0KGgoAAAANSUhEUgAAIygAACMoCAIAAADit+Xt";

fn b64(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap_or_default()
}

/// `imgutils.GenGIFData(1, 1, frames)`, which a frame count of zero leaves truncated: a header
/// `DecodeConfig` reads, and no frame or trailer for `CountGIFFrames`.
fn gif(frames: usize) -> Vec<u8> {
    let mut data = vec![
        b'G', b'I', b'F', b'8', b'9', b'a', 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 1, 1, 1,
    ];
    if frames == 0 {
        return data;
    }
    for _ in 0..frames {
        data.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x2, 0x2, 0x4c, 0x1, 0]);
    }
    data.push(0x3b);
    data
}

/// The files the suite uploaded through main Go into the trigger's channel, as the user.
pub struct Planted {
    /// Three texts, in upload order, sized 10, 30 and 20 bytes, so that time order and size
    /// order differ.
    pub a: String,
    pub b: String,
    pub c: String,
    /// An image attached to a post, with the mini preview the REST upload made.
    pub image: String,
    /// A text attached to a post that was then deleted, so the row is soft-deleted.
    pub deleted: String,
    /// `b`'s `CreateAt`, for the `Since` filter.
    pub since: i64,
}

impl Planted {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            a: var("HOOK_RECORDER_FILES_A"),
            b: var("HOOK_RECORDER_FILES_B"),
            c: var("HOOK_RECORDER_FILES_C"),
            image: var("HOOK_RECORDER_FILES_IMAGE"),
            deleted: var("HOOK_RECORDER_FILES_DELETED"),
            since: var("HOOK_RECORDER_FILES_SINCE").parse().unwrap_or(0),
        }
    }
}

fn side() -> String {
    std::env::var("HOOK_RECORDER_FILES_SIDE").unwrap_or_default()
}

async fn upload(
    api: &Client,
    out: &mut Vec<Json>,
    data: Vec<u8>,
    channel: &str,
    name: &str,
) -> Option<String> {
    let returns: Option<Z_UploadFileReturns> = call(
        api,
        out,
        "UploadFile",
        Z_UploadFileArgs {
            a: data,
            b: channel.to_owned(),
            c: name.to_owned(),
        },
    )
    .await;
    returns.and_then(|r| r.a).map(|info| info.id)
}

fn options(channel: &str) -> GetFileInfosOptions {
    GetFileInfosOptions {
        channel_ids: vec![channel.to_owned()],
        ..GetFileInfosOptions::default()
    }
}

async fn file_infos(
    api: &Client,
    out: &mut Vec<Json>,
    page: i64,
    per_page: i64,
    opt: Option<GetFileInfosOptions>,
) {
    let _: Option<Z_GetFileInfosReturns> = call(
        api,
        out,
        "GetFileInfos",
        Z_GetFileInfosArgs {
            a: page,
            b: per_page,
            c: opt.map(Box::new),
        },
    )
    .await;
}

/// The whole script, in order; see the module docs.
pub async fn run(api: &ApiClient, args: &CommandArgs) -> Vec<Json> {
    let client = api.client();
    let mut out = Vec::new();
    let planted = Planted::from_env();
    let tag = side();
    let channel = args.channel_id.as_str();
    let named = |stem: &str, ext: &str| format!("{stem}-mmrsfiles-{tag}.{ext}");

    // -- UploadFile --------------------------------------------------------------------------
    let text = upload(
        client,
        &mut out,
        b"hello from the files script".to_vec(),
        channel,
        &named("notes", "txt"),
    )
    .await
    .unwrap_or_default();
    let replaced = upload(
        client,
        &mut out,
        b"bytes a plugin replaces".to_vec(),
        channel,
        &named("hookreplace", "txt"),
    )
    .await
    .unwrap_or_default();
    upload(
        client,
        &mut out,
        b"renamed".to_vec(),
        channel,
        &named("hookrename", "txt"),
    )
    .await;
    upload(
        client,
        &mut out,
        b"refused".to_vec(),
        channel,
        &named("hookrefuse", "txt"),
    )
    .await;
    upload(
        client,
        &mut out,
        b64(PNG),
        channel,
        &named("picture", "png"),
    )
    .await;
    upload(
        client,
        &mut out,
        b64(JPEG_ROTATED),
        channel,
        &named("rotated", "jpg"),
    )
    .await;
    upload(
        client,
        &mut out,
        b64(PNG),
        channel,
        &named("hookunimage", "png"),
    )
    .await;
    upload(client, &mut out, gif(2), channel, &named("animated", "gif")).await;
    upload(client, &mut out, gif(1), channel, &named("still", "gif")).await;
    upload(
        client,
        &mut out,
        gif(0),
        channel,
        &named("truncated", "gif"),
    )
    .await;
    upload(
        client,
        &mut out,
        b64(PNG_HUGE),
        channel,
        &named("huge", "png"),
    )
    .await;
    upload(
        client,
        &mut out,
        b"not an image".to_vec(),
        channel,
        &named("garbage", "png"),
    )
    .await;
    upload(
        client,
        &mut out,
        b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"7\" height=\"5\"></svg>".to_vec(),
        channel,
        &named("vector", "svg"),
    )
    .await;
    upload(
        client,
        &mut out,
        b"no channel".to_vec(),
        "",
        &named("nochannel", "txt"),
    )
    .await;
    upload(
        client,
        &mut out,
        b"missing".to_vec(),
        MISSING,
        &named("missing", "txt"),
    )
    .await;
    upload(
        client,
        &mut out,
        Vec::new(),
        channel,
        &named("empty", "txt"),
    )
    .await;

    // -- GetFileInfo, GetFile, ReadFile, SetFileSearchableContent -----------------------------
    let info: Option<Z_GetFileInfoReturns> = call(
        client,
        &mut out,
        "GetFileInfo",
        Z_GetFileInfoArgs { a: text.clone() },
    )
    .await;
    for id in [&planted.image, &planted.deleted, &MISSING.to_owned()] {
        let _: Option<Z_GetFileInfoReturns> = call(
            client,
            &mut out,
            "GetFileInfo",
            Z_GetFileInfoArgs { a: id.clone() },
        )
        .await;
    }
    for id in [&text, &replaced, &planted.a, &MISSING.to_owned()] {
        let _: Option<Z_GetFileReturns> =
            call(client, &mut out, "GetFile", Z_GetFileArgs { a: id.clone() }).await;
    }
    let path = info.and_then(|i| i.a).map(|i| i.path).unwrap_or_default();
    for path in [path.as_str(), "mmrsfiles/no/such/file"] {
        let _: Option<Z_ReadFileReturns> = call(
            client,
            &mut out,
            "ReadFile",
            Z_ReadFileArgs { a: path.to_owned() },
        )
        .await;
    }
    for (id, content) in [
        (&text, "words a search could find"),
        (&planted.c, "planted words"),
        (&MISSING.to_owned(), "nowhere"),
        (&planted.deleted, "deleted"),
    ] {
        let _: Option<Z_SetFileSearchableContentReturns> = call(
            client,
            &mut out,
            "SetFileSearchableContent",
            Z_SetFileSearchableContentArgs {
                a: id.clone(),
                b: content.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_GetFileInfoReturns> = call(
        client,
        &mut out,
        "GetFileInfo",
        Z_GetFileInfoArgs { a: text.clone() },
    )
    .await;

    // -- GetFileInfos ------------------------------------------------------------------------
    file_infos(client, &mut out, 0, 100, Some(options(channel))).await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            sort_by: "Size".to_owned(),
            sort_descending: true,
            ..options(channel)
        }),
    )
    .await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            sort_by: "Size".to_owned(),
            ..options(channel)
        }),
    )
    .await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            sort_descending: true,
            include_deleted: true,
            ..options(channel)
        }),
    )
    .await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            since: planted.since,
            ..options(channel)
        }),
    )
    .await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            only_empty_content: true,
            ..options(channel)
        }),
    )
    .await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            user_ids: vec![args.user_id.clone(), MISSING.to_owned()],
            ..options(channel)
        }),
    )
    .await;
    file_infos(client, &mut out, 1, 2, Some(options(channel))).await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            sort_by: "Name".to_owned(),
            ..options(channel)
        }),
    )
    .await;
    file_infos(client, &mut out, -1, 10, Some(options(channel))).await;
    file_infos(client, &mut out, 0, -1, Some(options(channel))).await;
    file_infos(client, &mut out, 0, 0, Some(options(channel))).await;
    file_infos(
        client,
        &mut out,
        0,
        100,
        Some(GetFileInfosOptions {
            channel_ids: vec![MISSING.to_owned()],
            ..GetFileInfosOptions::default()
        }),
    )
    .await;

    // -- GetFileLink -------------------------------------------------------------------------
    for id in [&planted.image, &text, &MISSING.to_owned()] {
        let _: Option<Z_GetFileLinkReturns> = call(
            client,
            &mut out,
            "GetFileLink",
            Z_GetFileLinkArgs { a: id.clone() },
        )
        .await;
    }

    // -- CopyFileInfos -----------------------------------------------------------------------
    let copies: Option<Z_CopyFileInfosReturns> = call(
        client,
        &mut out,
        "CopyFileInfos",
        Z_CopyFileInfosArgs {
            a: args.user_id.clone(),
            b: vec![planted.b.clone(), planted.image.clone()],
        },
    )
    .await;
    for (user, ids) in [
        (
            args.user_id.clone(),
            vec![planted.a.clone(), MISSING.to_owned()],
        ),
        ("notauser".to_owned(), vec![planted.a.clone()]),
        (args.user_id.clone(), Vec::new()),
        (args.user_id.clone(), vec![planted.deleted.clone()]),
    ] {
        let _: Option<Z_CopyFileInfosReturns> = call(
            client,
            &mut out,
            "CopyFileInfos",
            Z_CopyFileInfosArgs { a: user, b: ids },
        )
        .await;
    }
    for id in copies.map(|c| c.a).unwrap_or_default() {
        let _: Option<Z_GetFileInfoReturns> =
            call(client, &mut out, "GetFileInfo", Z_GetFileInfoArgs { a: id }).await;
    }

    // -- OpenInteractiveDialog ---------------------------------------------------------------
    let dialog =
        |trigger: &str, title: &str, elements: Vec<DialogElement>| Z_OpenInteractiveDialogArgs {
            a: OpenDialogRequest {
                trigger_id: trigger.to_owned(),
                url: "http://localhost/hookrec/dialog".to_owned(),
                dialog: Dialog {
                    callback_id: "hookrec-callback".to_owned(),
                    title: title.to_owned(),
                    introduction_text: "Introduced **here**".to_owned(),
                    elements,
                    submit_label: "Send".to_owned(),
                    notify_on_cancel: true,
                    state: "hookrec-state".to_owned(),
                    ..Dialog::default()
                },
            },
        };
    let element = DialogElement {
        display_name: "Pick".to_owned(),
        name: "pick".to_owned(),
        r#type: "select".to_owned(),
        help_text: "one of them".to_owned(),
        options: vec![
            PostActionOptions {
                text: "One".to_owned(),
                value: "1".to_owned(),
            },
            PostActionOptions {
                text: "Two".to_owned(),
                value: "2".to_owned(),
            },
        ],
        ..DialogElement::default()
    };
    for args in [
        dialog(&args.trigger_id, "A dialog", vec![element.clone()]),
        dialog(&args.trigger_id, "", Vec::new()),
        dialog("not a trigger", "A dialog", vec![element]),
        dialog("", "A dialog", Vec::new()),
    ] {
        let _: Option<Z_OpenInteractiveDialogReturns> =
            call(client, &mut out, "OpenInteractiveDialog", args).await;
    }

    // -- SendMail ----------------------------------------------------------------------------
    for (to, subject, body) in [
        ("", "s", "b"),
        ("hookrec@example.com", "", "b"),
        ("hookrec@example.com", "s", ""),
        ("", "", ""),
        ("hookrec@example.com", "Subject", "<b>body</b>"),
    ] {
        let _: Option<Z_SendMailReturns> = call(
            client,
            &mut out,
            "SendMail",
            Z_SendMailArgs {
                a: to.to_owned(),
                b: subject.to_owned(),
                c: body.to_owned(),
            },
        )
        .await;
    }

    // -- PluginHTTP --------------------------------------------------------------------------
    let with_headers = [("X-Hookrec", "files"), ("User-Agent", "hookrec-agent/1.0")];
    for (method, url, headers, body) in [
        (
            "POST",
            "/mmrs.hookrecorder/echo?b=2&a=1&a=0",
            &with_headers[..],
            Some("ping"),
        ),
        ("GET", "/mmrs.hookrecorder/status", &with_headers[..], None),
        ("GET", "/mmrs.hookrecorder/late", &with_headers[..], None),
        ("GET", "/mmrs.hookrecorder/silent", &with_headers[..], None),
        ("GET", "/mmrs.hookrecorder/echo", &[][..], None),
        ("GET", "/mmrs.nosuchplugin/echo", &with_headers[..], None),
        ("GET", "/", &with_headers[..], None),
        ("GET", "/mmrs.hookrecorder", &with_headers[..], None),
        ("GET", "http://host//echo", &with_headers[..], None),
    ] {
        out.push(plugin_http(api, method, url, headers, body).await);
    }

    out
}

/// One `PluginHTTP`, written down as the head and body the plugin got back.
async fn plugin_http(
    api: &ApiClient,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Json {
    let request = HTTPRequestSubset {
        method: method.to_owned(),
        url: Some(gobwire::BinaryBytes(url.as_bytes().to_vec())),
        proto: "HTTP/1.1".to_owned(),
        proto_major: 1,
        proto_minor: 1,
        header: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), vec![(*v).to_owned()]))
            .collect(),
        host: "hookrec.invalid".to_owned(),
        remote_addr: "192.0.2.1:1234".to_owned(),
        request_uri: url.to_owned(),
        body: None,
    };
    let response = api
        .plugin_http(
            Some(Box::new(request)),
            body.map(|b| std::io::Cursor::new(b.as_bytes().to_vec())),
        )
        .await;
    let args = json!({ "method": method, "url": url, "headers": headers, "body": body });
    let Some(response) = response else {
        return json!({ "call": "PluginHTTP", "args": args, "returns": null });
    };
    let mut received = Vec::new();
    let _ = response
        .body
        .take(BODY_CAP)
        .read_to_end(&mut received)
        .await;
    let header: std::collections::BTreeMap<_, _> = response.header.into_iter().collect();
    json!({
        "call": "PluginHTTP",
        "args": args,
        "returns": {
            "StatusCode": response.status_code,
            "Header": header,
            "Body": String::from_utf8_lossy(&received),
        },
    })
}

/// `ServeHTTP` for the inter-plugin requests: what arrived is written down, and the answer is
/// chosen by the path.
///
/// | path | answer |
/// |---|---|
/// | `/echo` | 202, `X-Echo: <the X-Hookrec header>`, `echo: <body> <query>` |
/// | `/status` | 418 and no body |
/// | `/late` | a body, **then** a 500 status, which is too late to count |
/// | `/silent` | a header set, and nothing written |
/// | `/html` | a page with no `Content-Type`, which the host sniffs |
/// | `/flushed` | a byte, a flush, a byte: the head goes out at the flush |
/// | `/big` | [`BIG_CHUNKS`] writes of [`BIG_CHUNK`] bytes each, streamed |
/// | `/suggest/fetch` | `/hookrec fetch`'s dynamic list, after a first write that is not JSON |
/// | anything else | Go's 404 |
///
/// The whole body is read (up to [`SERVED_BODY_CAP`]), so neither host is left holding an
/// unread one; the transcript keeps its first [`BODY_CAP`] bytes, its length and a hash.
pub async fn serve(
    context: Option<Box<Context>>,
    request: Option<Box<HTTPRequestSubset>>,
    body: Option<RemoteReader>,
    mut writer: mm_plugin::http::RemoteResponseWriter,
) -> Json {
    let mut whole = Vec::new();
    if let Some(body) = body {
        let _ = body.take(SERVED_BODY_CAP).read_to_end(&mut whole).await;
    }
    let received = &whole[..whole.len().min(BODY_CAP as usize)];
    let request = request.map(|r| *r).unwrap_or_default();
    let url = request
        .url
        .as_ref()
        .map(|u| String::from_utf8_lossy(&u.0).into_owned())
        .unwrap_or_default();
    let header: std::collections::BTreeMap<_, _> = request.header.clone().into_iter().collect();
    let entry = json!({
        "hook": "ServeHTTP",
        "args": {
            "A": render_typed(&context),
            "Method": request.method,
            "URL": url,
            "Proto": request.proto,
            "Header": header,
            "Host": request.host,
            "RemoteAddr": request.remote_addr,
            "RequestURI": request.request_uri,
            "Body": String::from_utf8_lossy(received),
            "BodyLen": whole.len(),
            "BodyHash": fnv1a(&whole),
        },
    });

    let path = url.split('?').next().unwrap_or_default().to_owned();
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    match path.as_str() {
        "/echo" => {
            let echoed = header
                .get("X-Hookrec")
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_default();
            writer
                .header()
                .await
                .insert("X-Echo".to_owned(), vec![echoed]);
            writer.write_header(202).await;
            let _ = writer
                .write(format!("echo: {} {query}", String::from_utf8_lossy(received)).as_bytes())
                .await;
        }
        "/status" => writer.write_header(418).await,
        "/late" => {
            let _ = writer.write(b"early body").await;
            writer.write_header(500).await;
        }
        "/suggest/fetch" => {
            // Two writes: the server's own writer keeps only the last. Go decodes the first JSON
            // value — keys case-insensitively, a non-string field left empty, a non-object element
            // a zero item — and never reads what follows it.
            let _ = writer.write(b"[not json").await;
            let _ = writer
                .write(
                    br#"[{"item":"one","hint":"h1","helptext":"first"},{"Item":"two","HINT":"h2","HelpText":5},null,"x",{"ITEM":"three words"}] trailing"#,
                )
                .await;
        }
        "/html" => {
            let _ = writer.write(b"<html><body>hookrec</body></html>").await;
        }
        "/flushed" => {
            let _ = writer.write(b"a").await;
            writer.flush().await;
            let _ = writer.write(b"b").await;
        }
        "/big" => {
            for n in 0..BIG_CHUNKS {
                let chunk: Vec<u8> = (0..BIG_CHUNK)
                    .map(|i| b'a' + ((i + n) % 26) as u8)
                    .collect();
                let _ = writer.write(&chunk).await;
            }
        }
        "/silent" => {
            writer
                .header()
                .await
                .insert("X-Silent".to_owned(), vec!["1".to_owned()]);
        }
        _ => writer.not_found().await,
    }
    entry
}
