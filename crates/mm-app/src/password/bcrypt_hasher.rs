//! Port of `channels/app/password/hashers/bcrypt.go`.
//!
//! The **legacy** hasher. Nothing writes it any more — see the module docs on why it is here.

use bcrypt::{HashParts, Version};
use mm_model::user::{PasswordHashError, UserPasswordHasher};
use rand::TryRngCore as _;

use super::{CompareError, Phc, check_length, err_password_too_long};

/// Port of `hashers.BCryptCost` (bcrypt.go:42).
///
/// > the value of the cost parameter used throughout the history of the codebase
///
/// Which is what makes it load-bearing rather than a tuning knob: it appears in the stored string
/// as the two digits after `$2a$`, and every existing `Users.Password` row that predates the
/// PBKDF2 migration carries it.
pub const BCRYPT_COST: u32 = 10;

/// The salt length bcrypt's format fixes, in bytes. 16 bytes encode to the 22 base64 characters
/// the format reserves.
const BCRYPT_SALT_LEN_BYTES: usize = 16;

/// Port of `hashers.BCrypt` (bcrypt.go:37).
///
/// # The stored format is not PHC, despite looking like it
///
/// ```text
/// $2a$10$z0OlN1MpiLVlLTyE1xtEjOJ6/xV95RAwwIUaYKQBAqoeyvPgLEnUa
/// ```
///
/// `$2a` is a version tag rather than a function id, `10` is a bare number rather than
/// `name=value`, and there is no separator between the 22-character salt and the 31-character
/// digest. bcrypt.go's own doc comment says so, and `IsPHCValid` returns a flat `false` for this
/// hasher — which is exactly how `GetHasherFromPHCString` ends up routing unparseable stored
/// values here.
///
/// # The base64 is bcrypt's own alphabet
///
/// `./A-Za-z0-9`, not the standard one — the leading `.` and `/` are the giveaway. Decoding a
/// bcrypt salt with a standard base64 decoder mangles it silently, which is why this port leans
/// on the `bcrypt` crate's `HashParts` rather than slicing the string itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BCrypt;

impl BCrypt {
    /// Port of `hashers.NewBCrypt` (bcrypt.go:46).
    pub fn new() -> Self {
        Self
    }

    /// Hash with a caller-supplied salt.
    ///
    /// Not in the Go API — `x/crypto/bcrypt` does not expose one either — and it exists for the
    /// same reason [`super::Pbkdf2::hash_with_salt`] does: bcrypt is deterministic given its
    /// salt, so a test holding a hash the **Go** server produced can decode that salt, recompute,
    /// and assert Go's whole string. A Rust-only round trip would prove only that this module
    /// agrees with itself.
    ///
    /// The version is pinned to `2a`, which is what `x/crypto/bcrypt` emits.
    pub fn hash_with_salt(
        &self,
        password: &str,
        salt: [u8; BCRYPT_SALT_LEN_BYTES],
    ) -> Result<String, PasswordHashError> {
        check_length(password)?;
        let parts: HashParts = bcrypt::hash_with_salt(password, BCRYPT_COST, salt)
            .map_err(|e| PasswordHashError::Other(e.to_string()))?;
        Ok(parts.format_for_version(Version::TwoA))
    }

    /// Port of `(BCrypt).IsPHCValid` (bcrypt.go:87) — **always false**.
    ///
    /// Not a stub. bcrypt's format is not PHC-compliant, so no parsed PHC can ever describe it,
    /// and returning `false` unconditionally is what makes `GetHasherFromPHCString` treat a
    /// bcrypt row as needing migration on every login.
    pub fn is_phc_valid(&self, _phc: &Phc) -> bool {
        false
    }

    /// Port of `(BCrypt).CompareHashAndPassword` (bcrypt.go:73).
    ///
    /// Reads **only** `phc.hash`, which for a bcrypt row holds the entire stored string — see
    /// [`super::get_hasher_from_phc_string`]. Every other field is ignored, which is what
    /// bcrypt.go means by calling this hasher an edge case.
    ///
    /// # The length check is the security-relevant line
    ///
    /// `x/crypto/bcrypt.CompareHashAndPassword` has no length guard and its key schedule consumes
    /// only the first 72 bytes, so it **accepts** a 73-byte password against a hash of that
    /// password's first 72. The `hashers` package puts the check back, and this port follows the
    /// package. Dropping it would authenticate a login the Go server denies, against the same
    /// shared `Users.Password` row.
    pub fn compare_hash_and_password(&self, phc: &Phc, password: &str) -> Result<(), CompareError> {
        if password.len() > super::PASSWORD_MAX_LENGTH_BYTES {
            return Err(CompareError::TooLong(err_password_too_long()));
        }

        match bcrypt::verify(password, &phc.hash) {
            Ok(true) => Ok(()),
            Ok(false) => Err(CompareError::Mismatched),
            // Go returns `bcrypt`'s own error here for a malformed hash rather than folding it
            // into the mismatch — so a corrupt row is distinguishable from a wrong password.
            Err(e) => Err(CompareError::Other(e.to_string())),
        }
    }
}

impl UserPasswordHasher for BCrypt {
    /// Port of `(BCrypt).Hash` (bcrypt.go:57).
    ///
    /// Go's wrapper does two things over `bcrypt.GenerateFromPassword`: it translates
    /// `bcrypt.ErrPasswordTooLong` into the package's own sentinel, and it returns a `string`
    /// rather than a `[]byte`.
    ///
    /// The length check is hoisted **above** the crate call here rather than translated from the
    /// error afterwards. That is not a shortcut — see `bcrypt_truncation` in the oracle: bcrypt
    /// silently consumes only the first 72 bytes, so the boundary has to be enforced explicitly
    /// wherever it is not already, and doing it here makes the two hashers agree by construction.
    fn hash(&self, password: &str) -> Result<String, PasswordHashError> {
        check_length(password)?;

        let mut salt = [0u8; BCRYPT_SALT_LEN_BYTES];
        rand::rngs::OsRng.try_fill_bytes(&mut salt).map_err(|e| {
            PasswordHashError::Other(format!("unable to generate salt for user: {e}"))
        })?;

        self.hash_with_salt(password, salt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_has_go_s_shape() {
        let h = BCrypt::new().hash("hunter2").unwrap();
        assert_eq!(h.len(), 60);
        assert!(h.starts_with("$2a$10$"));
    }

    #[test]
    fn each_hash_draws_a_fresh_salt() {
        let a = BCrypt::new().hash("hunter2").unwrap();
        let b = BCrypt::new().hash("hunter2").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn an_over_long_password_is_refused_rather_than_truncated() {
        // 73 bytes. The crate would happily hash the first 72; Go's package refuses.
        let pw = "x".repeat(73);
        super::super::assert_too_long(BCrypt::new().hash(&pw));
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::password::go_parity::oracle;

    /// The layout, measured off a real Go hash rather than transcribed from bcrypt.go's comment.
    #[test]
    fn the_format_matches_go() {
        let f = &oracle()["bcrypt_format"];
        assert_eq!(f["example_total_len"], 60);
        assert_eq!(f["version_prefix"], "$2a$");
        assert_eq!(f["cost_digits"], BCRYPT_COST.to_string());
        assert_eq!(f["salt_b64_len"], 22);
        assert_eq!(f["digest_b64_len"], 31);
        assert_eq!(
            f["is_phc"], false,
            "shaped like PHC, and IsPHCValid still returns false"
        );
        assert!(
            f["alphabet_starts"].as_str().unwrap().starts_with("./"),
            "bcrypt's own base64 alphabet, not the standard one"
        );
    }

    /// **The test this module exists for**: recompute Go's hashes from Go's own salts.
    ///
    /// bcrypt is deterministic given cost and salt, so decoding the salt out of a hash the Go
    /// server produced and re-emitting the whole 60-byte string is an exact cross-language check.
    /// The salt is decoded with bcrypt's alphabet via the crate's own parser — decoding it as
    /// standard base64 would produce different bytes and a passing-looking test would be
    /// impossible to write.
    #[test]
    fn recomputes_go_s_hashes_byte_for_byte() {
        let h = BCrypt::new();
        let mut checked = 0;

        for case in oracle()["cases"].as_array().unwrap() {
            if !case["hashes_ok"].as_bool().unwrap() {
                continue;
            }
            let name = case["name"].as_str().unwrap();
            let password = case["password"].as_str().unwrap();
            let go_hash = case["bcrypt"].as_str().unwrap();

            assert_eq!(
                case["bcrypt_cost"].as_u64().unwrap() as u32,
                BCRYPT_COST,
                "{name}: Go's cost"
            );

            let salt = decode_bcrypt_salt(case["bcrypt_salt_b64"].as_str().unwrap());
            assert_eq!(
                h.hash_with_salt(password, salt).unwrap(),
                go_hash,
                "{name}: the whole 60-byte string must match Go's"
            );
            checked += 1;
        }

        assert_eq!(checked, 6, "every hashable case in the corpus was checked");
    }

    #[test]
    fn a_different_password_does_not_reproduce_go_s_hash() {
        let case = &oracle()["cases"][1]; // "ascii" / hunter2
        let salt = decode_bcrypt_salt(case["bcrypt_salt_b64"].as_str().unwrap());
        assert_ne!(
            BCrypt::new().hash_with_salt("hunter3", salt).unwrap(),
            case["bcrypt"].as_str().unwrap()
        );
    }

    /// An embedded NUL is part of the password, not a terminator. See the PBKDF2 twin.
    #[test]
    fn an_embedded_nul_is_part_of_the_password() {
        let case = oracle()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "embedded_nul")
            .unwrap();
        let salt = decode_bcrypt_salt(case["bcrypt_salt_b64"].as_str().unwrap());
        let go_hash = case["bcrypt"].as_str().unwrap();

        assert_eq!(
            BCrypt::new()
                .hash_with_salt(case["password"].as_str().unwrap(), salt)
                .unwrap(),
            go_hash
        );
        assert_ne!(
            BCrypt::new().hash_with_salt("ab", salt).unwrap(),
            go_hash,
            "truncating at the NUL must not produce Go's hash"
        );
    }

    /// The 72-byte truncation, and the asymmetry it creates between the two Go layers.
    ///
    /// `x/crypto/bcrypt.CompareHashAndPassword` **accepts** a 73-byte password against a hash of
    /// its first 72 bytes, while `GenerateFromPassword` refuses to produce one — and the
    /// `hashers` package puts the length check back on the compare path, so through the package
    /// the 73-byte password is rejected.
    ///
    /// That distinction is a security property, not a curiosity: a port reproducing the crate's
    /// behaviour rather than the package's would authenticate a login the Go server denies. It is
    /// pinned here because the verification half is not ported yet ([D-109]) and this is the fact
    /// whoever ports it needs.
    #[test]
    fn the_truncation_boundary_is_pinned_for_whoever_ports_verification() {
        let t = &oracle()["bcrypt_truncation"];
        assert_eq!(t["password_bytes"], 72);
        assert_eq!(t["longer_bytes"], 73);
        assert_eq!(
            t["x_crypto_accepts_73_byte_password"], true,
            "the CRATE-level primitive truncates and accepts"
        );
        assert_eq!(
            t["hashers_package_accepts_73_byte_password"], false,
            "the PACKAGE puts the length check back — this is the behaviour to match"
        );
        assert_eq!(
            t["pbkdf2_accepts_73_byte_password"], false,
            "PBKDF2 does not truncate; it simply hashes a different password"
        );
        assert_eq!(t["truncation_boundary_is_72"], true);
        assert_eq!(t["generate_still_refuses_73"], true);

        // Our `hash` matches the package, not the primitive.
        let long = "a".repeat(73);
        crate::password::assert_too_long(BCrypt::new().hash(&long));
    }

    /// Decode a bcrypt-alphabet salt into the 16 raw bytes `hash_with_salt` wants.
    ///
    /// bcrypt's base64 is `./A-Za-z0-9`, so this cannot go through a standard decoder. The
    /// `bcrypt` crate does not export its decoder, so the alphabet is spelled out — and the
    /// oracle records the same string, which `the_format_matches_go` checks the front of.
    fn decode_bcrypt_salt(b64: &str) -> [u8; 16] {
        const ALPHABET: &[u8] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        assert_eq!(b64.len(), 22, "a bcrypt salt is 22 encoded characters");

        let mut bits = Vec::with_capacity(22 * 6);
        for ch in b64.bytes() {
            let idx = ALPHABET
                .iter()
                .position(|c| *c == ch)
                .unwrap_or_else(|| panic!("{ch:?} is not in bcrypt's alphabet"));
            for shift in (0..6).rev() {
                bits.push((idx >> shift) & 1 == 1);
            }
        }

        // 22 characters carry 132 bits; the encoding pads the last 4, which are dropped.
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            for bit in 0..8 {
                if bits[i * 8 + bit] {
                    *byte |= 1 << (7 - bit);
                }
            }
        }
        out
    }
}
