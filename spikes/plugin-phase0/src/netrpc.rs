//! Spike-grade Go net/rpc client over one stream with the gob codec (net/rpc/client.go,
//! gobClientCodec): each call writes `Request{ServiceMethod, Seq}` then the args; each reply is
//! `Response{ServiceMethod, Seq, Error}` then the reply body. Calls are sequential here.

use anyhow::{Result, bail, ensure};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde_json::Value as Json;

use crate::gob::{Decoder, Encoder, Item, Ty, Val};

pub struct Client<S> {
    io: S,
    enc: Encoder,
    dec: Decoder,
    seq: u64,
}

fn request_ty() -> Ty {
    Ty::Struct(
        "Request",
        vec![("ServiceMethod", Ty::String), ("Seq", Ty::Uint)],
    )
}

/// Read one delimited gob message.
pub async fn read_message<S: AsyncRead + Unpin>(io: &mut S) -> Result<Vec<u8>> {
    let mut first = [0u8; 1];
    io.read_exact(&mut first).await?;
    let n = if first[0] < 0x80 {
        u64::from(first[0])
    } else {
        let k = (!first[0]).wrapping_add(1) as usize;
        ensure!(k <= 8, "bad count");
        let mut b = vec![0u8; k];
        io.read_exact(&mut b).await?;
        b.iter().fold(0u64, |a, &x| a << 8 | u64::from(x))
    };
    let mut msg = vec![0u8; usize::try_from(n)?];
    io.read_exact(&mut msg).await?;
    Ok(msg)
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    pub fn new(io: S) -> Self {
        Self {
            io,
            enc: Encoder::default(),
            dec: Decoder::default(),
            seq: 0,
        }
    }

    /// Next value, reading further messages when one spans several (see `Decoder::message`).
    async fn next_value(&mut self) -> Result<Json> {
        let (mut pending, mut ends) = (Vec::new(), Vec::new());
        loop {
            let msg = read_message(&mut self.io).await?;
            let item = if pending.is_empty() {
                self.dec.message(&msg)?
            } else {
                pending.extend_from_slice(&msg);
                ends.push(pending.len());
                self.dec.message_continued(&pending, &ends)?
            };
            match item {
                Item::Value(_, v) => return Ok(v),
                Item::TypeDefined(_) => {}
                Item::NeedMore => {
                    if pending.is_empty() {
                        pending.extend_from_slice(&msg);
                        ends.push(pending.len());
                    }
                    continue;
                }
            }
            pending.clear();
            ends.clear();
        }
    }

    /// Returns `Err` for a transport failure and `Ok(Err(text))` for Go's `ServerError`.
    pub async fn call(
        &mut self,
        method: &str,
        args_ty: &Ty,
        args: &Val,
    ) -> Result<std::result::Result<Json, String>> {
        let seq = self.seq;
        self.seq += 1;
        let mut out = self.enc.encode(
            &request_ty(),
            &Val::Struct(vec![Some(Val::String(method.into())), Some(Val::Uint(seq))]),
        );
        out.extend(self.enc.encode(args_ty, args));
        self.io.write_all(&out).await?;
        self.io.flush().await?;
        let header = self.next_value().await?;
        let body = self.next_value().await?;
        let got_seq = header.get("Seq").and_then(Json::as_u64).unwrap_or(0);
        if got_seq != seq {
            bail!("net/rpc: reply seq {got_seq}, want {seq}");
        }
        match header.get("Error").and_then(Json::as_str) {
            Some(e) if !e.is_empty() => Ok(Err(e.to_string())),
            _ => Ok(Ok(body)),
        }
    }
}
