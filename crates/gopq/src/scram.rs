//! SCRAM-SHA-256 without channel binding (RFC 5802, RFC 7677), as lib/pq's `scram` package runs
//! it: `n,,` GS2 header, a random nonce, and the server signature checked at the end.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::Error;

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key)
        .map_err(|_| Error::msg("pq: SCRAM-SHA-256 error: bad key"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// One exchange.
pub(crate) struct Scram {
    password: String,
    client_nonce: String,
    client_first_bare: String,
    server_signature: Vec<u8>,
}

impl Scram {
    pub(crate) fn new(password: &str) -> Self {
        let mut raw = [0u8; 18];
        rand::Rng::fill(&mut rand::rng(), &mut raw);
        let client_nonce = base64::engine::general_purpose::STANDARD.encode(raw);
        Scram {
            password: password.to_owned(),
            client_first_bare: format!("n=,r={client_nonce}"),
            client_nonce,
            server_signature: Vec::new(),
        }
    }

    /// The client-first message.
    pub(crate) fn first(&self) -> Vec<u8> {
        format!("n,,{}", self.client_first_bare).into_bytes()
    }

    /// The client-final message for the server-first message.
    pub(crate) fn step(&mut self, server_first: &[u8]) -> Result<Vec<u8>, Error> {
        let bad = |why: &str| Error::msg(format!("pq: SCRAM-SHA-256 error: {why}"));
        let text =
            std::str::from_utf8(server_first).map_err(|_| bad("server-first is not UTF-8"))?;
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for part in text.split(',') {
            if let Some(v) = part.strip_prefix("r=") {
                nonce = Some(v);
            } else if let Some(v) = part.strip_prefix("s=") {
                salt = Some(
                    base64::engine::general_purpose::STANDARD
                        .decode(v)
                        .map_err(|_| bad("bad salt"))?,
                );
            } else if let Some(v) = part.strip_prefix("i=") {
                iterations = Some(v.parse::<u32>().map_err(|_| bad("bad iteration count"))?);
            }
        }
        let (nonce, salt, iterations) = match (nonce, salt, iterations) {
            (Some(n), Some(s), Some(i)) => (n, s, i),
            _ => return Err(bad("incomplete server-first message")),
        };
        if !nonce.starts_with(&self.client_nonce) {
            return Err(bad(
                "server SCRAM-SHA-256 nonce is not prefixed by client nonce",
            ));
        }
        let mut salted = [0u8; 32];
        pbkdf2::pbkdf2::<HmacSha256>(self.password.as_bytes(), &salt, iterations, &mut salted)
            .map_err(|_| bad("pbkdf2"))?;
        let client_key = hmac(&salted, b"Client Key")?;
        let stored_key = Sha256::digest(&client_key);
        let without_proof = format!("c=biws,r={nonce}");
        let auth_message = format!("{},{text},{without_proof}", self.client_first_bare);
        let client_signature = hmac(&stored_key, auth_message.as_bytes())?;
        let proof: Vec<u8> = client_key
            .iter()
            .zip(&client_signature)
            .map(|(a, b)| a ^ b)
            .collect();
        let server_key = hmac(&salted, b"Server Key")?;
        self.server_signature = hmac(&server_key, auth_message.as_bytes())?;
        Ok(format!(
            "{without_proof},p={}",
            base64::engine::general_purpose::STANDARD.encode(proof)
        )
        .into_bytes())
    }

    /// Check the server-final message.
    pub(crate) fn finish(&self, server_final: &[u8]) -> Result<(), Error> {
        let text = std::str::from_utf8(server_final).unwrap_or("");
        if let Some(e) = text.strip_prefix("e=") {
            return Err(Error::msg(format!(
                "pq: SCRAM-SHA-256 error: server error: {e}"
            )));
        }
        let expected = format!(
            "v={}",
            base64::engine::general_purpose::STANDARD.encode(&self.server_signature)
        );
        if text.split(',').next() != Some(expected.as_str()) {
            return Err(Error::msg(
                "pq: SCRAM-SHA-256 error: server signature does not match",
            ));
        }
        Ok(())
    }
}
