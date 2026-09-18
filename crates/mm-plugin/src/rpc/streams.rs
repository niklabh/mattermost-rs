//! The API methods that carry an `io.Reader` (client_rpc.go).
//!
//! The caller keeps the reader and lends it over a brokered connection: it allocates an id,
//! serves the reader on it ([`crate::io_rpc::serve_reader`]), and sends the id as an ordinary
//! field of the wire struct. The far side dials that id and reads through it while the call is
//! outstanding, so the answer comes back only once it has read what it wants.

use std::future::Future;
use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use goplugin::MuxBroker;
use tokio::io::{AsyncRead, AsyncWriteExt};

use super::{ApiClient, NotImplemented};
use crate::io_rpc::{RemoteReader, serve_reader};
use crate::wire::http::Header;
use crate::wire::model::{FileInfo, UploadSession};
use crate::wire::plugin::HTTPRequestSubset;
use crate::wire::plugin::{
    Z_InstallPluginArgs, Z_InstallPluginReturns, Z_PluginHTTPArgs, Z_PluginHTTPReturns,
    Z_PluginHTTPStreamArgs, Z_PluginHTTPStreamReturns, Z_ReceiveSharedChannelAttachmentSyncMsgArgs,
    Z_ReceiveSharedChannelAttachmentSyncMsgReturns, Z_UploadDataArgs, Z_UploadDataReturns,
};

/// The server API whose methods take a reader, which the generated [`super::PluginApi`] cannot
/// describe. A host implements both.
pub trait PluginApiStreams: Send + Sync + 'static {
    /// Go: `UploadData(us *model.UploadSession, rd io.Reader) (*model.FileInfo, error)`.
    fn upload_data(
        &self,
        session: Option<Box<UploadSession>>,
        data: RemoteReader,
    ) -> impl Future<Output = Result<Z_UploadDataReturns, NotImplemented>> + Send {
        let _ = (session, data);
        async { Err(NotImplemented) }
    }

    /// Go: `InstallPlugin(file io.Reader, replace bool) (*model.Manifest, *model.AppError)`.
    fn install_plugin(
        &self,
        bundle: RemoteReader,
        replace: bool,
    ) -> impl Future<Output = Result<Z_InstallPluginReturns, NotImplemented>> + Send {
        let _ = (bundle, replace);
        async { Err(NotImplemented) }
    }

    /// Go: `ReceiveSharedChannelAttachmentSyncMsg(remoteID, channelID string, fi *model.FileInfo,
    /// data io.Reader) (*model.FileInfo, error)`.
    fn receive_shared_channel_attachment_sync_msg(
        &self,
        remote_id: String,
        channel_id: String,
        file: Option<Box<FileInfo>>,
        data: RemoteReader,
    ) -> impl Future<Output = Result<Z_ReceiveSharedChannelAttachmentSyncMsgReturns, NotImplemented>>
    + Send {
        let _ = (remote_id, channel_id, file, data);
        async { Err(NotImplemented) }
    }
}

/// Dial the stream the caller named, or fail the call as Go does with the dial error.
async fn lend(broker: &MuxBroker, id: u32) -> Result<RemoteReader, ServiceError> {
    broker
        .dial(id)
        .await
        .map(RemoteReader::new)
        .map_err(|e| ServiceError(e.to_string()))
}

/// Register the streaming half of the server API. Their not-implemented messages are Go's, which
/// (being hand-written) carry no full stop.
pub(super) fn register_api_streams<T: PluginApiStreams>(
    server: &mut Server,
    implementation: &Arc<T>,
    broker: &MuxBroker,
) {
    let this = Arc::clone(implementation);
    let b = broker.clone();
    server.register("Plugin.UploadData", move |args: Z_UploadDataArgs| {
        let (this, broker) = (Arc::clone(&this), b.clone());
        async move {
            let data = lend(&broker, args.plugin_stream_id).await?;
            this.upload_data(args.a, data)
                .await
                .map_err(|NotImplemented| {
                    ServiceError("API UploadData called but not implemented".into())
                })
        }
    });

    let this = Arc::clone(implementation);
    let b = broker.clone();
    server.register("Plugin.InstallPlugin", move |args: Z_InstallPluginArgs| {
        let (this, broker) = (Arc::clone(&this), b.clone());
        async move {
            let bundle = lend(&broker, args.plugin_stream_id).await?;
            this.install_plugin(bundle, args.b)
                .await
                .map_err(|NotImplemented| {
                    ServiceError("API InstallPlugin called but not implemented".into())
                })
        }
    });

    let this = Arc::clone(implementation);
    let b = broker.clone();
    server.register(
        "Plugin.ReceiveSharedChannelAttachmentSyncMsg",
        move |args: Z_ReceiveSharedChannelAttachmentSyncMsgArgs| {
            let (this, broker) = (Arc::clone(&this), b.clone());
            async move {
                let data = lend(&broker, args.data_stream_id).await?;
                this.receive_shared_channel_attachment_sync_msg(args.a, args.b, args.c, data)
                    .await
                    .map_err(|NotImplemented| {
                        ServiceError(
                            "API ReceiveSharedChannelAttachmentSyncMsg called but not implemented"
                                .into(),
                        )
                    })
            }
        },
    );
}

impl ApiClient {
    /// Lend `reader` on a brokered connection and answer with its id, for a wire struct's stream
    /// field. Go accepts in a goroutine and logs a failed accept without failing the call.
    fn lend_reader<R>(&self, reader: R, what: &'static str) -> u32
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let id = self.broker.next_id();
        let broker = self.broker.clone();
        tokio::spawn(async move {
            match broker.accept(id).await {
                Ok(conn) => {
                    if let Err(e) = serve_reader(reader, conn).await {
                        tracing::debug!(error = %e, "{what}: serving the stream ended");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "{what}: MuxBroker could not Accept connection")
                }
            }
        });
        id
    }

    /// Go: `UploadData(us *model.UploadSession, rd io.Reader) (*model.FileInfo, error)`.
    pub async fn upload_data<R>(
        &self,
        session: Option<Box<UploadSession>>,
        data: R,
    ) -> Z_UploadDataReturns
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let args = Z_UploadDataArgs {
            a: session,
            plugin_stream_id: self.lend_reader(data, "UploadData"),
        };
        self.call("UploadData", &args).await
    }

    /// Go: `InstallPlugin(file io.Reader, replace bool) (*model.Manifest, *model.AppError)`.
    pub async fn install_plugin<R>(&self, bundle: R, replace: bool) -> Z_InstallPluginReturns
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let args = Z_InstallPluginArgs {
            plugin_stream_id: self.lend_reader(bundle, "InstallPlugin"),
            b: replace,
        };
        self.call("InstallPlugin", &args).await
    }

    /// Go: `ReceiveSharedChannelAttachmentSyncMsg(remoteID, channelID string, fi *model.FileInfo,
    /// data io.Reader) (*model.FileInfo, error)`.
    pub async fn receive_shared_channel_attachment_sync_msg<R>(
        &self,
        remote_id: String,
        channel_id: String,
        file: Option<Box<FileInfo>>,
        data: R,
    ) -> Z_ReceiveSharedChannelAttachmentSyncMsgReturns
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let args = Z_ReceiveSharedChannelAttachmentSyncMsgArgs {
            a: remote_id,
            b: channel_id,
            c: file,
            data_stream_id: self.lend_reader(data, "ReceiveSharedChannelAttachmentSyncMsg"),
        };
        self.call("ReceiveSharedChannelAttachmentSyncMsg", &args)
            .await
    }
}

/// What a host answers a plugin's outward HTTP call with (client_rpc.go, `PluginHTTPStream`).
pub struct HttpResponse {
    pub status_code: i64,
    pub header: Header,
    /// Streamed to the plugin over its own connection, after the call has answered.
    pub body: Box<dyn AsyncRead + Send + Unpin>,
}

impl Default for HttpResponse {
    fn default() -> Self {
        Self {
            status_code: 0,
            header: Header::default(),
            body: Box::new(tokio::io::empty()),
        }
    }
}

/// What the plugin gets back: the head, and the body as it arrives.
pub struct RemoteHttpResponse {
    pub status_code: i64,
    pub header: Header,
    /// The host pushes the body raw, so this reads the connection directly rather than through
    /// a [`RemoteReader`]. Go asks for bytes over it that the host never reads (client_rpc.go,
    /// `pluginHTTPStream` wraps it in `connectIOReader` while the host answers with a plain
    /// `io.Copy`); this does not send those, which changes only bytes nobody consumes.
    pub body: Box<dyn AsyncRead + Send + Unpin>,
}

/// The outward HTTP call, whose server half both wire shapes share.
pub trait PluginApiHttp: Send + Sync + 'static {
    /// Go: `PluginHTTP(request *http.Request) *http.Response`.
    ///
    /// The body is a reader either way: a connection for the streaming shape, and the inline
    /// bytes for the buffered one.
    fn plugin_http(
        &self,
        request: Option<Box<HTTPRequestSubset>>,
        body: Box<dyn AsyncRead + Send + Unpin>,
    ) -> impl Future<Output = Result<HttpResponse, NotImplemented>> + Send {
        let _ = (request, body);
        async { Err(NotImplemented) }
    }
}

/// Register both wire shapes of the outward HTTP call: the streaming one a current plugin uses,
/// and the buffered one an older plugin falls back to (client_rpc.go).
pub(super) fn register_api_http<T: PluginApiHttp>(
    server: &mut Server,
    implementation: &Arc<T>,
    broker: &MuxBroker,
) {
    let this = Arc::clone(implementation);
    let b = broker.clone();
    server.register(
        "Plugin.PluginHTTPStream",
        move |args: Z_PluginHTTPStreamArgs| {
            let (this, broker) = (Arc::clone(&this), b.clone());
            async move {
                // Go dials the response connection before calling, and fails the call if it cannot.
                let response_connection =
                    broker.dial(args.response_body_stream).await.map_err(|e| {
                        ServiceError(format!("can't connect to remote response body stream: {e}"))
                    })?;
                // Go reads an empty body when the plugin sent no stream.
                let body: Box<dyn AsyncRead + Send + Unpin> = match args.request_body_stream {
                    0 => Box::new(tokio::io::empty()),
                    id => Box::new(RemoteReader::new(broker.dial(id).await.map_err(|e| {
                        ServiceError(format!("can't connect to remote request body stream: {e}"))
                    })?)),
                };

                let response =
                    this.plugin_http(args.request, body)
                        .await
                        .map_err(|NotImplemented| {
                            ServiceError("API PluginHTTP called but not implemented".into())
                        })?;

                // The head answers the call; the body follows over its own connection, pushed
                // raw, as Go's `io.Copy` does.
                let returns = Z_PluginHTTPStreamReturns {
                    status_code: response.status_code,
                    header: response.header,
                };
                tokio::spawn(async move {
                    let mut body = response.body;
                    let mut connection = response_connection;
                    if let Err(e) = tokio::io::copy(&mut body, &mut connection).await {
                        tracing::error!(error = %e, "error streaming response body");
                    }
                    let _ = connection.shutdown().await;
                });
                Ok::<_, ServiceError>(returns)
            }
        },
    );

    let this = Arc::clone(implementation);
    server.register("Plugin.PluginHTTP", move |args: Z_PluginHTTPArgs| {
        let this = Arc::clone(&this);
        async move {
            // The buffered shape carries the request body inline.
            let body = Box::new(std::io::Cursor::new(args.request_body));
            let response =
                this.plugin_http(args.request, body)
                    .await
                    .map_err(|NotImplemented| {
                        ServiceError("API PluginHTTP called but not implemented".into())
                    })?;
            let mut response_body = Vec::new();
            let mut source = response.body;
            tokio::io::copy(&mut source, &mut response_body)
                .await
                .map_err(|e| ServiceError(format!("RPC call to PluginHTTP API failed: {e}")))?;
            Ok::<_, ServiceError>(Z_PluginHTTPReturns {
                response: Some(Box::new(crate::wire::http::Response {
                    status_code: response.status_code,
                    header: response.header,
                    ..crate::wire::http::Response::default()
                })),
                response_body,
            })
        }
    });
}

impl ApiClient {
    /// Go: `PluginHTTP(request *http.Request) *http.Response`, which the plugin uses to reach the
    /// server's own HTTP handlers.
    ///
    /// The streaming shape is tried first; a host too old to have it answers
    /// `rpc: can't find method Plugin.PluginHTTPStream`, and Go then falls back to the buffered
    /// one (client_rpc.go, `PluginHTTP`). `None` is Go's nil response.
    pub async fn plugin_http<B>(
        &self,
        request: Option<Box<HTTPRequestSubset>>,
        body: Option<B>,
    ) -> Option<RemoteHttpResponse>
    where
        B: AsyncRead + Send + Unpin + 'static,
    {
        let request_body_stream = match body {
            Some(body) => self.lend_reader(body, "PluginHTTPStream"),
            None => 0,
        };
        let response_body_stream = self.broker.next_id();
        let accepted = {
            let broker = self.broker.clone();
            tokio::spawn(async move { broker.accept(response_body_stream).await })
        };

        let args = Z_PluginHTTPStreamArgs {
            response_body_stream,
            request,
            request_body_stream,
        };
        let returns: Result<Z_PluginHTTPStreamReturns, _> =
            self.client.call("Plugin.PluginHTTPStream", &args).await;
        let returns = match returns {
            Ok(returns) => returns,
            Err(go_netrpc::Error::Server(message))
                if message == "rpc: can't find method Plugin.PluginHTTPStream" =>
            {
                tracing::debug!("the host has no PluginHTTPStream; using the buffered call");
                return self.plugin_http_buffered(args.request).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "RPC call to PluginHTTPStream API failed");
                return None;
            }
        };

        match accepted.await {
            Ok(Ok(body)) => Some(RemoteHttpResponse {
                status_code: returns.status_code,
                header: returns.header,
                body: Box::new(body),
            }),
            _ => {
                tracing::error!("Failed to get response body stream for PluginHTTPStream");
                None
            }
        }
    }

    /// The buffered shape: the request body crosses inline and the response arrives whole
    /// (client_rpc.go, `pluginHTTPBuffered`).
    async fn plugin_http_buffered(
        &self,
        request: Option<Box<HTTPRequestSubset>>,
    ) -> Option<RemoteHttpResponse> {
        let args = Z_PluginHTTPArgs {
            request,
            request_body: Vec::new(),
        };
        let returns: Z_PluginHTTPReturns = match self.client.call("Plugin.PluginHTTP", &args).await
        {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call to PluginHTTP API failed");
                return None;
            }
        };
        let response = returns.response?;
        Some(RemoteHttpResponse {
            status_code: response.status_code,
            header: response.header,
            body: Box::new(std::io::Cursor::new(returns.response_body)),
        })
    }
}
