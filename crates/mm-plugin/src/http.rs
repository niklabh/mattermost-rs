//! An `http.ResponseWriter` that lives in the other process (http.go).
//!
//! The host keeps the real writer and serves it as a net/rpc service on a brokered connection:
//! `Header`, `Write`, `WriteHeader`, `SyncHeader` and `Flush`. The plugin holds a client that
//! caches the header map locally and pushes it with `SyncHeader` before every write, so a header
//! set through the map reaches the host even though the map itself is a copy.

use std::sync::{Arc, Mutex};

use go_netrpc::{Server, ServiceError};
use goplugin::rpc::Empty;

use crate::wire::http::Header;

/// Go's `StatusNotFound` reply body, as `http.Error` writes it.
pub const NOT_FOUND_BODY: &str = "404 page not found\n";

/// The response to an HTTP request a plugin serves. The host implements it over whatever it is
/// really writing to.
///
/// The methods are `http.ResponseWriter`'s, minus what net/rpc cannot carry. Go's server refuses
/// a status outside 100..=999 rather than letting a plugin panic it (http.go, `WriteHeader`).
pub trait ResponseWriter: Send + 'static {
    /// The header map as it stands. Go answers with the real one, which the plugin then edits
    /// through its copy.
    fn header(&mut self) -> Header;

    /// Replace the header map with the plugin's copy, dropping keys it no longer has
    /// (http.go, `SyncHeader`).
    fn sync_header(&mut self, header: Header);

    fn write(&mut self, body: &[u8]) -> std::io::Result<()>;

    fn write_header(&mut self, status: i64);

    /// Go flushes when the writer is an `http.Flusher` and does nothing when it is not.
    fn flush(&mut self) {}
}

/// Serve `writer` to the plugin on one brokered connection (http.go,
/// `httpResponseWriterRPCServer`).
pub fn response_writer_server<W: ResponseWriter>(writer: W) -> Server {
    let writer = Arc::new(Mutex::new(writer));
    let mut server = Server::new();

    let w = Arc::clone(&writer);
    server.register("Plugin.Header", move |_: Empty| {
        let w = Arc::clone(&w);
        async move {
            let header = w.lock().map_err(poisoned)?.header();
            Ok::<_, ServiceError>(header)
        }
    });

    let w = Arc::clone(&writer);
    server.register("Plugin.SyncHeader", move |header: Header| {
        let w = Arc::clone(&w);
        async move {
            w.lock().map_err(poisoned)?.sync_header(header);
            Ok::<_, ServiceError>(Empty {})
        }
    });

    let w = Arc::clone(&writer);
    server.register("Plugin.Write", move |body: Vec<u8>| {
        let w = Arc::clone(&w);
        async move {
            w.lock()
                .map_err(poisoned)?
                .write(&body)
                .map_err(|e| ServiceError(e.to_string()))?;
            Ok::<_, ServiceError>(Empty {})
        }
    });

    let w = Arc::clone(&writer);
    server.register("Plugin.WriteHeader", move |status: i64| {
        let w = Arc::clone(&w);
        async move {
            // http.go: a status Go's own server would panic on is refused instead.
            if !(100..=999).contains(&status) {
                tracing::error!(
                    "Plugin tried to write an invalid http status code: {status}. Did not write the invalid header."
                );
                return Err(ServiceError("invalid http status code".into()));
            }
            w.lock().map_err(poisoned)?.write_header(status);
            Ok::<_, ServiceError>(Empty {})
        }
    });

    let w = Arc::clone(&writer);
    server.register("Plugin.Flush", move |_: Empty| {
        let w = Arc::clone(&w);
        async move {
            w.lock().map_err(poisoned)?.flush();
            Ok::<_, ServiceError>(Empty {})
        }
    });

    server
}

fn poisoned<T>(_: T) -> ServiceError {
    ServiceError("the response writer is poisoned".into())
}

/// The host's `http.ResponseWriter`, from the plugin (http.go, `httpResponseWriterRPCClient`).
///
/// [`RemoteResponseWriter::header`] fetches the map once and keeps it; every write pushes it back
/// first, which is how a header set locally reaches the host.
pub struct RemoteResponseWriter {
    client: go_netrpc::Client,
    header: Option<Header>,
}

impl RemoteResponseWriter {
    pub fn new(client: go_netrpc::Client) -> Self {
        Self {
            client,
            header: None,
        }
    }

    /// The header map to write into, fetched from the host on first use.
    pub async fn header(&mut self) -> &mut Header {
        if self.header.is_none() {
            // Go ignores a failure here and hands back whatever it has, which is nil.
            self.header = Some(
                self.client
                    .call("Plugin.Header", &Empty {})
                    .await
                    .unwrap_or_default(),
            );
        }
        self.header.get_or_insert_with(Header::default)
    }

    async fn sync_header(&mut self) -> Result<(), go_netrpc::Error> {
        let header = self.header.clone().unwrap_or_default();
        self.client
            .call::<_, Empty>("Plugin.SyncHeader", &header)
            .await
            .map(|_| ())
    }

    /// Write body bytes, after pushing the header map (http.go, `Write`).
    pub async fn write(&mut self, body: &[u8]) -> Result<usize, go_netrpc::Error> {
        self.sync_header().await?;
        self.client
            .call::<_, Empty>("Plugin.Write", &body.to_vec())
            .await?;
        Ok(body.len())
    }

    /// Set the status, after pushing the header map. Go drops a failure here, as the
    /// `http.ResponseWriter` interface gives it nowhere to report one.
    pub async fn write_header(&mut self, status: i64) {
        if self.sync_header().await.is_err() {
            return;
        }
        let _: Result<Empty, _> = self.client.call("Plugin.WriteHeader", &status).await;
    }

    /// Best effort, as `http.Flusher` is (http.go, `Flush`).
    pub async fn flush(&mut self) {
        let _: Result<Empty, _> = self.client.call("Plugin.Flush", &Empty {}).await;
    }

    /// Go's `http.Error`: reset the content type, set the status, then the message and a newline.
    pub async fn error(&mut self, message: &str, status: i64) {
        let header = self.header().await;
        header.remove("Content-Length");
        header.insert(
            "Content-Type".into(),
            vec!["text/plain; charset=utf-8".into()],
        );
        header.insert("X-Content-Type-Options".into(), vec!["nosniff".into()]);
        self.write_header(status).await;
        let _ = self.write(format!("{message}\n").as_bytes()).await;
    }

    /// Go's `http.NotFound`, which is what both sides answer for a plugin without `ServeHTTP`.
    pub async fn not_found(&mut self) {
        self.error("404 page not found", 404).await;
    }

    pub async fn close(&self) -> Result<(), go_netrpc::Error> {
        self.client.close().await
    }
}
