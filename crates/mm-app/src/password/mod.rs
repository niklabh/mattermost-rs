//! Port of `channels/app/password/hashers/` — the password hashers.
//!
//! This is [D-108]'s dependency, and the reason it lives in `mm-app` rather than in `mm-model`
//! is licensing: `model.UserPasswordHasher` is declared in `server/public` (Apache-2.0) but every
//! implementation is in `server/channels/` (AGPL), and `mm-model` may not derive from that tree
//! ([D-031]). Go draws the line in the same place, which is why the hasher is a parameter to
//! `User.PreSave` at all.
//!
//! # bcrypt is not what a Mattermost server writes
//!
//! [D-108] was raised on the premise "Go uses `golang.org/x/crypto/bcrypt`". At the pinned SHA
//! that is the **legacy** path. `hashers.go` says:
//!
//! ```text
//! latestHasher PasswordHasher = DefaultPBKDF2()
//! ```
//!
//! and the only caller of `User.PreSave` in the whole tree is
//! `channels/store/sqlstore/user_store.go:180`:
//!
//! ```text
//! if err := user.PreSave(hashers.GetLatestHasher()); err != nil {
//! ```
//!
//! So every password the Go server writes today is a PBKDF2 PHC string. bcrypt survives only as
//! the fallback `GetHasherFromPHCString` returns for a stored hash that does not parse as PHC —
//! i.e. rows written before Mattermost's migration.
//!
//! Both are ported. [`Pbkdf2`] is [`latest_hasher`], because writing bcrypt into a column the Go
//! server is actively migrating **away** from would be a new divergence rather than compatibility.
//! [`BCrypt`] is owed regardless: those old rows still exist and still have to verify.
//!
//! # What is pinned, and how
//!
//! Both algorithms are deterministic **given the salt**, and both stored formats carry their own
//! salt. So the parity tests take Go's real output, decode the salt back out of it, recompute,
//! and assert the **whole Go string** byte-for-byte. That pins the write direction exactly — not
//! "Rust can read Go", but "Rust emits Go's bytes" — without needing both runtimes in one
//! process. See `fixtures/behaviour_password.json` and `reference/dump/behaviour_password.go`.
//!
//! # Verification, and how a stored value picks its hasher
//!
//! [`get_hasher_from_phc_string`] is the whole mechanism, and its shape is counter-intuitive:
//! **a parse failure is not an error.** A stored value that is not PHC is a pre-migration bcrypt
//! row, so the parser failing is how bcrypt is recognised, and the entire stored string is handed
//! back as the `hash` field. `""` and `"not a hash at all"` therefore route to bcrypt too, and
//! then fail to verify — which is the right answer, arrived at by a route worth knowing about.
//!
//! Two further routing facts, both measured:
//!
//! - **An unknown function id routes to bcrypt even when the string parses.** A valid
//!   `$argon2id$…` PHC is well-formed, is not PBKDF2, and falls to the `default` arm. So it is
//!   handed to bcrypt with the whole string as its hash.
//! - **`$pbkdf2` with bad or missing parameters is a hard error**, not a fallback. It matched the
//!   function id, so `NewPBKDF2FromPHC` runs and its failure propagates.
//!
//! # Not ported
//!
//! FIPS mode ([D-110]) and `App.migratePassword`, which belongs with the login route rather than
//! here — [`is_latest_hasher`] is the predicate it would branch on.

mod bcrypt_hasher;
mod pbkdf2_hasher;
pub mod phcparser;

pub use bcrypt_hasher::{BCRYPT_COST, BCrypt};
pub use pbkdf2_hasher::{
    PBKDF2_DEFAULT_KEY_LENGTH, PBKDF2_DEFAULT_PRF_NAME, PBKDF2_DEFAULT_WORK_FACTOR,
    PBKDF2_FUNCTION_ID, PBKDF2_SALT_LEN_BYTES, Pbkdf2,
};

pub use phcparser::Phc;

use mm_model::user::{PasswordHashError, USER_PASSWORD_MAX_LENGTH, UserPasswordHasher};

/// Port of `hashers.PasswordMaxLengthBytes` (hashers.go:78), which is `model.UserPasswordMaxLength`.
///
/// Aliased rather than re-transcribed, so the two cannot drift — the same treatment
/// `status::STATUS_CACHE_SIZE` gives `session::SESSION_CACHE_SIZE` (see [D-005]).
///
/// 72 is **bcrypt's** block limit. PBKDF2 has no such constraint and inherits the cap only
/// because this package applies one rule to every hasher — which is why a port that reasoned
/// from the algorithm rather than from the package would leave it off the PBKDF2 path.
pub const PASSWORD_MAX_LENGTH_BYTES: usize = USER_PASSWORD_MAX_LENGTH;

/// Port of `hashers.GetLatestHasher` (hashers_production.go:9).
///
/// Go has two build-tagged definitions: the `production` one returns `latestHasher`, and the
/// default one first consults a `testHasher` that `SetTestHasher` can install to make test suites
/// faster. The override is test-only scaffolding rather than server behaviour, so only the
/// production definition is reproduced; a Rust test wanting a cheap hasher constructs
/// [`Pbkdf2::new`] with a small work factor directly, which is what `FastTestHasher` does anyway.
pub fn latest_hasher() -> Pbkdf2 {
    Pbkdf2::default_params()
}

/// Port of `hashers.Hash` (hashers.go:135) — hash with the latest method.
pub fn hash(password: &str) -> Result<String, PasswordHashError> {
    latest_hasher().hash(password)
}

/// Port of `hashers.ErrPasswordTooLong` (hashers.go:87).
///
/// **Not** `model.ErrPasswordTooLong` — Go wraps it:
///
/// ```text
/// ErrPasswordTooLong = fmt.Errorf("hashers: %w", model.ErrPasswordTooLong)
/// ```
///
/// so the text a client sees carries a `hashers: ` prefix while `errors.Is` still finds the
/// sentinel underneath. `User::pre_save` folds this text into an `AppError`'s `detailed_error`,
/// so the prefix is wire-visible; returning the bare sentinel would drop nine bytes from a 400.
fn err_password_too_long() -> PasswordHashError {
    PasswordHashError::TooLong.wrap("hashers")
}

/// The length check both hashers apply before doing any work.
///
/// Counted in **bytes**, not runes: a 20-character emoji password is 80 bytes and is rejected.
fn check_length(password: &str) -> Result<(), PasswordHashError> {
    if password.len() > PASSWORD_MAX_LENGTH_BYTES {
        return Err(err_password_too_long());
    }
    Ok(())
}

/// Port of `hashers.ErrMismatchedHashAndPassword` (hashers.go:91) and the other verification
/// failures.
///
/// Kept distinct from [`PasswordHashError`] because the two directions fail differently: hashing
/// can only refuse an over-long password, while verification can also be handed a stored value it
/// cannot interpret. Collapsing them would let a caller treat "this row is corrupt" as "wrong
/// password", which is the difference between a 500 and a 401.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    /// The password does not match the stored hash. The **only** variant a login route should
    /// turn into a 401.
    #[error("hash and password do not match")]
    Mismatched,

    /// The password exceeded [`PASSWORD_MAX_LENGTH_BYTES`].
    ///
    /// Checked on the compare path too, and that is load-bearing: bcrypt's primitive silently
    /// truncates at 72 bytes, so without this a 73-byte password would verify against a hash of
    /// its first 72. See `bcrypt_truncation` in the oracle.
    #[error("{0}")]
    TooLong(#[source] PasswordHashError),

    /// The stored value could not be interpreted — a bad salt encoding, or parameters that do not
    /// match the hasher being asked.
    #[error("{0}")]
    Other(String),
}

/// Port of `hashers.PasswordHasher` (hashers.go:56), as far as verification needs it.
///
/// An enum rather than a trait object because Go's interface is closed in practice — `hashers`
/// exports exactly two implementations and `GetHasherFromPHCString` enumerates them — and because
/// [`is_latest_hasher`] compares hashers by value, which `dyn Trait` cannot do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hasher {
    BCrypt(BCrypt),
    Pbkdf2(Pbkdf2),
}

impl Hasher {
    /// Port of `PasswordHasher.Hash`.
    pub fn hash(&self, password: &str) -> Result<String, PasswordHashError> {
        match self {
            Self::BCrypt(h) => h.hash(password),
            Self::Pbkdf2(h) => h.hash(password),
        }
    }

    /// Port of `PasswordHasher.CompareHashAndPassword`.
    pub fn compare_hash_and_password(&self, phc: &Phc, password: &str) -> Result<(), CompareError> {
        match self {
            Self::BCrypt(h) => h.compare_hash_and_password(phc, password),
            Self::Pbkdf2(h) => h.compare_hash_and_password(phc, password),
        }
    }

    /// Port of `PasswordHasher.IsPHCValid`.
    pub fn is_phc_valid(&self, phc: &Phc) -> bool {
        match self {
            Self::BCrypt(h) => h.is_phc_valid(phc),
            Self::Pbkdf2(h) => h.is_phc_valid(phc),
        }
    }
}

/// Port of `hashers.getOriginalHasher` (hashers.go:97).
///
/// bcrypt is "somewhat of an edge case": it is not PHC-compliant, so the **whole stored string**
/// goes into the `hash` field and every other field stays empty. A port that tried to split it
/// into salt and hash would be inventing structure the format does not have.
fn get_original_hasher(phc_string: &str) -> (Hasher, Phc) {
    (
        Hasher::BCrypt(BCrypt::new()),
        Phc {
            hash: phc_string.to_owned(),
            ..Phc::default()
        },
    )
}

/// Port of `hashers.GetHasherFromPHCString` (hashers.go:108).
///
/// See the module docs: a parse failure is the bcrypt-detection mechanism, not an error, and an
/// unknown function id falls back to bcrypt even when the string parsed cleanly. The one genuine
/// error is a `$pbkdf2$` string whose parameters will not reconstruct a hasher.
pub fn get_hasher_from_phc_string(phc_string: &str) -> Result<(Hasher, Phc), CompareError> {
    let phc = match phcparser::parse(phc_string) {
        Ok(phc) => phc,
        // Not PHC — a legacy bcrypt row.
        Err(_) => return Ok(get_original_hasher(phc_string)),
    };

    // Check the latest hasher first, so a current row skips the reconstruction below.
    if latest_hasher().is_phc_valid(&phc) {
        return Ok((Hasher::Pbkdf2(latest_hasher()), phc));
    }

    if phc.id == PBKDF2_FUNCTION_ID {
        let hasher = Pbkdf2::from_phc(&phc).map_err(|e| {
            CompareError::Other(format!(
                "the provided PHC string is PBKDF2, but is not valid: {e}"
            ))
        })?;
        return Ok((Hasher::Pbkdf2(hasher), phc));
    }

    Ok(get_original_hasher(phc_string))
}

/// Port of `hashers.CompareHashAndPassword` (hashers.go:141) — verify with the latest method.
pub fn compare_hash_and_password(phc: &Phc, password: &str) -> Result<(), CompareError> {
    latest_hasher().compare_hash_and_password(phc, password)
}

/// Port of `hashers.IsLatestHasher` (hashers.go:147).
///
/// The predicate `App.migratePassword` branches on: a stored row hashed with anything else is
/// re-hashed on the next successful login. Nothing calls it yet — the login route is unported —
/// but it is the reason [`Pbkdf2`] compares by value rather than by identity.
pub fn is_latest_hasher(hasher: &Hasher) -> bool {
    matches!(hasher, Hasher::Pbkdf2(h) if *h == latest_hasher())
}

/// Assert a hashing failure is Go's `hashers.ErrPasswordTooLong` — both halves.
///
/// `errors.Is` must find the sentinel (that is what routes `PreSave` to a 400), **and** the text
/// must carry the `hashers: ` prefix Go's wrapper adds, because it reaches the client inside the
/// `AppError`'s `detailed_error`. Checking only the first would have missed the prefix; checking
/// only the second would not prove the branch.
#[cfg(test)]
pub(crate) fn assert_too_long(got: Result<String, PasswordHashError>) {
    let err = got.expect_err("expected a too-long failure");
    assert!(
        err.is_too_long(),
        "errors.Is(err, ErrPasswordTooLong): {err}"
    );
    assert_eq!(
        err.to_string(),
        "hashers: password too long; maximum length in bytes: 72"
    );
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;
    use std::sync::OnceLock;

    pub(super) fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../../fixtures/behaviour_password.json");
            serde_json::from_str(raw).expect("behaviour_password.json parses")
        })
    }

    /// The finding that reframed [D-108]: `User.PreSave` is handed PBKDF2, not bcrypt.
    ///
    /// Asserted against the oracle rather than stated in a comment, so if upstream ever swaps the
    /// latest hasher again this test fails instead of the claim quietly going stale.
    #[test]
    fn the_latest_hasher_is_pbkdf2_not_bcrypt() {
        let w = &oracle()["which_hasher_writes"];
        assert_eq!(w["is_pbkdf2"], true);
        assert_eq!(w["is_bcrypt"], false);
        assert_eq!(w["is_latest"], true);
        assert_eq!(w["go_type"], "hashers.PBKDF2");
        assert_eq!(w["prefix"], "$pbkdf2$");

        // And ours agrees: `hash` routes to the PBKDF2 header, not to `$2a$`.
        let ours = hash("hunter2").unwrap();
        assert!(
            ours.starts_with(w["prefix"].as_str().unwrap()),
            "latest_hasher must emit Go's prefix, got {ours}"
        );
    }

    /// bcrypt is still reachable, which is why it is ported despite nothing writing it.
    #[test]
    fn bcrypt_is_still_the_fallback() {
        assert_eq!(
            oracle()["which_hasher_writes"]["bcrypt_is_still_the_fallback"],
            true,
            "a stored hash that is not PHC routes back to bcrypt, so old rows still verify"
        );
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["BCryptCost"], BCRYPT_COST);
        assert_eq!(c["PBKDF2FunctionId"], PBKDF2_FUNCTION_ID);
        assert_eq!(c["PasswordMaxLengthBytes"], PASSWORD_MAX_LENGTH_BYTES);
        assert_eq!(c["UserPasswordMaxLength"], USER_PASSWORD_MAX_LENGTH);
        assert_eq!(
            c["ErrPasswordTooLong"],
            PasswordHashError::TooLong.to_string(),
            "the sentinel's text reaches the wire through AppError::wrap"
        );
    }

    /// The 72-byte cap, applied by **both** hashers and counted in bytes.
    #[test]
    fn both_hashers_reject_the_same_over_long_password() {
        let case = oracle()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "over_72")
            .expect("the over-cap case");
        let pw = case["password"].as_str().unwrap();
        assert_eq!(pw.len() as u64, case["password_bytes"].as_u64().unwrap());
        assert_eq!(case["is_password_too_long"], true);

        assert_too_long(BCrypt::new().hash(pw));
        assert_too_long(latest_hasher().hash(pw));
        // And Go's own text for this case, recorded in the corpus rather than transcribed.
        assert_eq!(
            case["hash_error"].as_str().unwrap(),
            "hashers: password too long; maximum length in bytes: 72"
        );
    }

    /// The cap is bytes, so the multi-byte case is the one that distinguishes it from runes.
    #[test]
    fn the_cap_counts_bytes_not_runes() {
        let case = oracle()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "unicode")
            .expect("the unicode case");
        let pw = case["password"].as_str().unwrap();
        assert_eq!(pw.len() as u64, case["password_bytes"].as_u64().unwrap());
        assert_eq!(
            pw.chars().count() as u64,
            case["password_runes"].as_u64().unwrap()
        );
        assert_ne!(
            case["password_bytes"], case["password_runes"],
            "a corpus where these agreed would not distinguish the two rules"
        );

        // 24 runes of this password is 72 bytes at the boundary and 76 one rune later, so a
        // rune-counting implementation would accept something Go refuses.
        let long = "🔒".repeat(19); // 76 bytes, 19 runes
        assert_eq!(long.len(), 76);
        assert_too_long(hash(&long));
    }
}

#[cfg(test)]
mod verify_go_parity {
    use super::go_parity::oracle;
    use super::*;

    /// Every pinned hash against every candidate password, through **both** hashers.
    ///
    /// The verdicts come from Go's `hashers` package rather than from `x/crypto` — see
    /// `bcrypt_truncation` — so this is the corpus that pins the behaviour a login route must
    /// have. `appended` on the 72-byte password is the case that separates the two layers: the
    /// primitive would accept it, the package refuses it as too long.
    #[test]
    fn compare_matches_go() {
        let rows = oracle()["compare"].as_array().unwrap();
        assert!(rows.len() >= 30, "the corpus should not have shrunk");

        let bc = BCrypt::new();
        let pb = latest_hasher();
        let mut matched = 0;

        for row in rows {
            let hash_name = row["hash_name"].as_str().unwrap();
            let candidate = row["candidate"].as_str().unwrap();
            let label = format!("{hash_name}/{candidate}");
            let password = row["password"].as_str().unwrap();

            let case = oracle()["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == hash_name)
                .unwrap();

            // --- bcrypt, reading only the `hash` field, as Go's does ---------------------------
            let bcrypt_phc = Phc {
                hash: case["bcrypt"].as_str().unwrap().to_owned(),
                ..Phc::default()
            };
            let got = bc.compare_hash_and_password(&bcrypt_phc, password);
            assert_eq!(
                got.is_ok(),
                row["bcrypt_matches"].as_bool().unwrap(),
                "{label}: bcrypt verdict"
            );
            if let Err(e) = got {
                assert_eq!(
                    e.to_string(),
                    row["bcrypt_error"].as_str().unwrap(),
                    "{label}: bcrypt error text"
                );
            }

            // --- PBKDF2, through the real parser ----------------------------------------------
            let pbkdf2_phc = phcparser::parse(case["pbkdf2"].as_str().unwrap()).unwrap();
            let got = pb.compare_hash_and_password(&pbkdf2_phc, password);
            assert_eq!(
                got.is_ok(),
                row["pbkdf2_matches"].as_bool().unwrap(),
                "{label}: pbkdf2 verdict"
            );
            if let Err(e) = got {
                assert_eq!(
                    e.to_string(),
                    row["pbkdf2_error"].as_str().unwrap(),
                    "{label}: pbkdf2 error text"
                );
            }

            if candidate == "correct" {
                assert!(row["bcrypt_matches"].as_bool().unwrap(), "{label}");
                matched += 1;
            }
        }

        assert_eq!(matched, 6, "every hashable case verified its own password");
    }

    /// The one row where bcrypt's two layers disagree, stated on its own.
    #[test]
    fn the_package_refuses_a_73_byte_password_the_primitive_would_accept() {
        let row = oracle()["compare"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["hash_name"] == "exactly_72" && r["candidate"] == "appended")
            .expect("the 72-byte hash against a 73-byte password");

        assert_eq!(row["bcrypt_matches"], false);
        assert_eq!(
            row["bcrypt_error"], "hashers: password too long; maximum length in bytes: 72",
            "the length check fires — it is NOT reported as a mismatch"
        );
        // And the primitive's opposite verdict, from the other oracle section.
        assert_eq!(
            oracle()["bcrypt_truncation"]["x_crypto_accepts_73_byte_password"],
            true,
            "which is exactly what this check exists to override"
        );

        let case = oracle()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "exactly_72")
            .unwrap();
        let phc = Phc {
            hash: case["bcrypt"].as_str().unwrap().to_owned(),
            ..Phc::default()
        };
        let password = format!("{}x", case["password"].as_str().unwrap());
        assert_eq!(password.len(), 73);

        match BCrypt::new().compare_hash_and_password(&phc, &password) {
            Err(CompareError::TooLong(_)) => {}
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    /// `IsPHCValid` — an exact, textual parameter match on both hashers.
    #[test]
    fn is_phc_valid_matches_go() {
        let pb = latest_hasher();
        let bc = BCrypt::new();

        for row in oracle()["is_phc_valid"].as_array().unwrap() {
            let name = row["name"].as_str().unwrap();
            let input = row["input"].as_str().unwrap();

            let parsed = phcparser::parse(input);
            assert_eq!(
                parsed.is_ok(),
                row["parses"].as_bool().unwrap(),
                "{name}: parses"
            );

            let Ok(phc) = parsed else { continue };
            assert_eq!(
                pb.is_phc_valid(&phc),
                row["pbkdf2_is_valid"].as_bool().unwrap(),
                "{name}: pbkdf2"
            );
            assert_eq!(
                bc.is_phc_valid(&phc),
                row["bcrypt_is_valid"].as_bool().unwrap(),
                "{name}: bcrypt — always false, by design"
            );
        }

        // The comparison is textual, so a numerically equal but differently written value fails.
        let padded = phcparser::parse("$pbkdf2$f=SHA256,w=0600000,l=32$c2FsdA$aGFzaA").unwrap();
        assert!(
            !pb.is_phc_valid(&padded),
            "Go compares the parameter STRINGS, so 0600000 != 600000"
        );
    }

    /// The router: which hasher a stored value gets, and what lands in its PHC.
    #[test]
    fn get_hasher_from_phc_string_matches_go() {
        for row in oracle()["router"].as_array().unwrap() {
            let name = row["name"].as_str().unwrap();
            let stored = row["stored"].as_str().unwrap();
            let got = get_hasher_from_phc_string(stored);

            match row["error"].as_str() {
                Some(want) => {
                    let err = got.expect_err(&format!("{name}: Go errored and we did not"));
                    assert_eq!(err.to_string(), want, "{name}: error text");
                }
                None => {
                    let (hasher, phc) =
                        got.unwrap_or_else(|e| panic!("{name}: Go succeeded, we got {e}"));

                    assert_eq!(
                        matches!(hasher, Hasher::BCrypt(_)),
                        row["is_bcrypt"].as_bool().unwrap(),
                        "{name}: routed to bcrypt?"
                    );
                    assert_eq!(
                        matches!(hasher, Hasher::Pbkdf2(_)),
                        row["is_pbkdf2"].as_bool().unwrap(),
                        "{name}: routed to pbkdf2?"
                    );
                    assert_eq!(
                        is_latest_hasher(&hasher),
                        row["is_latest"].as_bool().unwrap(),
                        "{name}: is this the hasher a login would migrate away from?"
                    );
                    assert_eq!(phc.id, row["phc_id"].as_str().unwrap(), "{name}: phc id");
                    assert_eq!(phc.salt, row["phc_salt"].as_str().unwrap(), "{name}: salt");
                    assert_eq!(phc.hash, row["phc_hash"].as_str().unwrap(), "{name}: hash");
                    assert_eq!(
                        phc.hash == stored,
                        row["hash_is_whole_input"].as_bool().unwrap(),
                        "{name}: a bcrypt row keeps the WHOLE stored string as its hash"
                    );
                }
            }
        }
    }

    /// The two routing results a reading gets wrong, stated on their own.
    #[test]
    fn the_router_falls_back_to_bcrypt_more_often_than_it_looks() {
        // A perfectly well-formed argon2id PHC is not PBKDF2, so it lands on bcrypt — parsing
        // successfully is not enough.
        let (hasher, phc) =
            get_hasher_from_phc_string("$argon2id$v=19$m=65536,t=2,p=1$c2FsdA$aGFzaA").unwrap();
        assert!(matches!(hasher, Hasher::BCrypt(_)));
        assert_eq!(phc.hash, "$argon2id$v=19$m=65536,t=2,p=1$c2FsdA$aGFzaA");
        assert_eq!(
            phc.id, "",
            "the parsed PHC is discarded for the bcrypt path"
        );

        // An empty stored password also routes to bcrypt, and then fails to verify — the right
        // answer, reached by a route worth knowing about.
        let (hasher, phc) = get_hasher_from_phc_string("").unwrap();
        assert!(matches!(hasher, Hasher::BCrypt(_)));
        assert!(matches!(
            hasher.compare_hash_and_password(&phc, ""),
            Err(CompareError::Other(_))
        ));

        // But `$pbkdf2` with unusable parameters is a hard error, not a fallback.
        assert!(get_hasher_from_phc_string("$pbkdf2").is_err());
    }

    /// End to end: hash with the latest hasher, then verify through the router.
    ///
    /// The write half and the read half were ported in different sessions against different
    /// oracle sections; this is the only test that runs them against each other.
    #[test]
    fn a_hash_we_write_verifies_through_the_router() {
        let stored = hash("hunter2").expect("hashes");

        let (hasher, phc) = get_hasher_from_phc_string(&stored).expect("routes");
        assert!(is_latest_hasher(&hasher), "a fresh hash needs no migration");
        assert!(hasher.compare_hash_and_password(&phc, "hunter2").is_ok());
        assert!(matches!(
            hasher.compare_hash_and_password(&phc, "hunter3"),
            Err(CompareError::Mismatched)
        ));
    }

    /// And the direction that matters for a shared database: a hash **Go** wrote verifies here.
    #[test]
    fn hashes_go_wrote_verify_here() {
        let mut checked = 0;
        for case in oracle()["cases"].as_array().unwrap() {
            if !case["hashes_ok"].as_bool().unwrap() {
                continue;
            }
            let password = case["password"].as_str().unwrap();

            for stored in [
                case["pbkdf2"].as_str().unwrap(),
                case["bcrypt"].as_str().unwrap(),
            ] {
                let (hasher, phc) = get_hasher_from_phc_string(stored).expect("routes");
                assert!(
                    hasher.compare_hash_and_password(&phc, password).is_ok(),
                    "a row the Go server wrote must verify: {stored}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 12, "six passwords x two hashers");
    }
}
