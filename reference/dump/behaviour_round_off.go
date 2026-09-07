package main

// Behavioural oracle for `utils.RoundOffToZeroesResolution` (channels/utils/utils.go:265),
// written to fixtures/behaviour_round_off.json.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-app`'s tests and never
// `mm-model`'s — the same rule behaviour_password.go and behaviour_subpath.go state ([D-031]).
//
// # Why a nine-line function needs an oracle
//
// It is the only thing standing between `GET /api/v4/usage/posts` and a wrong number, and every
// line of it is a place a reader guesses:
//
//  1. **The small-number window is inclusive and asymmetric in effect.** `n >= -9 && n <= 9`
//     returns `int64(n)` when the resolution is 0 and **0** otherwise — so a server with 7 posts
//     reports 0, not 7. That is not a rounding artefact; it is a different branch.
//  2. **`resolution` is clamped twice, against different things.** `max(0, minResolution)` first,
//     then `min(zeroes, resolution)` where `zeroes = int(math.Log10(math.Abs(n)))`. So the
//     requested resolution is an upper bound that the magnitude of `n` can lower, and the second
//     clamp reads the *already clamped* value.
//  3. **`int(math.Log10(...))` truncates toward zero, on a float.** `Log10(1000)` is not exactly
//     3 on every input, and `int()` of `2.9999999999999996` is 2 — a whole order of magnitude.
//     This is the single most likely divergence between a Go `float64` and a Rust `f64`, which is
//     why the corpus walks every power of ten and its neighbours.
//  4. **The truncation is integer division on the truncated float.** `int64(n) / tens * tens`
//     truncates toward zero, so a negative count rounds *up* in value.
//
// The two call sites disagree about the resolution, which is why the corpus records both:
// `GetPostsUsage` passes 3, `getStorageUsage` passes 8.
//
// # The function body is transcribed, and that is deliberate
//
// Importing `channels/utils` drags in goldmark (utils/markdown.go), which is not in this
// generator's go.sum. So `roundOffToZeroesResolution` below is utils.go:265-279 copied line for
// line, calling Go's **real** `math.Log10`, `math.Abs` and `math.Pow10` — the ingredients that
// actually decide the answer are Go's own. Same standing rule as the other transcriptions:
// **copy any upstream change character for character.**

import (
	"encoding/json"
	"errors"
	"math"
	"os"
	"path/filepath"
)

// roundOffToZeroesResolution is channels/utils/utils.go:265-279, transcribed.
func roundOffToZeroesResolution(n float64, minResolution int) int64 {
	resolution := max(0, minResolution)
	if n >= -9 && n <= 9 {
		if resolution == 0 {
			return int64(n)
		}
		return 0
	}

	zeroes := int(math.Log10(math.Abs(n)))
	resolution = min(zeroes, resolution)
	tens := int64(math.Pow10(resolution))
	significantDigits := int64(n) / tens
	return significantDigits * tens
}

// roundOffCorpus is every input class the two call sites can produce, plus the ones that decide
// the branches above.
//
// The counts are `int64`s in the caller — `AnalyticsPostCount` and `GetStorageUsage` both return
// one — so they are whole numbers here even though the parameter is `float64`. The powers of ten
// and their immediate neighbours are the `math.Log10` truncation probes.
//
// Every power of ten in `int64` range appears with both neighbours, because that is exactly where
// a `log10` implementation can land on the wrong side of an integer.
var roundOffCorpus = []float64{
	0, 1, 5, 9, 10, 11, 99, 100, 101, 999, 1000, 1001, 1099, 1100,
	9999, 10000, 10001, 99999, 100000, 100001,
	999999, 1000000, 1000001, 12345678, 99999999, 100000000, 100000001,
	123456789012, 999999999999999, 1000000000000000,
	// The negatives: unreachable from a COUNT(*) or a SUM of file sizes, but the small-number
	// window is written `n >= -9` and the integer division truncates toward zero, so a port that
	// used a floor division would differ here and nowhere else.
	-1, -9, -10, -11, -99, -100, -101, -1001, -12345,
	// A file-size total large enough to exercise resolution 8 fully.
	5200, 1234567890, 987654321098765,
}

// powersOfTenSweep is every 10^k in int64 range with its two neighbours, positive and negative.
//
// This is the part of the corpus that earns its keep. `int(math.Log10(|n|))` is a truncation of a
// float, and the only inputs where a truncation can go wrong are the ones whose logarithm is
// within a rounding of an integer — which is precisely 10^k and 10^k-1. The sweep found the real
// divergence documented at the top of this file; without it the corpus agreed with C's `log10`
// by luck.
func powersOfTenSweep() []float64 {
	out := []float64{}
	n := int64(1)
	for k := 0; k <= 18; k++ {
		for _, v := range []int64{n - 1, n, n + 1} {
			out = append(out, float64(v), float64(-v))
		}
		if k == 18 {
			break
		}
		n *= 10
	}
	// The extremes, where float64 can no longer represent consecutive integers at all.
	out = append(out, float64(int64(1)<<53), float64(int64(1)<<53+1), 9223372036854775807)
	return out
}

// roundOffResolutions are the values the tree actually passes: 3 from `GetPostsUsage`
// (app/usage.go:21) and 8 from `getStorageUsage` (api4/usage.go:47), plus 0 — the only value that
// makes the small-number window return `n` rather than 0 — and a resolution larger than any
// magnitude in the corpus, which is what the `min(zeroes, resolution)` clamp exists for.
var roundOffResolutions = []int{0, 1, 3, 8, 20, -1}

func writeRoundOffBehaviourFixture(outDir string) error {
	corpus := append(append([]float64{}, roundOffCorpus...), powersOfTenSweep()...)

	rows := make([]map[string]any, 0, len(corpus)*len(roundOffResolutions))
	for _, resolution := range roundOffResolutions {
		for _, n := range corpus {
			rows = append(rows, map[string]any{
				"n":              int64(n),
				"min_resolution": resolution,
				"result":         roundOffToZeroesResolution(n, resolution),
			})
		}
	}

	// `zeroes` on its own, for every magnitude in the corpus. The Rust port derives this from the
	// decimal digits rather than from a float logarithm — see the note at the top — and this
	// column is what proves the two agree, input by input, rather than in principle.
	magnitudes := make([]map[string]any, 0, len(corpus))
	seen := map[int64]bool{}
	for _, n := range corpus {
		if n >= -9 && n <= 9 {
			continue // `Log10` is never reached inside the small-number window.
		}
		if seen[int64(n)] {
			continue
		}
		seen[int64(n)] = true
		magnitudes = append(magnitudes, map[string]any{
			"n":      int64(n),
			"zeroes": int(math.Log10(math.Abs(n))),
		})
	}

	// Guard the transcription. `math.Log10` and `math.Pow10` are Go's, but the nine lines around
	// them are not, so assert the claims the doc comment above makes outright.
	if roundOffToZeroesResolution(7, 3) != 0 {
		return errors.New("transcription drift: a count inside the small window must be 0")
	}
	if roundOffToZeroesResolution(7, 0) != 7 {
		return errors.New("transcription drift: resolution 0 must return n inside the window")
	}
	if roundOffToZeroesResolution(12345, 3) != 12000 {
		return errors.New("transcription drift: resolution 3 must keep three trailing zeroes")
	}
	if roundOffToZeroesResolution(99, 8) != 90 {
		return errors.New("transcription drift: the magnitude clamps the resolution down")
	}

	out := map[string]any{
		"round_off_to_zeroes_resolution": rows,
		"log10_magnitude":                magnitudes,
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_round_off.json"), append(blob, '\n'), 0o644)
}
