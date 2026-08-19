package main

// Behavioural oracle for channels/app/password/hashers, written to fixtures/behaviour_password.json.
//
// This is the [D-108] oracle, and the first fixture generated from the **AGPL** half of the Go
// tree rather than from `server/public`. It therefore feeds `mm-app`'s tests, never `mm-model`'s:
// `mm-model` is Apache-2.0 and may not derive from `server/channels/` ([D-031]).
//
// # The headline: bcrypt is NOT what a Mattermost server writes
//
// [D-108] was raised saying "Go uses golang.org/x/crypto/bcrypt". At the pinned SHA that is the
// **legacy** path. hashers.go:
//
//	latestHasher PasswordHasher = DefaultPBKDF2()
//
// and channels/store/sqlstore/user_store.go:180 is the only caller of `User.PreSave`:
//
//	if err := user.PreSave(hashers.GetLatestHasher()); err != nil {
//
// So every password the Go server writes to `Users.Password` today is a PBKDF2 PHC string. bcrypt
// survives only as the fallback `GetHasherFromPHCString` returns when a stored hash does not parse
// as PHC — i.e. for rows written before the migration. A Rust server that hashed with bcrypt would
// still *authenticate* (Go would route those rows back to bcrypt), but it would be writing the
// superseded format into a column the Go server is actively migrating away from.
//
// Both are therefore pinned here. The oracle records the fixed format string of each.
//
// # Why the hashes are constants rather than freshly generated
//
// Both hashers take a random salt, so `Hash` is nondeterministic and a fixture that called it
// would be rewritten by every generator run — the defect [D-032] was raised for. So the corpus
// holds Go-produced hashes as **literals**, captured once from this exact package.
//
// A pasted literal is normally the guessing the oracle exists to prevent, so the generator
// **verifies every one of them with Go before writing**, and fails the run if any disagrees:
//
//   - `bcrypt.CompareHashAndPassword` must accept the pinned hash for its password, and reject it
//     for a different one;
//   - `bcrypt.Cost` must report 10;
//   - the PBKDF2 PHC must parse, satisfy `IsPHCValid` for the default parameters, and recompute to
//     the same digest from its own embedded salt.
//
// A mistyped character in any literal fails the generator, not a downstream Rust test.
//
// # What the Rust side can do with this
//
// Both algorithms are deterministic **given the salt**, and both formats carry their salt. So the
// Rust tests decode the salt out of Go's pinned hash, recompute, and assert byte-equality against
// the whole Go string. That pins the write direction exactly — not merely "Rust can read Go", but
// "Rust emits Go's bytes" — without needing the two runtimes in one process.
//
// # The password cap belongs to the hasher, not to the caller
//
// Both hashers reject > `model.UserPasswordMaxLength` (72) bytes with a wrapped
// `model.ErrPasswordTooLong`, and `User.PreSave` turns that into a distinct AppError id. 72 is
// bcrypt's own limit leaking into PBKDF2, which has no such constraint — recorded because a port
// would otherwise have no reason to enforce it on the PBKDF2 path.
//
// Determinism: fixed literals and fixed passwords only. No rand, no clock.

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/app/password/hashers"
	"github.com/mattermost/mattermost/server/v8/channels/app/password/phcparser"
	"golang.org/x/crypto/bcrypt"
)

// pinnedHash is one (password, Go output) pair, captured once from this package and re-verified
// on every generator run.
type pinnedHash struct {
	name   string
	pw     string
	bcrypt string // "" when Go refused to hash
	pbkdf2 string // "" when Go refused to hash
	errMsg string // Go's error text, "" when it succeeded
}

// The corpus. Every value below was produced by this package and is checked before it is written.
//
// The password set is chosen for the edges a hasher gets wrong: empty, an embedded NUL (which
// several bcrypt implementations truncate at and Go's does not), multi-byte UTF-8, and both sides
// of the 72-byte cap counted in BYTES rather than runes.
var pinnedHashes = []pinnedHash{
	{
		name:   "empty",
		pw:     "",
		bcrypt: "$2a$10$gSZylAupRaDSbThPRdNHa.a91BqVuwn.7B57P60bCRGYhXZtYfOCK",
		pbkdf2: "$pbkdf2$f=SHA256,w=600000,l=32$4EJ0aoqSHZt0dKO8ccz/OQ$oWnpQnGjl+x+6J08bTP1OVQ+7ZF29qMaucfyyDY0Rlw",
	},
	{
		name:   "ascii",
		pw:     "hunter2",
		bcrypt: "$2a$10$WDhgp5OhsT82IMGfMVI2VO5gMvw9t9GsLXDplXC..ugfK3oniyuYe",
		pbkdf2: "$pbkdf2$f=SHA256,w=600000,l=32$2H1h37Jzrfgk/ZZxlkosvA$lkERvYQfFUbBycl1MnNSkk4wu+SyoC35oek0zGow8dk",
	},
	{
		name:   "phrase",
		pw:     "correct horse battery staple",
		bcrypt: "$2a$10$N5yClp8t2C1ShU8819n0xuIt78oqLCKdKlesNgO78uL/qu88mJcR2",
		pbkdf2: "$pbkdf2$f=SHA256,w=600000,l=32$fyyUcl8vodqQnWo2LsarEg$zUX59NgQazmB/nnwXB6x+0vdXDEb15YgqQuCyxoN/lg",
	},
	{
		// 14 bytes, 9 runes — so a cap counted in runes would let a longer one through.
		name:   "unicode",
		pw:     "pässwörd\U0001f512",
		bcrypt: "$2a$10$vcPO5OqfPr/x6T6yV7MBk.vp35z9v.d.hNnYImednh3.UVLrEDSP.",
		pbkdf2: "$pbkdf2$f=SHA256,w=600000,l=32$CNkzsnsZnDfdZv8dH2yg3w$6xjgUdYO3F1HKxbuFLSwmg5Jd2aG6VG5QYUQ7+hdDYU",
	},
	{
		// Go's bcrypt hashes the whole 5 bytes. A C implementation using strlen() would hash "ab"
		// and quietly accept any password starting "ab" — the classic NUL-truncation bug.
		name:   "embedded_nul",
		pw:     "ab\x00cd",
		bcrypt: "$2a$10$BHL.sP5nZmM4PnM5U5S9KOpomOriMCGcuE2iHSBZmLdjlfRTH6sMC",
		pbkdf2: "$pbkdf2$f=SHA256,w=600000,l=32$rVBzJRTDqtNNGuSmq2Yk0g$abGOhpO3f6sIfe+0W5er88RTpyPsqXKr0lbLATRdUIM",
	},
	{
		name:   "exactly_72",
		pw:     "a123456789b123456789c123456789d123456789e123456789f123456789g123456789h1",
		bcrypt: "$2a$10$6YZg02vi4qe/edWiyw6jmuaN6wPlKiUK2gBFRRysgWft2jVxhxeNO",
		pbkdf2: "$pbkdf2$f=SHA256,w=600000,l=32$e4UZkOfJ7HB/hw3/Xugcfg$Ty/W+x4Yg+SIhGG0UCZDdWeCsmTQy8vRBZSA4FqvQhI",
	},
	{
		// One byte over. BOTH hashers refuse — PBKDF2 has no intrinsic limit and enforces this
		// one only because the package applies bcrypt's to everything.
		name:   "over_72",
		pw:     "a123456789b123456789c123456789d123456789e123456789f123456789g123456789h12",
		errMsg: "hashers: password too long; maximum length in bytes: 72",
	},
}

func writePasswordBehaviourFixture(outDir string) error {
	bc := hashers.NewBCrypt()
	pb := hashers.DefaultPBKDF2()

	cases := make([]map[string]any, 0, len(pinnedHashes))
	for _, c := range pinnedHashes {
		entry := map[string]any{
			"name":                 c.name,
			"password":             c.pw,
			"password_bytes":       len(c.pw),
			"password_runes":       len([]rune(c.pw)),
			"bcrypt":               c.bcrypt,
			"pbkdf2":               c.pbkdf2,
			"hashes_ok":            c.errMsg == "",
			"hash_error":           c.errMsg,
			"is_password_too_long": false,
		}

		if c.errMsg != "" {
			// Verify Go really does refuse, with that exact text, through BOTH hashers.
			if _, err := bc.Hash(c.pw); err == nil || err.Error() != c.errMsg {
				return fmt.Errorf("password %q: bcrypt error is %v, pinned %q", c.name, err, c.errMsg)
			}
			ptooLong := false
			if _, err := pb.Hash(c.pw); err == nil || err.Error() != c.errMsg {
				return fmt.Errorf("password %q: pbkdf2 error is %v, pinned %q", c.name, err, c.errMsg)
			} else {
				ptooLong = errors.Is(err, model.ErrPasswordTooLong)
			}
			entry["is_password_too_long"] = ptooLong
			cases = append(cases, entry)
			continue
		}

		// --- verify the pinned bcrypt literal --------------------------------------------------
		if err := bcrypt.CompareHashAndPassword([]byte(c.bcrypt), []byte(c.pw)); err != nil {
			return fmt.Errorf("password %q: pinned bcrypt hash does not verify: %w", c.name, err)
		}
		if err := bcrypt.CompareHashAndPassword([]byte(c.bcrypt), []byte(wrongPassword(c.pw))); err == nil {
			return fmt.Errorf("password %q: pinned bcrypt hash verifies a DIFFERENT password", c.name)
		}
		cost, err := bcrypt.Cost([]byte(c.bcrypt))
		if err != nil {
			return fmt.Errorf("password %q: pinned bcrypt hash has no readable cost: %w", c.name, err)
		}
		if cost != hashers.BCryptCost {
			return fmt.Errorf("password %q: pinned bcrypt cost is %d, package uses %d",
				c.name, cost, hashers.BCryptCost)
		}
		entry["bcrypt_cost"] = cost
		entry["bcrypt_len"] = len(c.bcrypt)
		// The 22-char base64 salt, sliced out at the fixed offset the format guarantees. Recorded
		// so the Rust test can recompute from it rather than re-deriving the offset itself.
		entry["bcrypt_salt_b64"] = c.bcrypt[7:29]

		// --- verify the pinned PBKDF2 literal --------------------------------------------------
		phc, err := phcparser.New(strings.NewReader(c.pbkdf2)).Parse()
		if err != nil {
			return fmt.Errorf("password %q: pinned pbkdf2 hash does not parse as PHC: %w", c.name, err)
		}
		if !pb.IsPHCValid(phc) {
			return fmt.Errorf("password %q: pinned pbkdf2 hash does not match the default parameters", c.name)
		}
		if err := pb.CompareHashAndPassword(phc, c.pw); err != nil {
			return fmt.Errorf("password %q: pinned pbkdf2 hash does not verify: %w", c.name, err)
		}
		if err := pb.CompareHashAndPassword(phc, wrongPassword(c.pw)); err == nil {
			return fmt.Errorf("password %q: pinned pbkdf2 hash verifies a DIFFERENT password", c.name)
		}
		salt, err := base64.RawStdEncoding.DecodeString(phc.Salt)
		if err != nil {
			return fmt.Errorf("password %q: pinned pbkdf2 salt is not raw-std base64: %w", c.name, err)
		}
		digest, err := base64.RawStdEncoding.DecodeString(phc.Hash)
		if err != nil {
			return fmt.Errorf("password %q: pinned pbkdf2 digest is not raw-std base64: %w", c.name, err)
		}
		entry["pbkdf2_id"] = phc.Id
		entry["pbkdf2_params"] = phc.Params
		entry["pbkdf2_salt_b64"] = phc.Salt
		entry["pbkdf2_hash_b64"] = phc.Hash
		entry["pbkdf2_salt_bytes"] = len(salt)
		entry["pbkdf2_hash_bytes"] = len(digest)

		cases = append(cases, entry)
	}

	out := map[string]any{
		"which_hasher_writes": whichHasherWrites(),
		"bcrypt_format":       bcryptFormat(),
		"bcrypt_truncation":   bcryptTruncation(pb),
		"pbkdf2_format":       pbkdf2Format(pb),
		"cases":               cases,
		"compare":             passwordCompareAll(),
		"is_phc_valid":        passwordIsPHCValidAll(pb, bc),
		"router":              passwordRouterAll(),
		"constants": map[string]any{
			"BCryptCost":             hashers.BCryptCost,
			"PBKDF2FunctionId":       hashers.PBKDF2FunctionId,
			"PasswordMaxLengthBytes": hashers.PasswordMaxLengthBytes,
			"UserPasswordMaxLength":  model.UserPasswordMaxLength,
			"ErrPasswordTooLong":     model.ErrPasswordTooLong.Error(),
		},
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_password.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

// passwordCompareAll drives every pinned hash against a set of candidate passwords, through the
// PACKAGE's CompareHashAndPassword rather than the underlying crypto.
//
// The distinction is the point. `x/crypto/bcrypt` truncates at 72 bytes and will accept a longer
// password; `hashers.BCrypt.CompareHashAndPassword` re-applies the length check first. A port
// written against the crate would authenticate a login the Go server denies — a security
// difference, so the verdicts are recorded per candidate rather than asserted in a comment.
//
// The candidate set is chosen so each hash sees: the right password, a same-length wrong one, an
// appended one (which bcrypt's primitive would accept at the cap), a truncated one, and an
// over-long one.
func passwordCompareAll() []map[string]any {
	bc := hashers.NewBCrypt()
	pb := hashers.DefaultPBKDF2()

	var out []map[string]any
	for _, c := range pinnedHashes {
		if c.errMsg != "" {
			continue
		}
		candidates := []struct {
			label string
			pw    string
		}{
			{"correct", c.pw},
			{"wrong_same_length", wrongPassword(c.pw)},
			{"appended", c.pw + "x"},
			{"empty", ""},
			{"over_72", strings.Repeat("a", 73)},
		}
		if len(c.pw) > 1 {
			candidates = append(candidates, struct {
				label string
				pw    string
			}{"truncated", c.pw[:len(c.pw)-1]})
		}

		for _, cand := range candidates {
			bcErr := bc.CompareHashAndPassword(phcparser.PHC{Hash: c.bcrypt}, cand.pw)

			pbPHC, parseErr := phcparser.New(strings.NewReader(c.pbkdf2)).Parse()
			var pbErr error
			if parseErr != nil {
				pbErr = parseErr
			} else {
				pbErr = pb.CompareHashAndPassword(pbPHC, cand.pw)
			}

			out = append(out, map[string]any{
				"hash_name":      c.name,
				"candidate":      cand.label,
				"password":       cand.pw,
				"bcrypt_matches": bcErr == nil,
				"bcrypt_error":   errText(bcErr),
				"pbkdf2_matches": pbErr == nil,
				"pbkdf2_error":   errText(pbErr),
			})
		}
	}
	return out
}

// passwordIsPHCValidAll records both hashers' verdict on a set of stored values.
//
// `BCrypt.IsPHCValid` returns a flat `false` for everything — it is not PHC-compliant and says so.
// `PBKDF2.IsPHCValid` is what decides whether a stored row needs migrating, so it is exact about
// the parameter set: three parameters, `f=SHA256`, and `w`/`l` matching the hasher's own.
func passwordIsPHCValidAll(pb hashers.PBKDF2, bc hashers.BCrypt) []map[string]any {
	inputs := []struct{ name, phc string }{
		{"default_pbkdf2", "$pbkdf2$f=SHA256,w=600000,l=32$c2FsdA$aGFzaA"},
		{"different_work_factor", "$pbkdf2$f=SHA256,w=1000,l=32$c2FsdA$aGFzaA"},
		{"different_key_length", "$pbkdf2$f=SHA256,w=600000,l=64$c2FsdA$aGFzaA"},
		{"different_prf", "$pbkdf2$f=SHA512,w=600000,l=32$c2FsdA$aGFzaA"},
		{"missing_a_param", "$pbkdf2$w=600000,l=32$c2FsdA$aGFzaA"},
		{"extra_param", "$pbkdf2$f=SHA256,w=600000,l=32,z=1$c2FsdA$aGFzaA"},
		{"wrong_id", "$argon2id$f=SHA256,w=600000,l=32$c2FsdA$aGFzaA"},
		{"no_params", "$pbkdf2$c2FsdA$aGFzaA"},
	}

	var out []map[string]any
	for _, in := range inputs {
		phc, err := phcparser.New(strings.NewReader(in.phc)).Parse()
		out = append(out, map[string]any{
			"name":            in.name,
			"input":           in.phc,
			"parses":          err == nil,
			"pbkdf2_is_valid": err == nil && pb.IsPHCValid(phc),
			"bcrypt_is_valid": err == nil && bc.IsPHCValid(phc),
		})
	}
	return out
}

// passwordRouterAll records which hasher GetHasherFromPHCString hands back, and what it puts in
// the PHC — which is the whole hasher-selection mechanism for a shared Users.Password column.
//
// The two things a port must not smooth over: a parse **failure** is not an error, it is how a
// legacy bcrypt row is recognised (and the whole stored string goes into `Hash`, not `Salt`); and
// a PBKDF2 string with the wrong parameters still routes to PBKDF2, reconstructed from its own
// header rather than from the defaults.
func passwordRouterAll() []map[string]any {
	inputs := []struct{ name, stored string }{
		{"real_bcrypt", "$2a$10$gSZylAupRaDSbThPRdNHa.a91BqVuwn.7B57P60bCRGYhXZtYfOCK"},
		{"real_pbkdf2", "$pbkdf2$f=SHA256,w=600000,l=32$4EJ0aoqSHZt0dKO8ccz/OQ$oWnpQnGjl+x+6J08bTP1OVQ+7ZF29qMaucfyyDY0Rlw"},
		{"pbkdf2_old_params", "$pbkdf2$f=SHA256,w=1000,l=32$c2FsdA$aGFzaA"},
		{"pbkdf2_bad_work_factor", "$pbkdf2$f=SHA256,w=abc,l=32$c2FsdA$aGFzaA"},
		{"unknown_function", "$argon2id$v=19$m=65536,t=2,p=1$c2FsdA$aGFzaA"},
		{"empty", ""},
		{"garbage", "not a hash at all"},
		{"id_only", "$pbkdf2"},
	}

	var out []map[string]any
	for _, in := range inputs {
		h, phc, err := hashers.GetHasherFromPHCString(in.stored)
		entry := map[string]any{
			"name":   in.name,
			"stored": in.stored,
			"error":  errText(err),
		}
		if err == nil {
			_, isBCrypt := h.(hashers.BCrypt)
			_, isPBKDF2 := h.(hashers.PBKDF2)
			entry["is_bcrypt"] = isBCrypt
			entry["is_pbkdf2"] = isPBKDF2
			entry["is_latest"] = hashers.IsLatestHasher(h)
			entry["phc_id"] = phc.Id
			entry["phc_params"] = phc.Params
			entry["phc_salt"] = phc.Salt
			entry["phc_hash"] = phc.Hash
			// For a bcrypt row the WHOLE stored string lands in Hash.
			entry["hash_is_whole_input"] = phc.Hash == in.stored
		}
		out = append(out, entry)
	}
	return out
}

// whichHasherWrites records, as data rather than as a claim in a comment, that the hasher
// `User.PreSave` is handed is PBKDF2 and not bcrypt.
//
// `GetLatestHasher()` is build-tag dependent — `hashers_dev.go` allows tests to substitute a
// faster one — so this also records that no substitution is in effect for this run.
func whichHasherWrites() map[string]any {
	latest := hashers.GetLatestHasher()
	sample, err := latest.Hash("hunter2")
	if err != nil {
		return map[string]any{"error": err.Error()}
	}
	_, isBCrypt := latest.(hashers.BCrypt)
	_, isPBKDF2 := latest.(hashers.PBKDF2)
	return map[string]any{
		"go_type": fmt.Sprintf("%T", latest),
		// Not the sample itself — it carries a random salt. Only its shape.
		"prefix":                       sample[:strings.Index(sample[1:], "$")+2],
		"is_bcrypt":                    isBCrypt,
		"is_pbkdf2":                    isPBKDF2,
		"is_latest":                    hashers.IsLatestHasher(latest),
		"call_site":                    "channels/store/sqlstore/user_store.go:180 — user.PreSave(hashers.GetLatestHasher())",
		"bcrypt_is_still_the_fallback": bcryptIsTheFallback(),
	}
}

// bcryptIsTheFallback proves the claim that bcrypt is still reachable: feed
// `GetHasherFromPHCString` something that is not a PHC string and see which hasher comes back.
// This is why the bcrypt port is owed even though nothing writes bcrypt any more.
func bcryptIsTheFallback() bool {
	h, _, err := hashers.GetHasherFromPHCString("$2a$10$gSZylAupRaDSbThPRdNHa.a91BqVuwn.7B57P60bCRGYhXZtYfOCK")
	if err != nil {
		return false
	}
	_, ok := h.(hashers.BCrypt)
	return ok
}

// bcryptFormat records the layout bcrypt.go's doc comment describes, measured off a real hash
// rather than transcribed from the comment.
func bcryptFormat() map[string]any {
	h := pinnedHashes[1].bcrypt // "hunter2"
	return map[string]any{
		"example_total_len": len(h),
		"version_prefix":    h[:4],
		"cost_digits":       h[4:6],
		"salt_b64_len":      len(h[7:29]),
		"digest_b64_len":    len(h[29:]),
		// bcrypt's base64 is its OWN alphabet ("./A-Za-z0-9"), not standard base64 — the leading
		// "." and "/" are what give it away. A port using a standard decoder silently mangles it.
		"alphabet_starts": "./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
		"is_phc":          false,
		"phc_note":        "shaped like PHC but not PHC-compliant: $2a$ is not a function id and the cost is not name=value",
	}
}

// pbkdf2Format records the PHC header the hasher precomputes, which is the part every stored hash
// shares and therefore the part a port must reproduce exactly.
func pbkdf2Format(pb hashers.PBKDF2) map[string]any {
	sample := pinnedHashes[1].pbkdf2 // "hunter2"
	header := sample[:strings.LastIndex(sample[:strings.LastIndex(sample, "$")], "$")+1]
	phc, _ := phcparser.New(strings.NewReader(sample)).Parse()
	return map[string]any{
		"header":            header,
		"id":                phc.Id,
		"params":            phc.Params,
		"param_order":       "f,w,l — written in that order by NewPBKDF2, and PHC params are order-sensitive text",
		"work_factor":       phc.Params["w"],
		"key_length":        phc.Params["l"],
		"prf":               phc.Params["f"],
		"salt_len_bytes":    16,
		"base64_encoding":   "RawStdEncoding — standard alphabet, NO padding",
		"is_phc_valid":      pb.IsPHCValid(phc),
		"work_factor_int":   mustAtoi(phc.Params["w"]),
		"key_length_int":    mustAtoi(phc.Params["l"]),
		"separator_between": "$ between header, salt and hash; the salt is NOT '$'-terminated by the header",
	}
}

// wrongPassword returns a password that is NOT the input but is the same length in bytes.
//
// The obvious negative control — append a character — does not work, and finding that out is what
// `bcrypt_truncation` below records: bcrypt hashes only the first 72 bytes, so appending to a
// 72-byte password produces something that still verifies. A same-length mutation cannot hit that.
func wrongPassword(pw string) string {
	if pw == "" {
		return "x"
	}
	b := []byte(pw)
	b[0] ^= 0x01
	return string(b)
}

// bcryptTruncation pins the asymmetry the corpus tripped over.
//
// bcrypt's key schedule consumes at most 72 bytes. `GenerateFromPassword` **refuses** anything
// longer (that is where `ErrPasswordTooLong` comes from), but `CompareHashAndPassword` does not
// carry the same check — so a 73-byte password silently verifies against a hash of its first 72.
//
// It matters at the boundary between the two hashers. `hashers.BCrypt.CompareHashAndPassword`
// adds the length check back, so through the *package* a 73-byte password is rejected; through
// x/crypto directly it is accepted. A Rust port that reproduces only the crate's behaviour and not
// the package's would accept a login the Go server rejects — which is a security difference, not a
// cosmetic one. Recorded through both paths so the port cannot pick the wrong one.
//
// PBKDF2 has no such truncation: it consumes the whole password, so the same probe on the PBKDF2
// hash fails to verify. Recorded side by side, because "both hashers cap at 72" is true of `Hash`
// and false of the underlying primitives.
func bcryptTruncation(pb hashers.PBKDF2) map[string]any {
	c := pinnedHashes[5] // exactly_72
	longer := c.pw + "x" // 73 bytes
	if len(c.pw) != 72 || len(longer) != 73 {
		return map[string]any{"error": "corpus entry 5 is no longer the 72-byte case"}
	}

	viaCrypto := bcrypt.CompareHashAndPassword([]byte(c.bcrypt), []byte(longer))
	viaPackage := hashers.NewBCrypt().CompareHashAndPassword(phcparser.PHC{Hash: c.bcrypt}, longer)

	phc, _ := phcparser.New(strings.NewReader(c.pbkdf2)).Parse()
	viaPBKDF2 := pb.CompareHashAndPassword(phc, longer)

	// And the reverse direction: 71 bytes must NOT verify, or the truncation would be at 71.
	shorter := c.pw[:71]

	return map[string]any{
		"password_bytes":                           len(c.pw),
		"longer_bytes":                             len(longer),
		"x_crypto_accepts_73_byte_password":        viaCrypto == nil,
		"hashers_package_accepts_73_byte_password": viaPackage == nil,
		"hashers_package_error":                    errText(viaPackage),
		"pbkdf2_accepts_73_byte_password":          viaPBKDF2 == nil,
		"truncation_boundary_is_72": bcrypt.CompareHashAndPassword(
			[]byte(c.bcrypt), []byte(shorter)) != nil,
		"generate_still_refuses_73": func() bool {
			_, err := bcrypt.GenerateFromPassword([]byte(longer), hashers.BCryptCost)
			return err != nil
		}(),
	}
}

func mustAtoi(s string) int {
	n, err := strconv.Atoi(s)
	if err != nil {
		return -1
	}
	return n
}
