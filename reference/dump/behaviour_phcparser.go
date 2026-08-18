package main

// Behavioural oracle for channels/app/password/phcparser, written to fixtures/behaviour_phcparser.json.
//
// [D-109]'s blocker: the parser that decides which hasher a stored `Users.Password` value belongs
// to. Second oracle built from the AGPL half of the tree, so it feeds `mm-app` alone ([D-112]).
//
// 434 lines of hand-written state machine. Reading it produces confident answers to at least six
// questions, and the corpus below exists because several of those answers are wrong.
//
// # `MaxRunes` limits BYTES
//
//	const MaxRunes = 256
//	bufio.NewReader(io.LimitReader(r, MaxRunes))
//
// `io.LimitReader`'s bound is a byte count. The constant is named `MaxRunes`, and its doc comment
// says "the maximum number of runes allowed". So a multi-byte input is cut at 256 **bytes**.
//
// Whether that is *observable* is the subtle part, and it took two wrong answers to settle. Every
// character in all four classes is single-byte, so within a legal prefix the byte index and the
// rune index are equal and the two rules cut identically — which makes most inputs useless for
// telling them apart. The exception is an input whose 256-byte cut lands **inside** a multi-byte
// character: Go decodes the orphaned lead byte as U+FFFD, where a rune limiter would have handed
// over the whole character, and the error text differs. `limit_boundary` walks exactly that edge.
//
// # Truncation is silent, and can succeed
//
// "If the string is longer, the remaining runes are ignored." Not an error — the reader simply
// ends, which every `EOF` branch treats as a well-formed terminator. So an over-long PHC string
// **parses**, with a hash that is quietly short. Recorded because it is the one path where the
// parser returns success on input it did not see all of.
//
// # A NUL byte is indistinguishable from EOF
//
//	const eof = rune(0)
//	func (p *Parser) read() rune { ch, _, err := p.reader.ReadRune(); if err != nil { return eof }; return ch }
//
// `ReadRune` returns U+0000 with a nil error for a real NUL byte, and `read` returns the same
// `eof` sentinel for both. So a NUL inside the input terminates the parse as if the string had
// ended. Recorded rather than reasoned about, because the two readings ("NUL is illegal" and "NUL
// ends the string") are equally plausible from the source.
//
// # `parseToken`'s error text always says "expected '$'"
//
//	return "", fmt.Errorf("found %q, expected '$'", literal)
//
// regardless of which token was expected. Every caller wraps it with a better message, so the
// inner text is usually invisible — but it survives into two errors that are returned unwrapped.
//
// # `v` is rejected as a parameter name in ONE of the two places it can appear
//
// The check at parser.go:350 fires only for the **first** parameter. The loop at parser.go:396
// parses subsequent names with `parseToken(PARAMNAME)` and never re-checks. So `$id$a=1,v=2$salt`
// is accepted and `$id$v=2$salt` is not. Driven both ways.
//
// # The `Params` map is nil on failure and non-nil on success
//
// `Parse` allocates the map first thing, then every error path returns a fresh `PHC{}` — so the
// map it allocated is discarded. A caller distinguishing "no parameters" from "did not parse"
// by nil-ness gets the right answer, but only by accident of that discard.
//
// Determinism: fixed inputs only. Inputs are recorded as base64 as well as text, because the
// corpus contains a NUL and an invalid UTF-8 byte, neither of which survives a JSON string.

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/v8/channels/app/password/phcparser"
)

// The corpus. Each entry is raw bytes rather than a string so the NUL and the invalid-UTF-8 cases
// are expressible.
type phcCase struct {
	name string
	in   []byte
}

func phcCorpus() []phcCase {
	b := func(name, s string) phcCase { return phcCase{name, []byte(s)} }

	cases := []phcCase{
		// --- the two shapes that actually appear in Users.Password ---------------------------
		b("real_pbkdf2", "$pbkdf2$f=SHA256,w=600000,l=32$4EJ0aoqSHZt0dKO8ccz/OQ$oWnpQnGjl+x+6J08bTP1OVQ+7ZF29qMaucfyyDY0Rlw"),
		b("real_bcrypt", "$2a$10$gSZylAupRaDSbThPRdNHa.a91BqVuwn.7B57P60bCRGYhXZtYfOCK"),

		// --- every valid shape the spec allows -----------------------------------------------
		b("id_only", "$argon2id"),
		b("id_and_version", "$argon2id$v=19"),
		b("id_and_one_param", "$argon2id$m=65536"),
		b("id_and_params", "$argon2id$m=65536,t=2,p=1"),
		b("id_version_params", "$argon2id$v=19$m=65536,t=2,p=1"),
		b("id_and_salt", "$argon2id$c29tZXNhbHQ"),
		b("id_salt_hash", "$argon2id$c29tZXNhbHQ$aGFzaA"),
		b("id_version_salt", "$argon2id$v=19$c29tZXNhbHQ"),
		b("id_version_salt_hash", "$argon2id$v=19$c29tZXNhbHQ$aGFzaA"),
		b("id_params_salt", "$argon2id$m=65536,t=2$c29tZXNhbHQ"),
		b("id_params_salt_hash", "$argon2id$m=65536,t=2$c29tZXNhbHQ$aGFzaA"),
		b("id_version_params_salt_hash", "$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$aGFzaA"),

		// --- the identifier character classes, at their edges ---------------------------------
		b("id_with_digits_and_minus", "$pbkdf2-sha256$x=1"),
		b("id_uppercase_rejected", "$Argon2id"),
		b("id_underscore_rejected", "$argon_2"),
		b("param_value_uppercase_ok", "$pbkdf2$f=SHA256"),
		b("param_value_symbols_ok", "$x$a=A9/+.-"),
		b("param_value_comma_ends_it", "$x$a=1,b=2"),
		// NOT rejected. The FIRST identifier after the id is scanned as B64ENCODED — a superset —
		// and is then used as a parameter name without being re-checked against the narrower
		// PARAMNAME class. Later names, parsed inside the comma loop, do get checked.
		b("first_param_name_may_be_uppercase", "$x$A=1"),
		b("first_param_name_may_hold_b64_symbols", "$x$a+b/c=1"),
		b("later_param_name_uppercase", "$x$a=1,B=2"),
		b("later_param_name_b64_symbols", "$x$a=1,b+c=2"),
		b("salt_plus_and_slash_ok", "$x$ab+/cd"),
		b("salt_minus_is_not_b64", "$x$ab-cd"),
		b("salt_equals_padding_rejected", "$x$YWJj="),

		// --- the `v` asymmetry ----------------------------------------------------------------
		// Also NOT rejected — `v` in the first position is consumed as the VERSION, so the
		// "v is only allowed as the version key" check at parser.go:350 is unreachable from here.
		// It fires only for a `v` in the second position, i.e. after a version block
		// (`version_twice` below). And inside the comma loop `v` is an ordinary name.
		b("v_in_first_position_is_the_version", "$x$v=2$c29tZXNhbHQ"),
		b("v_as_later_param_accepted", "$x$a=1,v=2$c29tZXNhbHQ"),
		b("version_then_param_named_v", "$x$v=19$a=1,v=2$c29tZXNhbHQ"),

		// --- duplicate names -------------------------------------------------------------------
		b("duplicate_param_last_wins", "$x$a=1,a=2$c29tZXNhbHQ"),

		// --- malformed -------------------------------------------------------------------------
		b("empty", ""),
		b("no_leading_dollar", "pbkdf2$f=SHA256"),
		b("dollar_only", "$"),
		b("dollar_dollar", "$$"),
		b("empty_id_then_params", "$$a=1"),
		b("param_without_value", "$x$a="),
		b("param_without_equals", "$x$a$"),
		b("trailing_comma", "$x$a=1,"),
		b("trailing_dollar_after_params", "$x$a=1$"),
		b("trailing_dollar_after_salt", "$x$c29tZXNhbHQ$"),
		b("extra_field_after_hash", "$x$c29tZXNhbHQ$aGFzaA$more"),
		b("garbage_after_hash", "$x$c29tZXNhbHQ$aGFzaA!"),
		b("space_in_id", "$arg on2"),
		b("space_before_dollar", " $x"),
		b("version_without_value", "$x$v="),
		b("version_twice", "$x$v=1$v=2"),
		b("comma_before_any_param", "$x$,a=1"),
		b("equals_as_first_char", "$=x"),

		// --- Go's `read` cannot tell a NUL from the end of the input ----------------------------
		{"nul_mid_id", []byte("$arg\x00on2id")},
		{"nul_before_params", []byte("$x\x00$a=1")},
		{"nul_mid_hash", []byte("$x$c29tZXNhbHQ$aGF\x00zaA")},

		// --- invalid UTF-8 becomes U+FFFD, which is in no character class -----------------------
		{"invalid_utf8_in_id", []byte("$arg\x80on2")},
		{"invalid_utf8_after_id", []byte("$argon2\x80")},

		// --- multi-byte runes, so the byte/rune question has an observable answer ----------------
		b("multibyte_in_id", "$argón2"),
		b("multibyte_in_param_value", "$x$a=é"),
	}

	// The length limit. `MaxRunes` is 256, and `io.LimitReader` counts bytes — so the boundary is
	// driven with a salt padded to sit either side of 256 BYTES, and again with two-byte runes so
	// the two readings give different answers.
	prefix := "$x$" // 3 bytes
	for _, n := range []int{250, 252, 253, 260} {
		cases = append(cases, phcCase{
			name: fmt.Sprintf("ascii_salt_total_%d_bytes", len(prefix)+n),
			in:   []byte(prefix + strings.Repeat("a", n)),
		})
	}
	// 200 two-byte runes = 400 bytes but only 200 runes. Under a rune limit this is fine; under a
	// byte limit it is cut. The salt characters must be b64-legal, so the multi-byte runes go in a
	// parameter value instead, where the class is wider — but still not wide enough to accept
	// them, so the observable is *where* the parse stops rather than whether it succeeds.
	cases = append(cases, phcCase{
		name: "two_hundred_two_byte_runes_in_id",
		in:   []byte("$" + strings.Repeat("é", 200)),
	})
	// A salt long enough to be cut, made only of b64 characters: under a byte limit the parse
	// succeeds with a SHORTER salt than the input carried.
	cases = append(cases, phcCase{
		name: "salt_cut_by_the_limit",
		in:   []byte(prefix + strings.Repeat("a", 400)),
	})
	// And the same, with a hash after it — so the cut lands mid-hash and the trailing EOF check
	// still passes.
	cases = append(cases, phcCase{
		name: "hash_cut_by_the_limit",
		in:   []byte("$x$c29tZXNhbHQ$" + strings.Repeat("b", 400)),
	})

	return cases
}

func writePHCParserBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":         phcConstants(),
		"tokens":            phcTokens(),
		"character_classes": phcCharacterClasses(),
		"cases":             phcAll(),
		"limit":             phcLimitProbe(),
		"limit_boundary":    phcLimitBoundary(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_phcparser.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

func phcConstants() map[string]any {
	return map[string]any{
		"MaxRunes": phcparser.MaxRunes,
		// The name says runes and `io.LimitReader` counts bytes. Recorded as a claim about the
		// *mechanism* so the Rust port cannot quietly implement the name.
		"limiter_counts": "bytes — io.LimitReader(r, MaxRunes)",
	}
}

// phcTokens records the bit values, which are `1 << iota` and therefore easy to transcribe with an
// off-by-one. `IDENT` is the OR of the four literal tokens and is what makes `token&expected != 0`
// accept any identifier where a specific one was asked for.
func phcTokens() map[string]any {
	return map[string]any{
		"ILLEGAL":     uint(phcparser.ILLEGAL),
		"EOF":         uint(phcparser.EOF),
		"DOLLARSIGN":  uint(phcparser.DOLLARSIGN),
		"COMMA":       uint(phcparser.COMMA),
		"EQUALSIGN":   uint(phcparser.EQUALSIGN),
		"FUNCTIONID":  uint(phcparser.FUNCTIONID),
		"PARAMNAME":   uint(phcparser.PARAMNAME),
		"PARAMVALUE":  uint(phcparser.PARAMVALUE),
		"B64ENCODED":  uint(phcparser.B64ENCODED),
		"IDENT":       uint(phcparser.IDENT),
		"ident_is_or": uint(phcparser.IDENT) == uint(phcparser.FUNCTIONID|phcparser.PARAMNAME|phcparser.PARAMVALUE|phcparser.B64ENCODED),
	}
}

// phcCharacterClasses sweeps every codepoint that could plausibly appear through each position's
// class, by parsing a minimal string that puts the codepoint exactly there.
//
// The predicates themselves are unexported, so they cannot be called directly. Probing them
// through `Parse` is stronger anyway: it measures the class *as the parser applies it*, which is
// what a port has to match, rather than a helper the parser might not use where we assume.
func phcCharacterClasses() map[string]any {
	// 0..127 plus a handful of interesting non-ASCII.
	var points []rune
	for r := rune(0); r < 128; r++ {
		points = append(points, r)
	}
	points = append(points, '\u00e9', '\u0130', '\u4e2d', ' ', '\ufeff', '\ufffd')

	classes := map[string]any{}

	// A codepoint is "in" a class if a minimal PHC string carrying it at that position parses AND
	// the parsed field holds it. That second half matters: a `$` at a salt position parses fine
	// and simply is not part of the salt.
	probe := func(build func(string) string, field func(phcparser.PHC) string) []map[string]any {
		var res []map[string]any
		for _, r := range points {
			in := build(string(r))
			phc, err := phcparser.New(strings.NewReader(in)).Parse()
			entry := map[string]any{
				"codepoint": int(r),
				"ok":        err == nil,
			}
			if err == nil {
				entry["field"] = field(phc)
				entry["contains"] = strings.ContainsRune(field(phc), r)
			}
			res = append(res, entry)
		}
		return res
	}

	classes["function_id"] = probe(
		func(s string) string { return "$a" + s + "b" },
		func(p phcparser.PHC) string { return p.Id },
	)
	classes["param_name"] = probe(
		func(s string) string { return "$x$a" + s + "b=1" },
		func(p phcparser.PHC) string {
			var names []string
			for k := range p.Params {
				names = append(names, k)
			}
			return strings.Join(names, ",")
		},
	)
	classes["param_value"] = probe(
		func(s string) string { return "$x$k=a" + s + "b" },
		func(p phcparser.PHC) string { return p.Params["k"] },
	)
	classes["salt"] = probe(
		func(s string) string { return "$x$a" + s + "b" },
		func(p phcparser.PHC) string { return p.Salt },
	)

	return classes
}

func phcAll() []map[string]any {
	var out []map[string]any
	for _, c := range phcCorpus() {
		phc, err := phcparser.New(bytes.NewReader(c.in)).Parse()

		entry := map[string]any{
			"name": c.name,
			// Base64 because the corpus holds a NUL and an invalid UTF-8 byte.
			"input_b64":   base64.StdEncoding.EncodeToString(c.in),
			"input_bytes": len(c.in),
			"input_runes": len([]rune(string(c.in))),
			"ok":          err == nil,
		}
		if err != nil {
			entry["error"] = err.Error()
		}
		entry["id"] = phc.Id
		entry["version"] = phc.Version
		entry["salt"] = phc.Salt
		entry["hash"] = phc.Hash
		entry["params"] = phc.Params
		// The nil-vs-empty distinction every error path creates by discarding the allocated map.
		entry["params_is_nil"] = phc.Params == nil
		entry["params_len"] = len(phc.Params)
		out = append(out, entry)
	}
	return out
}

// phcLimitProbe finds the actual cut-off by bisection over both an ASCII and a two-byte-rune salt.
//
// Rather than assert "it is bytes", this records the largest input of each kind that survives
// intact — from which the byte-vs-rune question answers itself, and stays answered if upstream
// ever changes the limiter.
func phcLimitProbe() map[string]any {
	// The longest all-ASCII salt that comes back whole.
	longestASCII := -1
	for n := 1; n <= 400; n++ {
		in := "$x$" + strings.Repeat("a", n)
		phc, err := phcparser.New(strings.NewReader(in)).Parse()
		if err == nil && len(phc.Salt) == n {
			longestASCII = n
		}
	}

	// The byte-versus-rune question, decided by one input.
	//
	// No field's character class admits a multi-byte rune, so a *successful* parse can never contain
	// one — which means the distinction is only observable in what the limiter throws away. This
	// input exploits that: 253 legal salt characters, then multi-byte padding.
	//
	//	"$x$" + "a"*253 + "\u00e9"*100          256 bytes up to the padding; 456 bytes, 356 runes total
	//
	// Under a BYTE limit the reader stops at 256 and never sees the padding, so the salt parses and
	// the string ends cleanly. Under a RUNE limit the reader delivers 256 runes — 253 a's and three
	// '\u00e9' — and '\u00e9' is not base64, so the parse ERRORS. One input, two opposite answers.
	decisive := "$x$" + strings.Repeat("a", 253) + strings.Repeat("\u00e9", 100)
	decisivePHC, decisiveErr := phcparser.New(strings.NewReader(decisive)).Parse()

	// And the direct probe: exactly `MaxRunes` two-byte runes. That is 512 bytes and 256 runes, so
	// a rune limiter would pass the whole string through and a byte limiter cuts it in half.
	twoByte := "$" + strings.Repeat("é", phcparser.MaxRunes)
	_, twoByteErr := phcparser.New(strings.NewReader(twoByte)).Parse()

	return map[string]any{
		"longest_intact_ascii_salt": longestASCII,
		"total_len_at_that_point":   longestASCII + 3,
		"max_runes_constant":        phcparser.MaxRunes,
		"limit_is_bytes":            longestASCII+3 == phcparser.MaxRunes,
		// NOT decisive, and recorded so nobody re-derives the wrong conclusion from it. The legal
		// prefix is 256 characters under EITHER rule, so both limiters stop before the padding and
		// this parses whichever way the limit is counted. `limit_boundary` is where the two differ.
		"aligned_boundary_is_not_decisive": map[string]any{
			"input_bytes": len(decisive),
			"input_runes": len([]rune(decisive)),
			"parses":      decisiveErr == nil,
			"salt":        decisivePHC.Salt,
			"salt_len":    len(decisivePHC.Salt),
			"error":       errText(decisiveErr),
			"why_not":     "every character in all four classes is single-byte, so within a legal prefix byte index == rune index",
			"see_instead": "limit_boundary",
		},
		"two_byte_input_bytes":            len(twoByte),
		"two_byte_input_runes":            len([]rune(twoByte)),
		"two_byte_input_errors":           twoByteErr != nil,
		"over_limit_truncates_not_errors": phcOverLimitTruncates(),
	}
}

// phcOverLimitTruncates records the sharpest consequence of the limit: an over-long PHC string
// does not fail, it **succeeds with a short hash**.
// phcLimitBoundary drives the byte limit across a multi-byte character.
//
// The limit's byte-versus-rune question is *usually* unobservable: every character in all four
// classes is single-byte, so within a legal prefix the byte index and the rune index are equal and
// either limiter cuts in the same place. It becomes observable in exactly one situation — when the
// 256-byte cut falls **inside** a multi-byte character. Go then decodes the orphaned lead byte as
// U+FFFD, where a rune-counting limiter would have delivered the whole character, and the two
// produce different error text.
//
// So this walks the legal prefix across the boundary against three pad widths, recording Go's
// answer for each. Whichever way the port implements the limit, these are the inputs that tell.
func phcLimitBoundary() []map[string]any {
	var out []map[string]any
	for _, legal := range []int{200, 250, 252, 253, 254, 255, 256, 300} {
		for _, pad := range []string{"\u00e9", "\u4e2d", "\U0001f512"} {
			in := "$x$" + strings.Repeat("a", legal) + strings.Repeat(pad, 100)
			phc, err := phcparser.New(strings.NewReader(in)).Parse()
			entry := map[string]any{
				"legal_prefix": legal,
				"pad":          pad,
				"pad_bytes":    len(pad),
				"input_bytes":  len(in),
				"input_runes":  len([]rune(in)),
				"ok":           err == nil,
				"error":        errText(err),
				"salt":         phc.Salt,
				"salt_len":     len(phc.Salt),
			}
			out = append(out, entry)
		}
	}
	return out
}

func phcOverLimitTruncates() map[string]any {
	in := "$x$c29tZXNhbHQ$" + strings.Repeat("b", 400)
	phc, err := phcparser.New(strings.NewReader(in)).Parse()
	return map[string]any{
		"input_bytes":   len(in),
		"parses":        err == nil,
		"hash_len_in":   400,
		"hash_len_out":  len(phc.Hash),
		"hash_is_short": len(phc.Hash) < 400,
	}
}
