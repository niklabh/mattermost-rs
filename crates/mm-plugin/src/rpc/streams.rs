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
use tokio::io::AsyncRead;

use super::{ApiClient, NotImplemented};
use crate::io_rpc::{RemoteReader, serve_reader};
use crate::wire::model::{FileInfo, UploadSession};
use crate::wire::plugin::{
    Z_InstallPluginArgs, Z_InstallPluginReturns, Z_ReceiveSharedChannelAttachmentSyncMsgArgs,
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
