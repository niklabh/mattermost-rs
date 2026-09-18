//! net/rpc/client.go.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use gobwire::{Decode, Decoder, Encode, Encoder};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::oneshot;

use crate::error::Error;
use crate::frame::read_value;
use crate::messages::{Request, Response};

/// What the reader task hands a pending call.
enum Outcome<'a> {
    /// The reply is buffered in the decoder.
    Reply(&'a mut Decoder),
    Failed(Error),
}

/// Completes one pending call. Returns an error only when decoding the reply failed, which ends
/// the connection.
type Completer = Box<dyn FnOnce(Outcome<'_>) -> Result<(), gobwire::Error> + Send>;

struct State {
    pending: HashMap<u64, Completer>,
    /// The user called `close`.
    closing: bool,
    /// The reader stopped; no call can succeed any more.
    shutdown: bool,
}

struct Writer {
    io: Box<dyn AsyncWrite + Send + Unpin>,
    enc: Encoder,
    seq: u64,
}

struct Shared {
    /// Held across sequence assignment, encoding and the write, like Go's `reqMutex`: the gob
    /// stream's type state and the byte order on the wire must agree.
    writer: tokio::sync::Mutex<Writer>,
    state: Mutex<State>,
}

/// A net/rpc client over one connection. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

impl Client {
    /// Start a client on a connected stream. Spawns the reply reader on the current tokio
    /// runtime.
    pub fn new<T>(io: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write): (ReadHalf<T>, WriteHalf<T>) = tokio::io::split(io);
        let shared = Arc::new(Shared {
            writer: tokio::sync::Mutex::new(Writer {
                io: Box::new(write),
                enc: Encoder::new(),
                seq: 0,
            }),
            state: Mutex::new(State {
                pending: HashMap::new(),
                closing: false,
                shutdown: false,
            }),
        });
        tokio::spawn(input(shared.clone(), BufReader::new(read)));
        Self { shared }
    }

    /// Call `service_method` and decode the reply into a fresh `R`.
    pub async fn call<A, R>(&self, service_method: &str, args: &A) -> Result<R, Error>
    where
        A: Encode + ?Sized,
        R: Decode + Default + Send + 'static,
    {
        self.call_into(service_method, args, R::default()).await
    }

    /// Call `service_method` and decode the reply **into** `reply`, merging as gob does: fields
    /// the reply omits keep the values `reply` already had.
    pub async fn call_into<A, R>(
        &self,
        service_method: &str,
        args: &A,
        reply: R,
    ) -> Result<R, Error>
    where
        A: Encode + ?Sized,
        R: Decode + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, Error>>();
        let completer: Completer = Box::new(move |outcome| match outcome {
            Outcome::Reply(dec) => {
                let mut reply = reply;
                match dec.decode_into(&mut reply) {
                    Ok(()) => {
                        let _ = tx.send(Ok(reply));
                        Ok(())
                    }
                    Err(e) => {
                        let e = Arc::new(e);
                        let _ = tx.send(Err(Error::ReadingBody(e.clone())));
                        Err(gobwire::Error::Corrupt(e.to_string()))
                    }
                }
            }
            Outcome::Failed(err) => {
                let _ = tx.send(Err(err));
                Ok(())
            }
        });

        let mut w = self.shared.writer.lock().await;
        let seq = {
            let mut st = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if st.shutdown || st.closing {
                return Err(Error::Shutdown);
            }
            let seq = w.seq;
            w.seq += 1;
            st.pending.insert(seq, completer);
            seq
        };

        // Encode the argument first: if it cannot be encoded, nothing — not even the header's type
        // definition — has entered the stream's type state or the wire.
        let mut body = Vec::new();
        let mut header = Vec::new();
        let encoded = w.enc.encode_into(args, &mut body).and_then(|()| {
            w.enc.encode_into(
                &Request {
                    service_method: service_method.to_owned(),
                    seq,
                },
                &mut header,
            )
        });
        let written = match encoded {
            Err(e) => Err(Error::from(e)),
            Ok(()) => {
                header.extend_from_slice(&body);
                match w.io.write_all(&header).await {
                    Ok(()) => w.io.flush().await.map_err(Error::from),
                    Err(e) => Err(e.into()),
                }
            }
        };
        drop(w);
        if let Err(e) = written {
            self.shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .remove(&seq);
            return Err(e);
        }
        rx.await.unwrap_or(Err(Error::Shutdown))
    }

    /// Close the connection. Pending calls fail with [`Error::Shutdown`]; closing twice returns
    /// it too.
    pub async fn close(&self) -> Result<(), Error> {
        {
            let mut st = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if st.closing {
                return Err(Error::Shutdown);
            }
            st.closing = true;
        }
        let mut w = self.shared.writer.lock().await;
        w.io.shutdown().await?;
        Ok(())
    }
}

/// client.go, `input`: read replies until the connection fails, then fail every pending call.
async fn input<R: AsyncRead + Unpin>(shared: Arc<Shared>, mut r: R) {
    let mut dec = Decoder::new();
    let mut buf = Vec::new();
    let err = loop {
        match read_value(&mut r, &mut dec, &mut buf).await {
            Ok(true) => {}
            Ok(false) => break None,
            Err(e) => break Some(e),
        }
        let header: Response = match dec.decode() {
            Ok(h) => h,
            Err(e) => break Some(e.into()),
        };
        match read_value(&mut r, &mut dec, &mut buf).await {
            Ok(true) => {}
            Ok(false) => break None,
            Err(e) => break Some(e),
        }
        let call = shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .remove(&header.seq);
        match call {
            None => {
                if let Err(e) = dec.discard() {
                    break Some(Error::Gob(Arc::new(gobwire::Error::Corrupt(format!(
                        "reading error body: {e}"
                    )))));
                }
            }
            Some(call) if !header.error.is_empty() => {
                let discarded = dec.discard();
                let _ = call(Outcome::Failed(Error::Server(header.error)));
                if let Err(e) = discarded {
                    break Some(Error::Gob(Arc::new(gobwire::Error::Corrupt(format!(
                        "reading error body: {e}"
                    )))));
                }
            }
            Some(call) => {
                if let Err(e) = call(Outcome::Reply(&mut dec)) {
                    break Some(Error::Gob(Arc::new(e)));
                }
            }
        }
    };
    let mut st = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
    st.shutdown = true;
    let err = match err {
        // io.EOF: expected if the user closed, unexpected otherwise.
        None if st.closing => Error::Shutdown,
        None => Error::UnexpectedEof,
        Some(e) => e,
    };
    if !st.closing {
        tracing::debug!(error = %err, "net/rpc client connection ended");
    }
    for (_, call) in st.pending.drain() {
        let _ = call(Outcome::Failed(err.clone()));
    }
}
