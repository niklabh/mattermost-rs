//! Port of `channels/app/password/hashers/pbkdf2.go`.
//!
//! The hasher a Mattermost server at the pinned SHA actually writes with.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hmac::Hmac;
use mm_model::user::{PasswordHashError, UserPasswordHasher};
use rand::TryRngCore as _;
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

use super::{CompareError, Phc, check_length, err_password_too_long};

/// Port of `hashers.PBKDF2FunctionId` (pbkdf2.go:22).
pub const PBKDF2_FUNCTION_ID: &str = "pbkdf2";

/// Port of `defaultPRFName` (pbkdf2.go:27) — unexported in Go, but it reaches the stored string
/// as the `f=` parameter, so it is part of the format rather than an implementation detail.
pub const PBKDF2_DEFAULT_PRF_NAME: &str = "SHA256";

/// Port of `defaultWorkFactor` (pbkdf2.go:28).
///
/// 600,000 iterations, per OWASP's PBKDF2-HMAC-SHA256 recommendation. This is deliberately
/// expensive — roughly a quarter-second of CPU — which is why [`UserPasswordHasher::hash`] must
/// not be called on a tokio worker thread without `spawn_blocking`.
pub const PBKDF2_DEFAULT_WORK_FACTOR: u32 = 600_000;

/// Port of `defaultKeyLength` (pbkdf2.go:29) — 32 bytes, i.e. SHA-256's own output size.
pub const PBKDF2_DEFAULT_KEY_LENGTH: usize = 32;

/// Port of `saltLenBytes` (pbkdf2.go:32).
pub const PBKDF2_SALT_LEN_BYTES: usize = 16;

/// Port of `hashers.PBKDF2` (pbkdf2.go:60).
///
/// # The stored format
///
/// ```text
/// $pbkdf2$f=SHA256,w=600000,l=32$<salt>$<hash>
/// ```
///
/// Four details of it are load-bearing and none is visible from the algorithm:
///
/// - **The base64 is `RawStdEncoding`** — the standard alphabet with **no padding**. A padded
///   encoder appends `=` and produces a string Go's parser rejects; a URL-safe alphabet swaps
///   `+/` for `-_` and silently changes the salt.
/// - **The parameters are ordered `f,w,l`** and are compared as text by `IsPHCValid`, so emitting
///   them in any other order produces a hash Go treats as a *different* hasher's — it would not
///   fail to parse, it would fail to match, and the user could not log in.
/// - **The work factor and key length are decimal strings** inside the header, not encoded
///   numbers.
/// - **The header ends with `$`**, so the salt follows immediately and is *not* separately
///   prefixed. The `$` before the hash is written separately.
///
/// The struct precomputes that header exactly as Go's `NewPBKDF2` does, which is also what makes
/// a wrong parameter a construction-time error rather than a per-hash one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pbkdf2 {
    work_factor: u32,
    key_length: usize,
    phc_header: String,
}

impl Pbkdf2 {
    /// Port of `hashers.DefaultPBKDF2` (pbkdf2.go:71).
    ///
    /// Go `panic`s if its own defaults are invalid; they are constants, so this cannot fail and
    /// the port has no error to return.
    pub fn default_params() -> Self {
        Self::new(PBKDF2_DEFAULT_WORK_FACTOR, PBKDF2_DEFAULT_KEY_LENGTH)
            .unwrap_or_else(|_| unreachable!("the compiled-in defaults are non-zero"))
    }

    /// Port of `hashers.NewPBKDF2` (pbkdf2.go:80).
    ///
    /// Go's guards are `workFactor <= 0` and `keyLength <= 0` on signed ints. The unsigned types
    /// here rule out the negative half at the type level, so only zero remains reachable.
    pub fn new(work_factor: u32, key_length: usize) -> Result<Self, PasswordHashError> {
        if work_factor == 0 {
            return Err(PasswordHashError::Other(
                "work factor must be strictly positive".to_owned(),
            ));
        }
        if key_length == 0 {
            return Err(PasswordHashError::Other(
                "key length must be strictly positive".to_owned(),
            ));
        }
        Ok(Self {
            work_factor,
            key_length,
            phc_header: format!(
                "${PBKDF2_FUNCTION_ID}$f={PBKDF2_DEFAULT_PRF_NAME},w={work_factor},l={key_length}$"
            ),
        })
    }

    /// The precomputed `$pbkdf2$f=…,w=…,l=…$` prefix every hash of this instance shares.
    pub fn phc_header(&self) -> &str {
        &self.phc_header
    }

    pub fn work_factor(&self) -> u32 {
        self.work_factor
    }

    pub fn key_length(&self) -> usize {
        self.key_length
    }

    /// Port of `(PBKDF2).hashWithSalt` (pbkdf2.go:130) — the base64 digest alone.
    ///
    /// Public because it is what makes the Go-parity test *exact*: given the salt decoded out of
    /// a hash the Go server produced, this must reproduce Go's digest byte-for-byte. Without a
    /// salt-taking entry point the best a test could do is a Rust-only round trip, which proves
    /// nothing about cross-server compatibility.
    pub fn hash_with_salt(&self, password: &str, salt: &[u8]) -> String {
        let mut out = vec![0u8; self.key_length];
        // The generic form, spelling out the PRF, because `f=SHA256` in the header is a promise
        // about *which* PRF and the two must not be able to drift apart.
        //
        // The only error is `InvalidLength` from the MAC's key init, and HMAC accepts a key of
        // any length — so this is unreachable. Swallowed rather than propagated because making
        // the signature fallible would push a branch no caller can take onto every call site.
        let _ =
            pbkdf2::pbkdf2::<Hmac<Sha256>>(password.as_bytes(), salt, self.work_factor, &mut out);
        STANDARD_NO_PAD.encode(out)
    }

    /// Port of `hashers.NewPBKDF2FromPHC` (pbkdf2.go:116).
    ///
    /// Rebuilds a hasher from a **stored** row's own parameters, so a password hashed under an
    /// older work factor can still be verified. `strconv.Atoi("")` fails, so a missing `w` or `l`
    /// takes the same branch as a malformed one — which is why a bare `$pbkdf2` is a hard error
    /// rather than a fallback to bcrypt.
    pub fn from_phc(phc: &Phc) -> Result<Self, PasswordHashError> {
        let raw_w = phc.params.get("w").map_or("", String::as_str);
        let work_factor: u32 = raw_w.parse().map_err(|_| {
            PasswordHashError::Other(format!("invalid work factor parameter 'w={raw_w}'"))
        })?;

        let raw_l = phc.params.get("l").map_or("", String::as_str);
        let key_length: usize = raw_l.parse().map_err(|_| {
            PasswordHashError::Other(format!("invalid key length parameter 'l={raw_l}'"))
        })?;

        Self::new(work_factor, key_length)
    }

    /// Port of `(PBKDF2).IsPHCValid` (pbkdf2.go:216).
    ///
    /// An **exact** parameter match: the function id, exactly three parameters, `f=SHA256`, and
    /// `w`/`l` equal to this instance's — compared as **text**, which is why the port formats its
    /// own numbers rather than parsing the stored ones. `w=0600000` would be numerically equal
    /// and is not a match.
    ///
    /// This is also the migration trigger: a row whose parameters have fallen behind fails here,
    /// and `GetHasherFromPHCString` then rebuilds a hasher from the row instead.
    pub fn is_phc_valid(&self, phc: &Phc) -> bool {
        phc.id == PBKDF2_FUNCTION_ID
            && phc.params.len() == 3
            && phc.params.get("f").map(String::as_str) == Some(PBKDF2_DEFAULT_PRF_NAME)
            && phc.params.get("w").map(String::as_str)
                == Some(self.work_factor.to_string().as_str())
            && phc.params.get("l").map(String::as_str) == Some(self.key_length.to_string().as_str())
    }

    /// Port of `(PBKDF2).CompareHashAndPassword` (pbkdf2.go:188).
    ///
    /// The order of the guards is Go's and is worth keeping: the length check comes first, then
    /// (under FIPS, which is [D-110]) a minimum length, then parameter validation, then the
    /// decode, and only then the comparison.
    ///
    /// The comparison is **constant-time**, over the base64 text rather than the decoded bytes —
    /// Go compares `[]byte(hash.Hash)` against the freshly encoded digest, so the port must
    /// encode rather than decode to stay on the same code path. `subtle` is already a workspace
    /// dependency for the OAuth secret compare, and this is the same kind of property: a
    /// short-circuiting `==` leaks the matching prefix length through timing.
    pub fn compare_hash_and_password(&self, phc: &Phc, password: &str) -> Result<(), CompareError> {
        if password.len() > super::PASSWORD_MAX_LENGTH_BYTES {
            return Err(CompareError::TooLong(err_password_too_long()));
        }

        // Go also short-circuits here when `fipsMinKeyLength > 0`, which only a `requirefips`
        // build sets. Non-FIPS is the default and the only shape modelled — see [D-110].

        if !self.is_phc_valid(phc) {
            return Err(CompareError::Other(
                "the stored password does not comply with the PBKDF2 parser's PHC serialization"
                    .to_owned(),
            ));
        }

        let salt = STANDARD_NO_PAD
            .decode(&phc.salt)
            .map_err(|e| CompareError::Other(format!("failed decoding hash's salt: {e}")))?;

        let new_hash = self.hash_with_salt(password, &salt);

        if !bool::from(phc.hash.as_bytes().ct_eq(new_hash.as_bytes())) {
            return Err(CompareError::Mismatched);
        }

        Ok(())
    }

    /// Assemble the stored string from an already-chosen salt.
    ///
    /// Split out from [`Self::hash`] so the salt can be supplied by a test; [`Self::hash`] is the
    /// only production entry point and always draws a fresh one.
    pub fn format_hash(&self, password: &str, salt: &[u8]) -> String {
        let mut s = String::with_capacity(self.phc_header.len() + 24 + 1 + 44);
        s.push_str(&self.phc_header);
        s.push_str(&STANDARD_NO_PAD.encode(salt));
        s.push('$');
        s.push_str(&self.hash_with_salt(password, salt));
        s
    }
}

impl Default for Pbkdf2 {
    fn default() -> Self {
        Self::default_params()
    }
}

impl UserPasswordHasher for Pbkdf2 {
    /// Port of `(PBKDF2).Hash` (pbkdf2.go:147).
    ///
    /// The length check runs **first**, before the salt is drawn — so an over-long password
    /// costs no entropy and no CPU.
    fn hash(&self, password: &str) -> Result<String, PasswordHashError> {
        check_length(password)?;

        let mut salt = [0u8; PBKDF2_SALT_LEN_BYTES];
        // Go: `io.ReadFull(rand.Reader, salt)` over `crypto/rand`. `OsRng` is the same source.
        // The error is propagated rather than swallowed: a salt that is not random is a silent
        // catastrophe, so a failing CSPRNG must refuse to hash.
        rand::rngs::OsRng.try_fill_bytes(&mut salt).map_err(|e| {
            PasswordHashError::Other(format!("unable to generate salt for user: {e}"))
        })?;

        Ok(self.format_hash(password, &salt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap instance, so the tests that do not assert against Go's parameters stay fast.
    /// `FastTestHasher` in `hashers_dev.go` does exactly this.
    fn cheap() -> Pbkdf2 {
        Pbkdf2::new(1_000, PBKDF2_DEFAULT_KEY_LENGTH).unwrap()
    }

    #[test]
    fn the_header_is_go_s_header() {
        assert_eq!(
            Pbkdf2::default_params().phc_header(),
            "$pbkdf2$f=SHA256,w=600000,l=32$"
        );
    }

    #[test]
    fn zero_parameters_are_rejected() {
        assert!(Pbkdf2::new(0, 32).is_err());
        assert!(Pbkdf2::new(600_000, 0).is_err());
    }

    #[test]
    fn each_hash_draws_a_fresh_salt() {
        let h = cheap();
        let a = h.hash("hunter2").unwrap();
        let b = h.hash("hunter2").unwrap();
        assert_ne!(a, b, "two hashes of one password must differ");
        assert_eq!(a[..h.phc_header().len()], b[..h.phc_header().len()]);
    }

    /// The salt and digest are raw-std base64: standard alphabet, no `=` padding.
    #[test]
    fn the_encoding_is_unpadded_standard_base64() {
        let s = cheap().hash("hunter2").unwrap();
        let parts: Vec<&str> = s.split('$').collect();
        // ["", "pbkdf2", "f=SHA256,w=1000,l=32", salt, hash]
        assert_eq!(parts.len(), 5);
        assert!(!parts[3].contains('='), "no padding on the salt");
        assert!(!parts[4].contains('='), "no padding on the digest");
        assert_eq!(parts[3].len(), 22, "16 bytes unpadded");
        assert_eq!(parts[4].len(), 43, "32 bytes unpadded");
        assert!(
            !parts[3].contains('-') && !parts[3].contains('_'),
            "standard alphabet, not URL-safe"
        );
    }

    #[test]
    fn an_over_long_password_costs_nothing() {
        let pw = "x".repeat(73);
        crate::password::assert_too_long(cheap().hash(&pw));
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::password::go_parity::oracle;

    /// The format header, read off Go rather than off pbkdf2.go's doc comment.
    #[test]
    fn the_phc_header_matches_go() {
        let f = &oracle()["pbkdf2_format"];
        let h = Pbkdf2::default_params();
        assert_eq!(h.phc_header(), f["header"].as_str().unwrap());
        assert_eq!(f["id"], PBKDF2_FUNCTION_ID);
        assert_eq!(f["prf"], PBKDF2_DEFAULT_PRF_NAME);
        assert_eq!(
            f["work_factor_int"].as_u64().unwrap() as u32,
            PBKDF2_DEFAULT_WORK_FACTOR
        );
        assert_eq!(
            f["key_length_int"].as_u64().unwrap() as usize,
            PBKDF2_DEFAULT_KEY_LENGTH
        );
        assert_eq!(
            f["salt_len_bytes"].as_u64().unwrap() as usize,
            PBKDF2_SALT_LEN_BYTES
        );
        assert_eq!(f["is_phc_valid"], true);
    }

    /// **The test this whole module exists for.**
    ///
    /// For each password, take the hash the Go server produced, decode its salt, recompute here,
    /// and assert the entire string byte-for-byte. PBKDF2 is deterministic given a salt, so this
    /// is exact rather than structural: it proves that a password hashed by the Rust server lands
    /// in `Users.Password` in a form the Go server reading the same row will accept.
    ///
    /// Note this runs at Go's real 600,000 iterations — a cheap work factor would prove nothing,
    /// since the iteration count is part of what is being matched.
    #[test]
    fn recomputes_go_s_hashes_byte_for_byte() {
        let h = Pbkdf2::default_params();
        let mut checked = 0;

        for case in oracle()["cases"].as_array().unwrap() {
            if !case["hashes_ok"].as_bool().unwrap() {
                continue;
            }
            let name = case["name"].as_str().unwrap();
            let password = case["password"].as_str().unwrap();
            let go_hash = case["pbkdf2"].as_str().unwrap();
            let salt_b64 = case["pbkdf2_salt_b64"].as_str().unwrap();

            let salt = STANDARD_NO_PAD
                .decode(salt_b64)
                .unwrap_or_else(|e| panic!("{name}: Go's salt is not raw-std base64: {e}"));
            assert_eq!(
                salt.len(),
                case["pbkdf2_salt_bytes"].as_u64().unwrap() as usize,
                "{name}: salt length"
            );

            assert_eq!(
                h.format_hash(password, &salt),
                go_hash,
                "{name}: the whole stored string must match Go's"
            );
            checked += 1;
        }

        assert_eq!(checked, 6, "every hashable case in the corpus was checked");
    }

    /// The corpus is not degenerate: a *wrong* password must not reproduce Go's digest.
    ///
    /// Without this, an implementation that ignored the password entirely and echoed the salt
    /// would pass the test above for a corpus of one.
    #[test]
    fn a_different_password_does_not_reproduce_go_s_hash() {
        let h = Pbkdf2::default_params();
        let case = &oracle()["cases"][1]; // "ascii" / hunter2
        let salt = STANDARD_NO_PAD
            .decode(case["pbkdf2_salt_b64"].as_str().unwrap())
            .unwrap();
        assert_ne!(
            h.format_hash("hunter3", &salt),
            case["pbkdf2"].as_str().unwrap()
        );
    }

    /// An embedded NUL must be hashed, not treated as a terminator.
    ///
    /// A C-derived implementation using `strlen` would hash `"ab"` and then accept **any**
    /// password beginning `ab`. Go hashes all five bytes; so must we.
    #[test]
    fn an_embedded_nul_is_part_of_the_password() {
        let h = Pbkdf2::default_params();
        let case = oracle()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "embedded_nul")
            .unwrap();
        let password = case["password"].as_str().unwrap();
        assert_eq!(password.len(), 5, "the NUL is one of the five bytes");

        let salt = STANDARD_NO_PAD
            .decode(case["pbkdf2_salt_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            h.format_hash(password, &salt),
            case["pbkdf2"].as_str().unwrap()
        );
        assert_ne!(
            h.format_hash("ab", &salt),
            case["pbkdf2"].as_str().unwrap(),
            "truncating at the NUL must not produce Go's hash"
        );
    }
}
