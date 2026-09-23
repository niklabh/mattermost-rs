//! Port of `PluginAPI.PluginHTTP` (app/plugin_api.go:1336) with `ServeInterPluginRequest` and
//! `ServeInternalPluginRequest` (app/plugin_requests.go:59) and the `PluginResponseWriter` it
//! answers through (app/response_transfer.go) — one plugin's HTTP request to another's
//! `ServeHTTP`.
//!
//! # The response is ready at the first byte, not at the end
//!
//! Go serves the destination's `ServeHTTP` on a goroutine and returns the `*http.Response` as
//! soon as the writer is **ready**: at the first `WriteHeader`, `Write` or `Flush`, or when the
//! hook returns without any. The status and the header are what they were then (a `WriteHeader`
//! after a `Write` is too late, and no status at all is 200); the body streams afterwards through
//! a pipe. [`PluginResponseWriter`] is that writer, with a channel for the pipe — which buffers
//! where Go's pipe blocks the writer until the reader takes the bytes, so only memory differs.
//!
//! # A request with no header reaches no plugin
//!
//! `ServeInternalPluginRequest` does `r.Header.Set("Mattermost-User-Id", "")` on the request the
//! plugin sent, and gob delivers an empty header map as **nil**: the `Set` panics, the goroutine's
//! `recover` closes the pipe with the panic as its error, and the calling plugin gets a 200 with
//! no header and an empty body — without the destination's `ServeHTTP` ever being called. This
//! reproduces that answer. A plugin built on the SDK sends every request through
//! `http.NewRequest`, whose header is empty unless the plugin set one, so this is the common case,
//! not a corner.
//!
//! # What Go does not survive, and this does
//!
//! A nil `URL`, or one `url.Parse` refuses on the way in, dereferences nil in Go (the RPC
//! handler panics and takes the server down); a rest-of-path `url.Parse` refuses panics the same
//! way, one line before the message Go wrote for it. All three are answered here with the 400
//! Go's message describes.

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use mm_model::go_url;
use mm_plugin::http::ResponseWriter;
use mm_plugin::rpc::{HttpResponse, NotImplemented};
use mm_plugin::wire::http::Header;
use mm_plugin::wire::plugin::{Context, HTTPRequestSubset};
use tokio::io::{AsyncRead, ReadBuf};

use super::AppPluginApi;

/// What the calling plugin receives before any plugin is asked: a status and a plain body, and
/// Go's nil header.
fn plain(status: i64, body: &str) -> HttpResponse {
    HttpResponse {
        status_code: status,
        header: Header::default(),
        body: Box::new(std::io::Cursor::new(body.as_bytes().to_vec())),
    }
}

/// Where `PluginHTTP` sends a request, or the 400 it answers instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterPluginTarget {
    /// The destination plugin and the URL it is handed: the path after the plugin id, and the
    /// query re-encoded by `url.Values.Encode` (sorted by key, canonically escaped).
    Plugin { id: String, url: String },
    /// A 400 with this body.
    Refused(String),
}

/// `PluginHTTP`'s routing (app/plugin_api.go:1337): `SplitN(Path, "/", 3)` must give three
/// parts, the second is the plugin, and `"/" + the third` is parsed as the new URL with the old
/// URL's query, re-encoded. The URL arrives as `url.URL`'s binary form, which is its string.
pub fn inter_plugin_target(url: Option<&[u8]>) -> InterPluginTarget {
    const NOT_ENOUGH: &str = "Not enough URL. Form of URL should be /<pluginid>/*";
    let Some(parsed) = url.and_then(|raw| go_url::go_parse(&String::from_utf8_lossy(raw)).ok())
    else {
        return InterPluginTarget::Refused(NOT_ENOUGH.to_owned());
    };
    let path = String::from_utf8_lossy(&parsed.path).into_owned();
    let split: Vec<&str> = path.splitn(3, '/').collect();
    if split.len() != 3 {
        return InterPluginTarget::Refused(NOT_ENOUGH.to_owned());
    }
    let destination = split[1];
    let new_url = go_url::go_parse(&format!("/{}", split[2]));
    match new_url {
        Ok(mut new_url) if !destination.is_empty() => {
            new_url.raw_query = parsed.query().encode();
            InterPluginTarget::Plugin {
                id: destination.to_owned(),
                url: new_url.to_go_string(),
            }
        }
        Ok(_) => InterPluginTarget::Refused(
            "No plugin specified. Form of URL should be /<pluginid>/*".to_owned(),
        ),
        Err(err) => {
            InterPluginTarget::Refused(format!("Form of URL should be /<pluginid>/* Error: {err}"))
        }
    }
}

/// Port of `PluginResponseWriter` (app/response_transfer.go): the header map, the first status,
/// the pipe the body goes through, and the one-shot that hands the head to the caller.
pub struct PluginResponseWriter {
    header: Header,
    status: i64,
    ready: Option<tokio::sync::oneshot::Sender<(i64, Header)>>,
    body: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

impl PluginResponseWriter {
    /// A writer, the head it will hand over, and the body it will stream.
    pub fn new() -> (
        Self,
        tokio::sync::oneshot::Receiver<(i64, Header)>,
        ChannelReader,
    ) {
        let (ready, head) = tokio::sync::oneshot::channel();
        let (body, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                header: Header::default(),
                status: 0,
                ready: Some(ready),
                body: Some(body),
            },
            head,
            ChannelReader {
                rx,
                pending: Vec::new(),
                at: 0,
            },
        )
    }

    /// `markResponseReady` with `GenerateResponse`: the status (200 when none was written) and
    /// the header as they stand, once.
    fn mark_ready(&mut self) {
        if let Some(ready) = self.ready.take() {
            let status = if self.status == 0 { 200 } else { self.status };
            let _ = ready.send((status, self.header.clone()));
        }
    }
}

impl ResponseWriter for PluginResponseWriter {
    fn header(&mut self) -> Header {
        self.header.clone()
    }

    fn sync_header(&mut self, header: Header) {
        self.header = header;
    }

    fn write(&mut self, body: &[u8]) -> std::io::Result<()> {
        self.mark_ready();
        match &self.body {
            Some(pipe) if pipe.send(body.to_vec()).is_ok() => Ok(()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io: read/write on closed pipe",
            )),
        }
    }

    /// Only the first status counts. The guard is Go's, and here it is observably redundant:
    /// the status is read only when the head is handed over, which happens once, at the first
    /// status or byte — so a later status can reach no one either way (a mutation removing it
    /// survives for that reason, not for want of a test).
    fn write_header(&mut self, status: i64) {
        if self.status == 0 {
            self.status = status;
            self.mark_ready();
        }
    }

    fn flush(&mut self) {
        self.mark_ready();
    }
}

/// `Close`: ready if nothing made it so, and the pipe's end.
impl Drop for PluginResponseWriter {
    fn drop(&mut self) {
        self.mark_ready();
        self.body.take();
    }
}

/// The read half of the pipe.
pub struct ChannelReader {
    rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pending: Vec<u8>,
    at: usize,
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if self.at < self.pending.len() {
                let n = (self.pending.len() - self.at).min(buf.remaining());
                let at = self.at;
                buf.put_slice(&self.pending[at..at + n]);
                self.at += n;
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    self.pending = chunk;
                    self.at = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AppPluginApi {
    /// Port of `PluginAPI.PluginHTTP` (app/plugin_api.go:1336); see the module docs.
    pub(super) async fn http_plugin_http(
        &self,
        request: Option<Box<HTTPRequestSubset>>,
        body: Box<dyn AsyncRead + Send + Unpin>,
    ) -> Result<HttpResponse, NotImplemented> {
        let mut request = request.map(|r| *r).unwrap_or_default();
        let (destination, url) = match inter_plugin_target(request.url.as_ref().map(|u| &u.0[..])) {
            InterPluginTarget::Plugin { id, url } => (id, url),
            InterPluginTarget::Refused(message) => return Ok(plain(400, &message)),
        };
        request.url = Some(gobwire::BinaryBytes(url.into_bytes()));

        // `ServeInternalPluginRequest`, with no user.
        let Some(environment) = self.app.plugins_environment() else {
            let body = self.plugins_disabled_body();
            return Ok(HttpResponse {
                status_code: 501,
                header: Header::default(),
                body: Box::new(std::io::Cursor::new(body.into_bytes())),
            });
        };
        let hooks = match environment.hooks_for_plugin(&destination) {
            Ok(hooks) => hooks,
            Err(err) => {
                tracing::error!(
                    source_plugin_id = %self.id,
                    target_plugin_id = %destination,
                    error = %err,
                    "Access to route for non-existent plugin in internal plugin request"
                );
                return Ok(not_found());
            }
        };
        if request.header.is_empty() {
            // `r.Header.Set` on gob's nil map: see the module docs.
            tracing::error!(
                source_plugin_id = %self.id,
                "Failed to close plugin response pipe: panic in plugin request: assignment to entry in nil map"
            );
            return Ok(HttpResponse {
                status_code: 200,
                ..HttpResponse::default()
            });
        }
        let context = Context {
            request_id: mm_model::utils::new_id(),
            user_agent: request
                .header
                .get("User-Agent")
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_default(),
            ..Context::default()
        };
        request
            .header
            .insert("Mattermost-User-Id".to_owned(), vec![String::new()]);
        request
            .header
            .insert("Mattermost-Plugin-Id".to_owned(), vec![self.id.clone()]);

        let (writer, head, reader) = PluginResponseWriter::new();
        tokio::spawn(async move {
            hooks
                .serve_http(
                    Some(Box::new(context)),
                    Some(Box::new(request)),
                    Some(body),
                    writer,
                )
                .await;
        });
        let (status_code, header) = head.await.unwrap_or((200, Header::default()));
        Ok(HttpResponse {
            status_code,
            header,
            body: Box::new(reader),
        })
    }

    /// `app.plugin.disabled.app_error` as `appErr.ToJSON()` writes it: Go's plugins-off answer,
    /// reachable only while the environment is being torn down.
    fn plugins_disabled_body(&self) -> String {
        let mut err = mm_model::utils::AppError::new(
            "ServeInternalPluginRequest",
            "app.plugin.disabled.app_error",
            None,
            "Plugin environment not found.",
            501,
        );
        if let Some(bundle) = crate::i18n::loaded() {
            bundle.translate_app_error(
                bundle.server_locale(&self.app.config().default_server_locale),
                &mut err,
            );
        }
        serde_json::to_string(&err).unwrap_or_default()
    }
}

/// Go's `http.NotFound` into the writer: `404 page not found`, as plain text.
fn not_found() -> HttpResponse {
    let header = Header::from([
        (
            "Content-Type".to_owned(),
            vec!["text/plain; charset=utf-8".to_owned()],
        ),
        (
            "X-Content-Type-Options".to_owned(),
            vec!["nosniff".to_owned()],
        ),
    ]);
    HttpResponse {
        status_code: 404,
        header,
        body: Box::new(std::io::Cursor::new(
            mm_plugin::http::NOT_FOUND_BODY.as_bytes().to_vec(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    fn target(url: &str) -> InterPluginTarget {
        inter_plugin_target(Some(url.as_bytes()))
    }

    #[test]
    fn the_first_segment_names_the_plugin_and_the_query_is_re_encoded() {
        assert_eq!(
            target("/com.example.p/api/v1/x?b=2&a=1&a=0"),
            InterPluginTarget::Plugin {
                id: "com.example.p".into(),
                url: "/api/v1/x?a=1&a=0&b=2".into(),
            }
        );
        assert_eq!(
            target("http://host/p/"),
            InterPluginTarget::Plugin {
                id: "p".into(),
                url: "/".into(),
            }
        );
    }

    #[test]
    fn a_short_or_empty_path_is_refused_with_gos_messages() {
        let not_enough = InterPluginTarget::Refused(
            "Not enough URL. Form of URL should be /<pluginid>/*".into(),
        );
        assert_eq!(target("/"), not_enough);
        assert_eq!(target("/p"), not_enough);
        assert_eq!(target(""), not_enough);
        assert_eq!(inter_plugin_target(None), not_enough);
        // `//x` alone is a host, not a path; the empty plugin id needs a path of `//x`.
        assert_eq!(target("//x"), not_enough);
        assert_eq!(
            target("http://h//x"),
            InterPluginTarget::Refused(
                "No plugin specified. Form of URL should be /<pluginid>/*".into()
            )
        );
    }

    /// The status and header are the ones in place at the first write; a late status is lost,
    /// and no status at all is 200.
    #[tokio::test]
    async fn the_head_is_taken_at_the_first_write() {
        let (mut writer, head, mut reader) = PluginResponseWriter::new();
        let mut header = writer.header();
        header.insert("X-A".into(), vec!["1".into()]);
        writer.sync_header(header);
        writer.write(b"he").expect("written");
        writer.write_header(500);
        writer.write(b"llo").expect("written");
        drop(writer);
        let (status, header) = head.await.expect("ready");
        assert_eq!(status, 200);
        assert_eq!(header["X-A"], vec!["1".to_owned()]);
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await.expect("read");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn only_the_first_status_counts_and_a_silent_writer_is_ready_on_close() {
        let (mut writer, head, _reader) = PluginResponseWriter::new();
        writer.write_header(201);
        writer.write_header(418);
        assert_eq!(head.await.expect("ready").0, 201);

        let (writer, head, mut reader) = PluginResponseWriter::new();
        drop(writer);
        assert_eq!(head.await.expect("ready").0, 200);
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await.expect("read");
        assert!(body.is_empty());
    }
}
