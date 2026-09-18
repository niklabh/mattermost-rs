//! `FileWillBeUploaded` (client_rpc.go), the hook that lends a reader and a writer at once.
//!
//! The host lends the uploaded file as a reader ([`crate::io_rpc`]) and a second connection for
//! the replacement, which the plugin writes to raw — Go copies it straight into the host's
//! `io.Writer`. The host waits for that copy to finish before the call returns, so a plugin that
//! rewrites a file is done by the time the hook answers.

use std::future::Future;
use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use goplugin::MuxBroker;
use goplugin::yamux::Stream;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::{HooksClient, NotImplemented, hook_id};
use crate::io_rpc::{RemoteReader, serve_reader};
use crate::wire::model::FileInfo;
use crate::wire::plugin::{Context, Z_FileWillBeUploadedArgs, Z_FileWillBeUploadedReturns};

/// The hook that rewrites an uploaded file, which the generated [`super::Hooks`] cannot describe.
pub trait HooksFileUpload: Send + Sync + 'static {
    /// Go: `FileWillBeUploaded(c *Context, info *model.FileInfo, file io.Reader, output
    /// io.Writer) (*model.FileInfo, string)`.
    ///
    /// Writing nothing to `output` leaves the file as it was; anything written replaces it.
    fn file_will_be_uploaded(
        &self,
        context: Option<Box<Context>>,
        info: Option<Box<FileInfo>>,
        file: RemoteReader,
        output: Stream,
    ) -> impl Future<Output = Result<Z_FileWillBeUploadedReturns, NotImplemented>> + Send {
        let _ = (context, info, file, output);
        async { Err(NotImplemented) }
    }
}

/// Register the hook on a plugin's server.
pub(super) fn register_hooks_file_upload<H: HooksFileUpload>(
    server: &mut Server,
    hooks: &Arc<H>,
    broker: &MuxBroker,
) {
    let this = Arc::clone(hooks);
    let b = broker.clone();
    server.register(
        "Plugin.FileWillBeUploaded",
        move |args: Z_FileWillBeUploadedArgs| {
            let (this, broker) = (Arc::clone(&this), b.clone());
            async move {
                let file = broker
                    .dial(args.uploaded_file_stream)
                    .await
                    .map_err(|e| ServiceError(e.to_string()))?;
                let output = broker
                    .dial(args.replacement_file_stream)
                    .await
                    .map_err(|e| ServiceError(e.to_string()))?;

                let returns = this
                    .file_will_be_uploaded(args.a, args.b, RemoteReader::new(file), output)
                    .await
                    .map_err(|NotImplemented| {
                        // Go's message here is not wrapped in encodableError, unlike its
                        // neighbours (client_rpc.go).
                        ServiceError("hook FileWillBeUploaded called but not implemented".into())
                    })?;
                Ok::<_, ServiceError>(returns)
            }
        },
    );
}

impl HooksClient {
    /// Go: `FileWillBeUploaded(c *Context, info *model.FileInfo, file io.Reader, output
    /// io.Writer) (*model.FileInfo, string)`.
    ///
    /// The file info the caller passes is the default answer, as it is for the other hooks that
    /// may rewrite their argument — but this one does not decode into it, so a plugin that
    /// answers replaces it outright (client_rpc.go).
    pub async fn file_will_be_uploaded<R, W>(
        &self,
        context: Option<Box<Context>>,
        info: Option<Box<FileInfo>>,
        file: R,
        output: W,
    ) -> Z_FileWillBeUploadedReturns
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let default = Z_FileWillBeUploadedReturns {
            a: info.clone(),
            b: String::new(),
        };
        if !self.implements(hook_id::FILE_WILL_BE_UPLOADED) {
            return default;
        }

        let uploaded_file_stream = self.broker.next_id();
        let broker = self.broker.clone();
        tokio::spawn(async move {
            match broker.accept(uploaded_file_stream).await {
                Ok(conn) => {
                    if let Err(e) = serve_reader(file, conn).await {
                        tracing::debug!(error = %e, "FileWillBeUploaded: serving the file ended");
                    }
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "Plugin failed to serve upload file stream. MuxBroker could not Accept connection"
                ),
            }
        });

        // The replacement arrives raw; Go copies it into the caller's writer and waits for that
        // copy before answering, so the file is complete when the hook returns.
        let replacement_file_stream = self.broker.next_id();
        let broker = self.broker.clone();
        let replacement = tokio::spawn(async move {
            let mut output = output;
            match broker.accept(replacement_file_stream).await {
                Ok(mut conn) => {
                    if let Err(e) = tokio::io::copy(&mut conn, &mut output).await {
                        tracing::error!(error = %e, "Error reading replacement file.");
                    }
                    let _ = output.flush().await;
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "Plugin failed to serve replacement file stream. MuxBroker could not Accept connection"
                ),
            }
        });

        let args = Z_FileWillBeUploadedArgs {
            a: context,
            b: info,
            uploaded_file_stream,
            replacement_file_stream,
        };
        let returns = match self.rpc("FileWillBeUploaded", &args).await {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call FileWillBeUploaded to plugin failed.");
                default
            }
        };
        // Ensure the copy from the replacement connection above completes.
        let _ = replacement.await;
        returns
    }
}
