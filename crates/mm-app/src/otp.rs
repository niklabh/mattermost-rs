//! Multi-factor authentication: port of `platform/shared/mfa` (mfa.go) and of the TOTP it runs,
//! `github.com/dgryski/dgoogauth@v0.0.0-20190221195224-5a805980a5f3` (googauth.go), as Mattermost
//! configures it — time-based, a window of three 30-second steps, and a replay list.
//!
//! # What a reader could get wrong
//!
//! - **The window is ±1 step**, not ±3: `WindowSize: 3` is the *width*, and `checkTotpCode` scans
//!   `t0 - 3/2 ..= t0 + 3/2`.
//! - **The replay list is always on.** `GetMfaUsedTimestamps` returns a non-nil slice even for a
//!   user with none, so `DisallowReuse` is never nil and every accepted step is remembered: the
//!   same code cannot be used twice within its window, and entries older than the window are
//!   dropped each time one is added. Sorted ascending.
//! - **Two kinds of refusal.** A token that is not six digits (or eight starting 1–9 — a scratch
//!   code, which Mattermost never issues, so always refused) is a *parse* error; a well-formed one
//!   that matches no step, or a step already used, is `InvalidToken`. The app layer turns them into
//!   different statuses, and on activation the parse error is a **500**.
//! - **The secret is base32 with padding.** 20 random bytes encode to 32 characters with none;
//!   `base32.StdEncoding.DecodeString` refuses lower case and a missing pad, and a secret it
//!   refuses makes `ComputeCode` return -1, which no token matches.
//! - **The QR code's URL is built by hand**, not with `url.Values`: the issuer is the site URL with
//!   `https://`, then `http://`, then `www.` trimmed and `url.QueryEscape`d, and the e-mail is
//!   interpolated raw.
//!
//! # The secret is the one value no oracle can fix
//!
//! `GenerateSecret` reads 20 bytes from `crypto/rand`. [`generate_secret_from`] takes the bytes,
//! so everything after them — the encoding, the URL, the QR code and the PNG — is compared with
//! Go's own output for the secret Go chose (`fixtures/behaviour_mfa.json`, `generate_secret`), and
//! [`generate_secret`] alone draws the randomness.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// `mfaSecretSize`: 160 bits, as RFC 4226 recommends.
pub const MFA_SECRET_SIZE: usize = 20;

/// `dgoogauth.OTPConfig.WindowSize` as `mfa.authenticate` sets it.
const WINDOW_SIZE: i64 = 3;

/// The two ways `mfa.authenticate` refuses a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// `dgoogauth.ErrInvalidCode`, wrapped: the token is not a six-digit (or scratch) code.
    #[error("unable to parse the token: invalid code")]
    Parse,
    /// `mfa.InvalidToken`: well formed, but no step in the window matches, or it was used.
    #[error("invalid mfa token")]
    Invalid,
}

/// The standard base32 alphabet.
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// `base32.StdEncoding.EncodeToString`.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let bits = buf.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let symbols = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for i in 0..8 {
            if i < symbols {
                out.push(BASE32[((bits >> (35 - 5 * i)) & 31) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `base32.StdEncoding.DecodeString`: `\r` and `\n` dropped first, then quanta of eight with the
/// padding rules of `Encoding.decode` — a missing pad, a pad before the second symbol of a
/// quantum, anything but pads after the first, and the lengths RFC 4648 forbids are errors.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let src: Vec<u8> = s.bytes().filter(|&b| b != b'\r' && b != b'\n').collect();
    let decode = |c: u8| BASE32.iter().position(|&a| a == c).map(|p| p as u8);
    let mut out = Vec::new();
    let mut i = 0;
    let mut end = false;
    while i < src.len() && !end {
        let mut dbuf = [0u8; 8];
        let mut dlen = 8;
        let mut j = 0;
        while j < 8 {
            if i == src.len() {
                // Out of input mid-quantum with padding expected.
                return None;
            }
            let c = src[i];
            i += 1;
            let remaining = src.len() - i;
            if c == b'=' && j >= 2 && remaining < 8 {
                if remaining + j < 7 {
                    return None;
                }
                for k in 0..(7 - j) {
                    if remaining > k && src[i + k] != b'=' {
                        return None;
                    }
                }
                dlen = j;
                end = true;
                if matches!(dlen, 1 | 3 | 6) {
                    return None;
                }
                break;
            }
            dbuf[j] = decode(c)?;
            j += 1;
        }
        let bytes = match dlen {
            8 => 5,
            7 => 4,
            5 => 3,
            4 => 2,
            2 => 1,
            _ => 0,
        };
        let packed = [
            dbuf[0] << 3 | dbuf[1] >> 2,
            dbuf[1] << 6 | dbuf[2] << 1 | dbuf[3] >> 4,
            dbuf[3] << 4 | dbuf[4] >> 1,
            dbuf[4] << 7 | dbuf[5] << 2 | dbuf[6] >> 3,
            dbuf[6] << 5 | dbuf[7],
        ];
        out.extend_from_slice(&packed[..bytes]);
    }
    Some(out)
}

/// Port of `dgoogauth.ComputeCode`: HOTP (RFC 4226) over the base32 secret and a counter — the
/// 30-second step for TOTP. `-1` when the secret does not decode.
pub fn compute_code(secret: &str, value: i64) -> i64 {
    let Some(key) = base32_decode(secret) else {
        return -1;
    };
    let Ok(mut mac) = <Hmac<Sha1> as Mac>::new_from_slice(&key) else {
        return -1;
    };
    mac.update(&value.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let offset = (h[19] & 0x0f) as usize;
    let truncated = u32::from_be_bytes([h[offset], h[offset + 1], h[offset + 2], h[offset + 3]]);
    i64::from((truncated & 0x7fff_ffff) % 1_000_000)
}

/// Port of `mfa.authenticate` over `OTPConfig.Authenticate`, at the 30-second step `t0`: the
/// token trimmed (`strings.TrimSpace`), checked for shape, then matched against steps
/// `t0 - 1 ..= t0 + 1`. On success, the replay list to store: the matched step added, sorted,
/// and every step before the window dropped.
pub fn authenticate(
    secret: &str,
    used: &[i64],
    token: &str,
    t0: i64,
) -> Result<Vec<i64>, TokenError> {
    let password = token.trim();
    let bytes = password.as_bytes();
    let scratch = match bytes.len() {
        6 if bytes[0].is_ascii_digit() => false,
        8 if (b'1'..=b'9').contains(&bytes[0]) => true,
        _ => return Err(TokenError::Parse),
    };
    // `strconv.Atoi`: the first byte is a digit, so no sign is possible; any other non-digit fails.
    if !bytes.iter().all(u8::is_ascii_digit) {
        return Err(TokenError::Parse);
    }
    let code: i64 = password.parse().map_err(|_| TokenError::Parse)?;
    if scratch {
        // `checkScratchCodes` over Mattermost's empty list.
        return Err(TokenError::Invalid);
    }

    let min_t = t0 - WINDOW_SIZE / 2;
    let max_t = t0 + WINDOW_SIZE / 2;
    for t in min_t..=max_t {
        if compute_code(secret, t) == code {
            if used.contains(&t) {
                return Err(TokenError::Invalid);
            }
            let mut reuse = used.to_vec();
            reuse.push(t);
            reuse.sort_unstable();
            reuse.retain(|&ts| ts >= min_t);
            return Ok(reuse);
        }
    }
    Err(TokenError::Invalid)
}

/// `getIssuerFromURL`: the site URL trimmed of whitespace, then of one `https://`, one `http://`
/// and one `www.`, then `url.QueryEscape`d; `Mattermost` when empty.
pub fn issuer_from_url(uri: &str) -> String {
    let site_url = uri.trim();
    let issuer = if site_url.is_empty() {
        "Mattermost"
    } else {
        let s = site_url.strip_prefix("https://").unwrap_or(site_url);
        let s = s.strip_prefix("http://").unwrap_or(s);
        s.strip_prefix("www.").unwrap_or(s)
    };
    mm_model::go_url::query_escape(issuer)
}

/// The `otpauth://` URL `GenerateSecret` encodes: issuer, raw e-mail, secret, issuer again.
pub fn auth_link(site_url: &str, email: &str, secret: &str) -> String {
    let issuer = issuer_from_url(site_url);
    format!("otpauth://totp/{issuer}:{email}?secret={secret}&issuer={issuer}")
}

/// What `mfa.GenerateSecret` returns: the secret and the QR code's PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSecret {
    pub secret: String,
    pub png: Vec<u8>,
}

/// Port of `mfa.GenerateSecret` from its random bytes on: base32, the link, `qr.Encode(link,
/// qr.H).PNG()`. The caller stores the secret.
pub fn generate_secret_from(
    random: &[u8; MFA_SECRET_SIZE],
    site_url: &str,
    email: &str,
) -> Result<GeneratedSecret, goqr::QrError> {
    let secret = base32_encode(random);
    let png = goqr::encode(&auth_link(site_url, email, &secret), goqr::Level::H)?.png();
    Ok(GeneratedSecret { secret, png })
}

/// [`generate_secret_from`] over 20 bytes from the operating system's generator, as
/// `crypto/rand.Read`.
pub fn generate_secret(site_url: &str, email: &str) -> Result<GeneratedSecret, goqr::QrError> {
    let mut random = [0u8; MFA_SECRET_SIZE];
    rand::Rng::fill(&mut rand::rng(), &mut random);
    generate_secret_from(&random, site_url, email)
}

/// `model.MfaSecret.QRCode`: the PNG, standard base64 with padding.
pub fn qr_code_base64(png: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// The current 30-second step, `time.Now().Unix() / 30`.
pub fn current_step() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX) / 30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trips_with_gos_padding() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"abcde", b"abcdef"] {
            let encoded = base32_encode(data);
            assert_eq!(base32_decode(&encoded).as_deref(), Some(data), "{encoded}");
        }
        assert_eq!(base32_encode(b"abc"), "MFRGG===");
        assert_eq!(base32_decode("mfrgg==="), None, "lower case");
        assert_eq!(base32_decode("MFRGG"), None, "missing padding");
        assert_eq!(
            base32_decode("MFRG\nG==="),
            Some(b"abc".to_vec()),
            "newlines dropped"
        );
        assert_eq!(base32_decode("M======="), None, "one symbol is no byte");
    }

    /// RFC 6238's SHA-1 vectors, truncated to six digits.
    #[test]
    fn rfc_6238_vectors() {
        let secret = base32_encode(b"12345678901234567890");
        for (time, code) in [
            (59, 287_082),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
        ] {
            assert_eq!(compute_code(&secret, time / 30), code, "{time}");
        }
    }

    #[test]
    fn the_issuer_is_the_bare_host() {
        assert_eq!(issuer_from_url(""), "Mattermost");
        assert_eq!(issuer_from_url("https://www.example.com"), "example.com");
        assert_eq!(issuer_from_url("http://localhost:8065"), "localhost%3A8065");
        assert_eq!(issuer_from_url("  https://a.b/c d  "), "a.b%2Fc+d");
    }

    /// `fixtures/behaviour_mfa.json`, from Go's `mfa` and `dgoogauth` packages.
    mod go_parity {
        use super::*;

        fn oracle() -> serde_json::Value {
            serde_json::from_str(include_str!("../../../fixtures/behaviour_mfa.json"))
                .expect("behaviour_mfa.json is generated by reference/dump")
        }

        #[test]
        fn every_code_is_dgoogauths() {
            let oracle = oracle();
            let rows = oracle["compute_code"].as_array().expect("rows");
            assert!(rows.len() >= 80);
            for row in rows {
                let secret = row["secret"].as_str().unwrap();
                let value = row["value"].as_i64().unwrap();
                assert_eq!(
                    compute_code(secret, value),
                    row["code"].as_i64().unwrap(),
                    "{secret} {value}"
                );
            }
        }

        /// Go chose the secret; the QR code, the link inside it and the store write must follow
        /// from it exactly.
        #[test]
        fn every_generated_secret_renders_gos_png() {
            let oracle = oracle();
            let b64 = base64::engine::general_purpose::STANDARD;
            for row in oracle["generate_secret"].as_array().expect("rows") {
                let secret = row["secret"].as_str().unwrap();
                let random: [u8; MFA_SECRET_SIZE] = base32_decode(secret)
                    .expect("Go's secret decodes")
                    .try_into()
                    .expect("20 bytes");
                let got = generate_secret_from(
                    &random,
                    row["site_url"].as_str().unwrap(),
                    row["email"].as_str().unwrap(),
                )
                .unwrap();
                assert_eq!(got.secret, secret);
                assert!(
                    got.png == b64.decode(row["png_b64"].as_str().unwrap()).unwrap(),
                    "{row}"
                );
                let calls = row["calls"].as_array().unwrap();
                assert_eq!(calls.len(), 1, "one write: {calls:?}");
                assert_eq!(calls[0]["call"], "UpdateMfaSecret");
                assert_eq!(calls[0]["secret"], secret);
            }
        }

        fn token_of(row: &serde_json::Value, t0: i64) -> String {
            match row["token_offset"].as_i64() {
                Some(offset) => {
                    let pad = row["token_pad"].as_str().unwrap();
                    let code = compute_code("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP", t0 + offset);
                    format!("{pad}{code:06}{pad}")
                }
                None => row["token_raw"].as_str().unwrap().to_owned(),
            }
        }

        fn used_of(row: &serde_json::Value, t0: i64) -> Vec<i64> {
            row["used"]
                .as_array()
                .unwrap()
                .iter()
                .map(|u| u.as_i64().unwrap() + t0)
                .collect()
        }

        /// `ValidateToken`: `(ok, err)` and the replay list it stores, relative to `t0`. Replayed
        /// at an arbitrary step: only offsets were recorded.
        #[test]
        fn every_validation_is_gos() {
            let oracle = oracle();
            let t0 = 58_000_123;
            for row in oracle["validate"].as_array().expect("rows") {
                let name = row["name"].as_str().unwrap();
                let got = authenticate(
                    row["secret"].as_str().unwrap(),
                    &used_of(row, t0),
                    &token_of(row, t0),
                    t0,
                );
                let calls = row["calls"].as_array().cloned().unwrap_or_default();
                match got {
                    Ok(stored) => {
                        assert!(row["ok"].as_bool().unwrap(), "{name}: Go refused");
                        assert!(!row["error"].as_bool().unwrap(), "{name}");
                        let want: Vec<i64> = calls[0]["ts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|t| t.as_i64().unwrap() + t0)
                            .collect();
                        assert_eq!(stored, want, "{name}");
                    }
                    Err(TokenError::Invalid) => {
                        assert!(!row["ok"].as_bool().unwrap(), "{name}: Go accepted");
                        assert!(!row["error"].as_bool().unwrap(), "{name}: Go errored");
                        assert!(calls.is_empty(), "{name}");
                    }
                    Err(TokenError::Parse) => {
                        assert!(row["error"].as_bool().unwrap(), "{name}: Go did not error");
                        assert!(calls.is_empty(), "{name}");
                    }
                }
            }
        }

        /// `Activate`: the same decision, with `InvalidToken` told apart from the parse error.
        #[test]
        fn every_activation_is_gos() {
            let oracle = oracle();
            let t0 = 58_000_123;
            for row in oracle["activate"].as_array().expect("rows") {
                let name = row["name"].as_str().unwrap();
                let got = authenticate(
                    row["secret"].as_str().unwrap(),
                    &used_of(row, t0),
                    &token_of(row, t0),
                    t0,
                );
                assert_eq!(got.is_err(), row["error"].as_bool().unwrap(), "{name}");
                assert_eq!(
                    got == Err(TokenError::Invalid),
                    row["invalid_token"].as_bool().unwrap(),
                    "{name}"
                );
                let calls = row["calls"].as_array().cloned().unwrap_or_default();
                if got.is_ok() {
                    assert_eq!(calls[0]["call"], "UpdateMfaActive", "{name}");
                    assert_eq!(calls[1]["call"], "StoreMfaUsedTimestamps", "{name}");
                } else {
                    assert!(calls.is_empty(), "{name}");
                }
            }
        }
    }
}
