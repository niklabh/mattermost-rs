//! `ServeHTTP` and `ServeMetrics` (client_rpc.go), the hooks that carry a stream each way.
//!
//! The host lends two connections: the response writer, served as net/rpc ([`crate::http`]), and
//! the request body, served as a reader ([`crate::io_rpc`]). A request with no body sends the id
//! `0`, which the plugin reads as "no body" rather than as a stream to dial. The call itself
//! answers nothing; what the plugin writes goes back over the writer connection while the call is
//! outstanding.

use std::future::Future;
use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use goplugin::MuxBroker;
use goplugin::rpc::Empty;
use tokio::io::AsyncRead;

use super::{HooksClient, NotImplemented, hook_id};
use crate::http::{RemoteResponseWriter, ResponseWriter, response_writer_server};
use crate::io_rpc::{RemoteReader, serve_reader};
use crate::wire::plugin::{Context, HTTPRequestSubset, Z_ServeHTTPArgs, Z_ServeMetricsArgs};

/// The hooks that serve HTTP, which the generated [`super::Hooks`] cannot describe: their
/// arguments are connections, not values.
pub trait HooksHttp: Send + Sync + 'static {
    /// Go: `ServeHTTP(c *Context, w http.ResponseWriter, r *http.Request)`.
    ///
    /// The default is Go's: a plugin whose `ServeHTTP` is missing answers `404 page not found`
    /// (client_rpc.go, `hooksRPCServer.ServeHTTP`).
    fn serve_http(
        &self,
        context: Option<Box<Context>>,
        request: Option<Box<HTTPRequestSubset>>,
        body: Option<RemoteReader>,
        writer: RemoteResponseWriter,
    ) -> impl Future<Output = Result<(), NotImplemented>> + Send {
        let _ = (context, request, body);
        async move {
            let mut writer = writer;
            writer.not_found().await;
            Ok(())
        }
    }

    /// Go: `ServeMetrics(c *Context, w http.ResponseWriter, r *http.Request)`.
    fn serve_metrics(
        &self,
        context: Option<Box<Context>>,
        request: Option<Box<HTTPRequestSubset>>,
        body: Option<RemoteReader>,
        writer: RemoteResponseWriter,
    ) -> impl Future<Output = Result<(), NotImplemented>> + Send {
        let _ = (context, request, body);
        async move {
            let mut writer = writer;
            writer.not_found().await;
            Ok(())
        }
    }
}

/// Register the HTTP hooks on a plugin's server.
pub(super) fn register_hooks_http<H: HooksHttp>(
    server: &mut Server,
    hooks: &Arc<H>,
    broker: &MuxBroker,
) {
    let this = Arc::clone(hooks);
    let b = broker.clone();
    server.register("Plugin.ServeHTTP", move |args: Z_ServeHTTPArgs| {
        let (this, broker) = (Arc::clone(&this), b.clone());
        async move {
            let (writer, body) = dial_http(
                &broker,
                args.response_writer_stream,
                args.request_body_stream,
            )
            .await?;
            let connection = writer.client().clone();
            let _ = this
                .serve_http(args.context, args.request, body, writer)
                .await;
            // Go's `defer w.Close()`: the writer, and a connection hijacked through it, end with
            // the hook.
            let _ = connection.close().await;
            Ok::<_, ServiceError>(Empty {})
        }
    });

    let this = Arc::clone(hooks);
    let b = broker.clone();
    server.register("Plugin.ServeMetrics", move |args: Z_ServeMetricsArgs| {
        let (this, broker) = (Arc::clone(&this), b.clone());
        async move {
            let (writer, body) = dial_http(
                &broker,
                args.response_writer_stream,
                args.request_body_stream,
            )
            .await?;
            let connection = writer.client().clone();
            let _ = this
                .serve_metrics(args.context, args.request, body, writer)
                .await;
            // Go's `defer w.Close()`: the writer, and a connection hijacked through it, end with
            // the hook.
            let _ = connection.close().await;
            Ok::<_, ServiceError>(Empty {})
        }
    });
}

/// Dial the two connections a served request needs. A failed dial fails the call, as Go's does.
async fn dial_http(
    broker: &MuxBroker,
    writer_stream: u32,
    body_stream: u32,
) -> Result<(RemoteResponseWriter, Option<RemoteReader>), ServiceError> {
    let writer = broker
        .dial(writer_stream)
        .await
        .map_err(|e| ServiceError(e.to_string()))?;
    let writer = RemoteResponseWriter::new(go_netrpc::Client::new(writer));
    // Go sends 0 for a request with no body, and reads an empty one in its place.
    let body = if body_stream == 0 {
        None
    } else {
        Some(RemoteReader::new(
            broker
                .dial(body_stream)
                .await
                .map_err(|e| ServiceError(e.to_string()))?,
        ))
    };
    Ok((writer, body))
}

impl HooksClient {
    /// Go: `ServeHTTP(c *Context, w http.ResponseWriter, r *http.Request)`.
    ///
    /// A plugin that does not implement it is not called at all: the host answers
    /// `404 page not found` itself, through `writer`.
    pub async fn serve_http<W, B>(
        &self,
        context: Option<Box<Context>>,
        request: Option<Box<HTTPRequestSubset>>,
        body: Option<B>,
        writer: W,
    ) where
        W: ResponseWriter,
        B: AsyncRead + Send + Unpin + 'static,
    {
        self.serve(
            hook_id::SERVE_HTTP,
            "ServeHTTP",
            body,
            writer,
            |response_writer_stream, request_body_stream| Z_ServeHTTPArgs {
                response_writer_stream,
                request,
                context,
                request_body_stream,
            },
        )
        .await;
    }

    /// Go: `ServeMetrics(c *Context, w http.ResponseWriter, r *http.Request)`.
    pub async fn serve_metrics<W, B>(
        &self,
        context: Option<Box<Context>>,
        request: Option<Box<HTTPRequestSubset>>,
        body: Option<B>,
        writer: W,
    ) where
        W: ResponseWriter,
        B: AsyncRead + Send + Unpin + 'static,
    {
        self.serve(
            hook_id::SERVE_METRICS,
            "ServeMetrics",
            body,
            writer,
            |response_writer_stream, request_body_stream| Z_ServeMetricsArgs {
                response_writer_stream,
                request,
                context,
                request_body_stream,
            },
        )
        .await;
    }

    /// Both hooks, which differ only in the wire struct `make_args` builds from the two ids.
    async fn serve<W, B, A, F>(
        &self,
        id: usize,
        name: &'static str,
        body: Option<B>,
        writer: W,
        make_args: F,
    ) where
        W: ResponseWriter,
        B: AsyncRead + Send + Unpin + 'static,
        A: gobwire::Encode,
        F: FnOnce(u32, u32) -> A,
    {
        let mut writer = writer;
        if !self.implements(id) {
            not_found(&mut writer);
            return;
        }

        let response_writer_stream = self.broker.next_id();
        let broker = self.broker.clone();
        let served = response_writer_server(writer);
        tokio::spawn(async move {
            broker
                .accept_and_serve(response_writer_stream, Arc::new(served))
                .await;
        });

        // Go sends 0 when the request has no body, and lends a reader when it does.
        let mut request_body_stream = 0;
        if let Some(body) = body {
            request_body_stream = self.broker.next_id();
            let broker = self.broker.clone();
            tokio::spawn(async move {
                match broker.accept(request_body_stream).await {
                    Ok(conn) => {
                        if let Err(e) = serve_reader(body, conn).await {
                            tracing::debug!(error = %e, "{name}: serving the request body ended");
                        }
                    }
                    Err(e) => tracing::error!(
                        error = %e,
                        "Plugin failed to {name}, muxBroker couldn't Accept request body connection"
                    ),
                }
            });
        }

        let args = make_args(response_writer_stream, request_body_stream);
        let call: Result<Empty, _> = self.rpc(name, &args).await;
        if let Err(e) = call {
            tracing::error!(error = %e, "Plugin failed to {name}, RPC call failed");
            // The writer is on the other task now, so the 500 goes over the same connection.
            let broker = self.broker.clone();
            if let Ok(conn) = broker.dial(response_writer_stream).await {
                let mut remote = RemoteResponseWriter::new(go_netrpc::Client::new(conn));
                remote.error("500 internal server error", 500).await;
            }
        }
    }
}

/// Go's `http.NotFound` against a local writer, for a hook the plugin does not implement.
fn not_found<W: ResponseWriter>(writer: &mut W) {
    let mut header = writer.header();
    header.remove("Content-Length");
    header.insert(
        "Content-Type".into(),
        vec!["text/plain; charset=utf-8".into()],
    );
    header.insert("X-Content-Type-Options".into(), vec!["nosniff".into()]);
    writer.sync_header(header);
    writer.write_header(404);
    let _ = writer.write(crate::http::NOT_FOUND_BODY.as_bytes());
}
