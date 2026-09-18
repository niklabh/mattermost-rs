//! net/rpc/server.go.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gobwire::{Decode, Decoder, Encode, Encoder};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;

use crate::error::Error;
use crate::frame::read_value;
use crate::messages::{InvalidRequest, Request, Response};

/// The error a method returns: its text becomes the reply's `Error` (Go's `err.Error()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceError(pub String);

impl<E: std::fmt::Display> From<E> for ServiceError {
    fn from(e: E) -> Self {
        ServiceError(e.to_string())
    }
}

type Reply = Box<dyn Encode + Send + Sync>;
type ReplyFuture = Pin<Box<dyn Future<Output = Result<Reply, ServiceError>> + Send>>;

/// A registered method, type-erased: decode the argument from the stream (which has to happen
/// in order, on the reading task), then produce the reply asynchronously.
trait Method: Send + Sync {
    fn start(&self, dec: &mut Decoder) -> Result<ReplyFuture, gobwire::Error>;
}

struct FnMethod<A, F> {
    f: F,
    _arg: std::marker::PhantomData<fn() -> A>,
}

impl<A, R, F, Fut> Method for FnMethod<A, F>
where
    A: Decode + Default + Send + 'static,
    R: Encode + Send + Sync + 'static,
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ServiceError>> + Send + 'static,
{
    fn start(&self, dec: &mut Decoder) -> Result<ReplyFuture, gobwire::Error> {
        // server.go, readRequest: a fresh argument per call.
        let arg: A = dec.decode()?;
        let fut = (self.f)(arg);
        Ok(Box::pin(
            async move { fut.await.map(|r| Box::new(r) as Reply) },
        ))
    }
}

/// A set of methods, served on any number of connections.
#[derive(Default)]
pub struct Server {
    services: HashMap<String, HashMap<String, Arc<dyn Method>>>,
}

impl Server {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `f` as `"Service.Method"`. Go derives these names from exported methods of a
    /// registered receiver; here they are explicit.
    ///
    /// # Panics
    ///
    /// If `service_method` has no `.`, or is already registered — both programming errors at
    /// setup, like Go's `Register` returning an error.
    pub fn register<A, R, F, Fut>(&mut self, service_method: &str, f: F) -> &mut Self
    where
        A: Decode + Default + Send + 'static,
        R: Encode + Send + Sync + 'static,
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R, ServiceError>> + Send + 'static,
    {
        #[allow(clippy::expect_used)]
        let dot = service_method
            .rfind('.')
            .expect("service method must be \"Service.Method\"");
        let (service, method) = (&service_method[..dot], &service_method[dot + 1..]);
        let previous = self.services.entry(service.to_owned()).or_default().insert(
            method.to_owned(),
            Arc::new(FnMethod {
                f,
                _arg: std::marker::PhantomData,
            }),
        );
        assert!(previous.is_none(), "{service_method} registered twice");
        self
    }

    /// server.go, `readRequestHeader`: the method, or the exact error Go answers with.
    fn lookup(&self, service_method: &str) -> Result<Arc<dyn Method>, String> {
        let Some(dot) = service_method.rfind('.') else {
            return Err(format!(
                "rpc: service/method request ill-formed: {service_method}"
            ));
        };
        let Some(service) = self.services.get(&service_method[..dot]) else {
            return Err(format!("rpc: can't find service {service_method}"));
        };
        service
            .get(&service_method[dot + 1..])
            .cloned()
            .ok_or_else(|| format!("rpc: can't find method {service_method}"))
    }

    /// Serve one connection until it closes (server.go, `ServeCodec`): calls run concurrently,
    /// and when the input ends the server waits for them before closing its side.
    pub async fn serve<T>(self: Arc<Self>, io: T) -> Result<(), Error>
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write) = tokio::io::split(io);
        let write: BoxWrite = Box::new(write);
        let writer = Arc::new(tokio::sync::Mutex::new((write, Encoder::new())));
        let mut r = BufReader::new(read);
        let mut dec = Decoder::new();
        let mut buf = Vec::new();
        let mut calls = JoinSet::new();
        let result = loop {
            match read_value(&mut r, &mut dec, &mut buf).await {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
            // A header that does not decode ends the connection (keepReading = false).
            let req: Request = match dec.decode() {
                Ok(req) => req,
                Err(e) => break Err(e.into()),
            };
            match read_value(&mut r, &mut dec, &mut buf).await {
                Ok(true) => {}
                Ok(false) => break Err(Error::UnexpectedEof),
                Err(e) => break Err(e),
            }
            let started = match self.lookup(&req.service_method) {
                Err(msg) => {
                    dec.discard()?;
                    Err(msg)
                }
                // An argument that does not decode fails this call only (keepReading = true).
                Ok(method) => method.start(&mut dec).map_err(|e| e.to_string()),
            };
            let writer = writer.clone();
            match started {
                Err(msg) => {
                    if let Err(e) = respond(&writer, req, Err(ServiceError(msg))).await {
                        break Err(e);
                    }
                }
                Ok(fut) => {
                    calls.spawn(async move {
                        let reply = fut.await;
                        if let Err(e) = respond(&writer, req, reply).await {
                            tracing::debug!(error = %e, "net/rpc: writing response");
                        }
                    });
                }
            }
        };
        while calls.join_next().await.is_some() {}
        let mut w = writer.lock().await;
        let _ = w.0.shutdown().await;
        result
    }
}

/// server.go, `sendResponse`: the header, then the reply or `invalidRequest`.
type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

async fn respond(
    writer: &tokio::sync::Mutex<(BoxWrite, Encoder)>,
    req: Request,
    reply: Result<Reply, ServiceError>,
) -> Result<(), Error> {
    let mut w = writer.lock().await;
    let (header_error, body): (String, Reply) = match reply {
        Ok(r) => (String::new(), r),
        Err(ServiceError(msg)) => (msg, Box::new(InvalidRequest {})),
    };
    let mut body_bytes = Vec::new();
    // Go logs and closes the connection when a reply will not encode; so does this.
    if let Err(e) = w.1.encode_into(&*body, &mut body_bytes) {
        tracing::warn!(error = %e, "net/rpc: gob error encoding body");
        let _ = w.0.shutdown().await;
        return Err(e.into());
    }
    let mut out = Vec::new();
    w.1.encode_into(
        &Response {
            service_method: req.service_method,
            seq: req.seq,
            error: header_error,
        },
        &mut out,
    )?;
    out.extend_from_slice(&body_bytes);
    w.0.write_all(&out).await?;
    w.0.flush().await?;
    Ok(())
}
