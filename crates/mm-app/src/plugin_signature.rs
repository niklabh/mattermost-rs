//! Port of `app/plugin_signature.go`: whether a plugin bundle carries a detached OpenPGP signature
//! from a key this server trusts.
//!
//! # The keys, in Go's order
//!
//! `verifyPlugin` (plugin_signature.go:76) tries the compiled-in Mattermost key, then — while
//! `FeatureFlags.EnableMFIPluginSignaturePublicKey` is on, its default — the compiled-in MFI key,
//! then each `PluginSettings.SignaturePublicKeyFiles` name, read from the configuration store's
//! `ConfigurationFiles`. A name that cannot be read is logged and skipped. The two compiled-in
//! keys are byte-identical copies of `plugin_public_keys.go`, in `plugin_keys/`.
//!
//! # What "verifies" means, from golang.org/x/crypto/openpgp
//!
//! [`verify_signature`] is `verifySignature`: each input is de-armored when it carries an armor
//! header anywhere (text before it is skipped) and used raw otherwise; the key ring is read; and
//! `CheckDetachedSignature` walks the signature packets **until the first whose issuer is in the
//! ring** with a signing-capable key, and checks only that one. So a stranger's signature ahead of
//! ours still verifies, and a bad signature of ours ahead of a good one does not. Anything that is
//! not a signature packet on the way is an error. The verdicts are pinned to Go's by
//! `fixtures/behaviour_plugin_signature.json`; Go's error text is only ever logged, so only the
//! verdict is compared.

use std::io::{BufRead, Read};

use mm_model::utils::AppError;
use mm_store::ConfigStore;
use pgp::composed::{Deserializable, SignedPublicKey};
use pgp::packet::{Packet, PacketParser, Signature, SignatureType, SubpacketData};
use pgp::types::{KeyDetails, KeyId};

use crate::App;

/// `mattermostPluginPublicKey` (plugin_public_keys.go:32).
const MATTERMOST_PLUGIN_PUBLIC_KEY: &[u8] =
    include_bytes!("plugin_keys/mattermost-plugin-public-key.asc");
/// `mfiPluginPublicKey` (plugin_public_keys.go:6).
const MFI_PLUGIN_PUBLIC_KEY: &[u8] = include_bytes!("plugin_keys/mfi-plugin-public-key.asc");

/// Why a signature did not verify. Go wraps each of these and only logs them.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("can't decode public key: {0}")]
    DecodeKey(std::io::Error),
    #[error("can't decode signature: {0}")]
    DecodeSignature(std::io::Error),
    #[error("can't read public key: {0}")]
    ReadKey(pgp::errors::Error),
    #[error("error while checking the signature: {0}")]
    Check(String),
}

/// `decodeIfArmored` (plugin_signature.go:144): the armored body when an armor header is found,
/// the input untouched when none is. `armor.Decode` skips anything before the header, and a body
/// or checksum that fails is an error when read — here, at once.
fn decode_if_armored(bytes: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let Some(start) = armor_start(bytes) else {
        return Ok(bytes.to_vec());
    };
    let mut dearmor = pgp::armor::Dearmor::new(&bytes[start..]);
    let mut out = Vec::new();
    dearmor.read_to_end(&mut out)?;
    Ok(out)
}

/// Where `armor.Decode` finds its start line: the first line beginning `-----BEGIN ` and ending
/// `-----` (x/crypto/openpgp/armor, `armorStart` / `armorEndOfLine`).
fn armor_start(bytes: &[u8]) -> Option<usize> {
    let mut offset = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.starts_with(b"-----BEGIN ") && trimmed.ends_with(b"-----") {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// A key the ring offers for one issuer id: the primary or a subkey, with the self-signature that
/// decides its usage (`openpgp.Key`).
enum RingKey<'a> {
    Primary(&'a pgp::packet::PublicKey),
    Subkey(&'a pgp::packet::PublicSubkey),
}

impl RingKey<'_> {
    fn verify(&self, signature: &Signature, message: &[u8]) -> Result<(), pgp::errors::Error> {
        match self {
            Self::Primary(key) => signature.verify(key, message),
            Self::Subkey(key) => signature.verify(key, message),
        }
    }
}

/// `KeyFlagSign` usage as `KeysByIdUsage` reads it: a self-signature without a key-flags
/// subpacket (`FlagsValid` false) admits any usage; one with it must carry the sign flag.
fn may_sign(self_signature: Option<&Signature>) -> bool {
    let Some(sig) = self_signature else {
        return true;
    };
    let flags_valid = sig.config().is_some_and(|c| {
        c.hashed_subpackets()
            .any(|p| matches!(p.data, SubpacketData::KeyFlags(_)))
    });
    !flags_valid || sig.key_flags().sign()
}

/// Whether a self-signature carries a revocation reason (`SelfSignature.RevocationReason`).
fn revoked(self_signature: Option<&Signature>) -> bool {
    self_signature.is_some_and(|s| s.revocation_reason_code().is_some())
}

/// `EntityList.KeysByIdUsage(id, KeyFlagSign)` (openpgp/keys.go).
fn keys_for_signing<'a>(ring: &'a [SignedPublicKey], id: &KeyId) -> Vec<RingKey<'a>> {
    let mut keys = Vec::new();
    for entity in ring {
        if !entity.details.revocation_signatures.is_empty() {
            continue;
        }
        if &entity.primary_key.legacy_key_id() == id {
            // The primary identity's self-signature: the first, unless one says it is primary.
            let mut self_sig: Option<&Signature> = None;
            for user in &entity.details.users {
                let Some(sig) = user.signatures.first() else {
                    continue;
                };
                if self_sig.is_none() {
                    self_sig = Some(sig);
                } else if sig.is_primary() {
                    self_sig = Some(sig);
                    break;
                }
            }
            if !revoked(self_sig) && may_sign(self_sig) {
                keys.push(RingKey::Primary(&entity.primary_key));
            }
        }
        for subkey in &entity.public_subkeys {
            if &subkey.key.legacy_key_id() == id {
                let binding = subkey.signatures.first();
                if !revoked(binding) && may_sign(binding) {
                    keys.push(RingKey::Subkey(&subkey.key));
                }
            }
        }
    }
    keys
}

/// `openpgp.ReadKeyRing`: every transferable public key in the input. A key the parser cannot
/// read is skipped, the way Go skips an unsupported or structurally bad entity. Go fails outright
/// on input that is not packets at all, where this yields an empty ring; both end in "not
/// verified", which is all the caller sees.
fn read_key_ring(bytes: &[u8]) -> Result<Vec<SignedPublicKey>, SignatureError> {
    let keys = SignedPublicKey::from_bytes_many(bytes).map_err(SignatureError::ReadKey)?;
    Ok(keys.filter_map(Result::ok).collect())
}

/// Port of `verifySignature` (plugin_signature.go:121) and `CheckDetachedSignature`.
pub fn verify_signature(
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<(), SignatureError> {
    let key = decode_if_armored(public_key).map_err(SignatureError::DecodeKey)?;
    let signature = decode_if_armored(signature).map_err(SignatureError::DecodeSignature)?;
    let ring = read_key_ring(&key)?;
    check_detached_signature(&ring, message, &signature)
}

fn check_detached_signature(
    ring: &[SignedPublicKey],
    message: &[u8],
    signature: &[u8],
) -> Result<(), SignatureError> {
    let check = |why: String| SignatureError::Check(why);
    let mut reader: &[u8] = signature;
    if reader.fill_buf().is_ok_and(<[u8]>::is_empty) {
        return Err(check(
            "openpgp: signature made by unknown entity".to_owned(),
        ));
    }
    for packet in PacketParser::new(reader) {
        let packet = packet.map_err(|e| check(e.to_string()))?;
        let Packet::Signature(sig) = packet else {
            return Err(check(
                "openpgp: invalid data: non signature packet found".to_owned(),
            ));
        };
        let Some(issuer) = sig.issuer_key_id().last().copied().copied() else {
            return Err(check(
                "openpgp: invalid data: signature doesn't have an issuer".to_owned(),
            ));
        };
        let keys = keys_for_signing(ring, &issuer);
        if keys.is_empty() {
            continue;
        }
        // `hashForSignature`: binary and text signatures only.
        if !matches!(sig.typ(), Some(SignatureType::Binary | SignatureType::Text)) {
            return Err(check("openpgp: unsupported signature type".to_owned()));
        }
        let mut last = String::new();
        for key in &keys {
            match key.verify(&sig, message) {
                Ok(()) => return Ok(()),
                Err(err) => last = err.to_string(),
            }
        }
        return Err(check(last));
    }
    Err(check(
        "openpgp: signature made by unknown entity".to_owned(),
    ))
}

impl App {
    /// Port of `Channels.verifyPlugin` (plugin_signature.go:76): the Mattermost key, the MFI key
    /// while its flag is on, then each configured key file; the first that verifies wins. None
    /// is the 500 `api.plugin.verify_plugin.app_error`.
    #[tracing::instrument(skip_all)]
    pub async fn verify_plugin(
        &self,
        plugin: &[u8],
        signature: &[u8],
    ) -> Result<(), Box<AppError>> {
        if verify_signature(MATTERMOST_PLUGIN_PUBLIC_KEY, plugin, signature).is_ok() {
            tracing::debug!("Plugin signature verified using hard-coded public key");
            return Ok(());
        }
        let config = self.config();
        if config.feature_flag_enable_mfi_plugin_signature_public_key
            && verify_signature(MFI_PLUGIN_PUBLIC_KEY, plugin, signature).is_ok()
        {
            tracing::debug!("Plugin signature verified using hard-coded MFI public key");
            return Ok(());
        }
        for name in &config.plugin_signature_public_key_files {
            let key = match self.store.config().get_file(name).await {
                Ok(Some(key)) => key,
                Ok(None) | Err(_) => {
                    tracing::warn!(public_key_path = %name, "Unable to read configured signature public key file");
                    continue;
                }
            };
            if verify_signature(&key, plugin, signature).is_ok() {
                tracing::debug!(public_key_path = %name, "Plugin signature verified using configured public key");
                return Ok(());
            }
        }
        Err(AppError::boxed(
            "VerifyPlugin",
            "api.plugin.verify_plugin.app_error",
            None,
            "",
            500,
        ))
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use base64::Engine;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_plugin_signature.json"
        ))
        .expect("behaviour_plugin_signature.json is generated by reference/dump")
    }

    fn bytes(oracle: &serde_json::Value, section: &str, name: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(oracle[section][name].as_str().expect("a base64 string"))
            .expect("base64")
    }

    /// Every verdict in the corpus, Go's against ours.
    #[test]
    fn verify_signature_matches_go() {
        let oracle = oracle();
        let cases = oracle["verify"].as_array().expect("an array of cases");
        assert!(cases.len() >= 20);
        let mut accepted = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let key = bytes(&oracle, "keys", case["key"].as_str().unwrap());
            let message = bytes(&oracle, "bundles", case["message"].as_str().unwrap());
            let signature = bytes(&oracle, "signatures", case["signature"].as_str().unwrap());
            let verified = case["verified"].as_bool().unwrap();
            let ours = verify_signature(&key, &message, &signature);
            assert_eq!(
                ours.is_ok(),
                verified,
                "{name}: Go {} / Rust {ours:?}",
                case["err"]
            );
            accepted += usize::from(verified);
        }
        assert!(
            accepted >= 5 && accepted < cases.len(),
            "both verdicts occur"
        );
    }

    /// The compiled-in keys are Go's bytes and parse as key rings; a test key signature does not
    /// verify against them.
    #[test]
    fn the_compiled_in_keys_are_readable_and_do_not_accept_the_test_key() {
        let oracle = oracle();
        let message = bytes(&oracle, "bundles", "alpha");
        let signature = bytes(&oracle, "signatures", "alpha_binary");
        for key in [MATTERMOST_PLUGIN_PUBLIC_KEY, MFI_PLUGIN_PUBLIC_KEY] {
            let ring = read_key_ring(&decode_if_armored(key).unwrap()).unwrap();
            assert_eq!(ring.len(), 1);
            assert!(verify_signature(key, &message, &signature).is_err());
        }
    }
}
