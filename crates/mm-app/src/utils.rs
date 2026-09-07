//! Port of the pieces of `channels/utils/utils.go` that a migrated route reaches.
//!
//! `channels/utils` is a grab-bag; this is not a port of the file. It holds the one function the
//! usage routes cannot answer without, and it lives in `mm-app` rather than `mm-model` because
//! `server/channels/` is the **AGPL** half of the tree ([D-031]).

/// Port of `utils.RoundOffToZeroesResolution` (utils.go:265).
///
/// Rounds `n` down to `min_resolution` significant trailing zeroes — the deliberate blurring the
/// usage endpoints apply before reporting a count. `GetPostsUsage` passes 3;
/// `getStorageUsage` passes 8.
///
/// # The small-number window returns zero, not the number
///
/// `-9..=9` is answered before any arithmetic, and inside it the answer is `n` **only** when the
/// resolution is 0 — otherwise it is `0`. So a server with seven posts reports zero posts, and
/// that is the function working. A port that treated the window as "too small to round" and
/// returned `n` would be right for the one caller that passes 0 and wrong for both real ones.
///
/// # The resolution is clamped twice, and the second clamp reads the first
///
/// `max(0, min_resolution)` first, so a negative request means "no rounding"; then
/// `min(zeroes, resolution)`, where `zeroes` is the base-10 magnitude of `n`. The magnitude can
/// only lower the resolution, never raise it, which is what stops `1000` at resolution 8 from
/// becoming `0`.
///
/// # `f64::log10` is **not** Go's `math.Log10`, and the corpus proved it
///
/// `zeroes` is `int(math.Log10(math.Abs(n)))` in Go — a truncation of a float, and therefore at
/// the mercy of which side of an integer the logarithm lands on. Go's `math.Log10` is
/// `Log(x) * (1/Ln10)` implemented in pure Go (math/log10.go) over the FreeBSD `e_log.c`
/// polynomial; C's `log10` is a different function. They disagree, measured over the corpus:
///
/// | `n` | Go's `int(Log10)` | Rust `f64::log10` truncated | exact |
/// |---|---|---|---|
/// | `999_999_999_999_999` | 14 | **15** | 14 |
/// | `1_000_000_000_000_000` | **14** | 15 | 15 |
///
/// So neither float is reliable and they fail in opposite directions. This port therefore
/// computes the magnitude **exactly**, by counting decimal places — see [`decimal_magnitude`] —
/// and the whole 954-row corpus is what says that is safe rather than merely tidier.
///
/// It is safe for a reason worth stating, because it is not obvious: the magnitude is used only
/// as `min(zeroes, resolution)`, and the two real call sites pass resolutions **3** and **8**.
/// Every input where Go's float is off by one is at least 10^15, where the `min` clamps to the
/// resolution regardless — so an off-by-one in `zeroes` cannot reach the answer. The corpus
/// exercises resolution 20 as well, which *can* see the difference, and the results still agree:
/// at `n = 10^15` Go computes `10 * 10^14` and this computes `1 * 10^15`.
///
/// The `as` casts are the same truncation-toward-zero Go's `int()` and `int64()` do.
///
/// The parameter is `f64` because Go's is, and both callers hand it an `i64` count that has
/// already been widened. Above 2^53 that widening is lossy on both servers identically, so the
/// signature is kept rather than "improved" to `i64`.
pub fn round_off_to_zeroes_resolution(n: f64, min_resolution: i32) -> i64 {
    let mut resolution = min_resolution.max(0);
    if (-9.0..=9.0).contains(&n) {
        if resolution == 0 {
            return n as i64;
        }
        return 0;
    }

    let zeroes = decimal_magnitude(n.abs());
    resolution = zeroes.min(resolution);
    let tens = 10f64.powi(resolution) as i64;
    let significant_digits = (n as i64) / tens;
    significant_digits * tens
}

/// `int(math.Log10(|n|))` without the float — the count of decimal places below `x`.
///
/// Returns the largest `k` with `10^k <= x`, for the `x > 9` this is only ever called with.
///
/// The loop stops at 22 because `1e22` is the largest power of ten exactly representable as an
/// `f64`; beyond it `bound *= 10.0` would start rounding and the comparison would be a guess.
/// Nothing can reach that: both callers widen an `i64`, whose magnitude tops out below `1e19`.
/// Go has no such bound and no such guarantee — its `Log10` simply gets less accurate — so this
/// is the one place the port is deliberately *more* defined than the original.
fn decimal_magnitude(x: f64) -> i32 {
    let mut zeroes = 0;
    let mut bound = 10.0f64;
    while zeroes < 22 && x >= bound {
        zeroes += 1;
        bound *= 10.0;
    }
    zeroes
}

#[cfg(test)]
mod go_parity {
    use super::*;

    /// Every row of the corpus `reference/dump/behaviour_round_off.go` produced, against the real
    /// `math.Log10`/`math.Pow10`.
    ///
    /// This is the whole test. The branches are cheap to state and expensive to get right, and
    /// the one that would actually bite — the `int(log10(n))` truncation at a power of ten — is
    /// invisible to any assertion a reader writes from the source.
    #[test]
    fn every_corpus_row_matches_go() {
        #[derive(serde::Deserialize)]
        struct Row {
            n: i64,
            min_resolution: i32,
            result: i64,
        }
        #[derive(serde::Deserialize)]
        struct Corpus {
            round_off_to_zeroes_resolution: Vec<Row>,
        }

        let corpus: Corpus =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_round_off.json"))
                .expect("behaviour_round_off.json is generated by reference/dump");
        assert!(
            corpus.round_off_to_zeroes_resolution.len() > 200,
            "the corpus is the test; an empty one passes vacuously"
        );

        for row in &corpus.round_off_to_zeroes_resolution {
            assert_eq!(
                round_off_to_zeroes_resolution(row.n as f64, row.min_resolution),
                row.result,
                "round_off_to_zeroes_resolution({}, {})",
                row.n,
                row.min_resolution
            );
        }
    }

    /// **The divergence itself, pinned.** Every input where Go's float logarithm disagrees with
    /// the exact magnitude, and no others.
    ///
    /// This is not a test of the port — [`every_corpus_row_matches_go`] is that. It is a test of
    /// the *reasoning* that lets the port use exact arithmetic: the claim is "Go is off by one
    /// only at magnitudes far above any resolution the callers pass", and if a Go release changed
    /// `math.Log10` so that it were off by one at, say, 10^8, this would fail and say so — where
    /// the result-level test might not, because the two errors could still cancel.
    #[test]
    fn go_s_float_logarithm_is_off_by_one_at_exactly_two_magnitudes() {
        #[derive(serde::Deserialize)]
        struct Magnitude {
            n: i64,
            zeroes: i32,
        }
        #[derive(serde::Deserialize)]
        struct Corpus {
            log10_magnitude: Vec<Magnitude>,
        }

        let corpus: Corpus =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_round_off.json"))
                .expect("behaviour_round_off.json is generated by reference/dump");

        let mut disagreements: Vec<i64> = corpus
            .log10_magnitude
            .iter()
            .filter(|row| decimal_magnitude((row.n as f64).abs()) != row.zeroes)
            .map(|row| row.n)
            .collect();
        disagreements.sort_unstable();

        assert_eq!(
            disagreements,
            vec![
                -1_000_000_000_000_001,
                -1_000_000_000_000_000,
                1_000_000_000_000_000,
                1_000_000_000_000_001,
            ],
            "Go's Log10 disagrees with the exact magnitude only at 10^15 and its successor; \
             every other input in the corpus agrees, and 15 is far above the resolutions 3 and 8 \
             that the two callers pass"
        );
        for n in &disagreements {
            assert!(
                n.abs() >= 1_000_000_000_000_000,
                "a disagreement below 10^15 would be reachable through the storage route's \
                 resolution of 8: {n}"
            );
        }
    }

    /// The three claims the doc comment makes, stated as assertions so a reader who changes the
    /// function is told which sentence they falsified — the corpus alone says only "row 118".
    #[test]
    fn the_documented_branches_are_the_real_ones() {
        assert_eq!(
            round_off_to_zeroes_resolution(7.0, 3),
            0,
            "inside the window a non-zero resolution answers 0, not n"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(7.0, 0),
            7,
            "and resolution 0 is the only way to get n back"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(99.0, 8),
            90,
            "the magnitude clamps the requested resolution down"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(1234.0, -1),
            1234,
            "a negative resolution is clamped to 0, which rounds nothing"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(-12345.0, 3),
            -12000,
            "integer division truncates toward zero, so a negative rounds up in value"
        );
    }
}
