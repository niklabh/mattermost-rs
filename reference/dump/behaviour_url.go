package main

// Behavioural oracle for model.IsValidHTTPURL (utils.go:790) and model.SlackCompatibleBool
// (slack_compatibility.go:29), written to fixtures/behaviour_url.json.
//
// Both are leaves blocking message_attachment.go: `MessageAttachment.IsValid` calls
// IsValidHTTPURL six times, and `MessageAttachmentField.Short` is a SlackCompatibleBool.
//
// IsValidHTTPURL is [D-003] — the last of the three "Go stdlib does the real work" validators,
// after IsValidEmail (net/mail) and IsValidLocale (x/text/language). It delegates to
// `net/url.ParseRequestURI`, so the accepted set is Go's URI grammar and not something to
// reason out. The corpus is therefore large and deliberately adversarial around the places a
// hand-written parser goes wrong:
//
//   - the `http://` prefix test is `strings.Index(...) != 0`, i.e. **case-sensitive** and
//     position-sensitive;
//   - `ParseRequestURI` does not strip a `#fragment`, unlike `Parse`;
//   - a bracketed host is checked for a closing `]` and a valid port and **not** for being an
//     actual IP;
//   - `%` escapes inside the host are rejected unless they encode a byte >= 0x80 (or are
//     `%25`), which is the opposite of the intuition that percent-encoding is always allowed.

import (
	"encoding/json"
	"fmt"
	"net/url"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeURLBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_valid_http_url":       httpURLAll(),
		"is_valid_http_url_fuzz":  httpURLFuzzAll(),
		"is_valid_http_url_bytes": httpURLByteTables(),
		"slack_compatible_bool":   slackBoolAll(),
		"http_url_diagnostics":    httpURLDiagnostics(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_url.json"), append(blob, '\n'), 0o644)
}

// --- IsValidHTTPURL ---------------------------------------------------------------------

var httpURLCorpus = []string{
	// The prefix gate.
	"",
	"x",
	"http",
	"http:",
	"http:/",
	"http://",
	"https://",
	"HTTP://example.com",
	"Http://example.com",
	"HTTPS://example.com",
	"hTtP://example.com",
	" http://example.com",
	"\thttp://example.com",
	"xhttp://example.com",
	"ahttp://example.com",
	"ftp://example.com",
	"file:///etc/passwd",
	"//example.com",
	"/relative/path",
	"mailto:a@b.com",
	"javascript:alert(1)",
	"httpss://example.com",
	"http:://example.com",

	// The ordinary shapes.
	"http://example.com",
	"https://example.com",
	"http://example.com/",
	"http://example.com/path",
	"http://example.com/path/to/thing",
	"http://example.com/path?query=1",
	"http://example.com/path?query=1&other=2",
	"http://example.com?query=1",
	"http://example.com:8080",
	"http://example.com:8080/path",
	"http://example.com:0",
	"http://example.com:65536",
	"http://example.com:99999999999999999999",
	"http://example.com:",
	"http://example.com:abc",
	"http://example.com:80a",
	"http://example.com: 80",
	"http://example.com:8080:9090",

	// Host shapes. A single character, a bare dot and a hyphen are all accepted hosts.
	"http://x",
	"http://.",
	"http://..",
	"http://-",
	"http://_",
	"http://~",
	"http://localhost",
	"http://127.0.0.1",
	"http://999.999.999.999",
	"http://sub.domain.example.co.uk",
	"http://example.com.",

	// Empty authority: Host is "" and the function fails on that, not on a parse error.
	"http:///",
	"http:///path",
	"http://?query",
	"http://#fragment",
	"http:////double",

	// Userinfo. LastIndex means an earlier @ lands inside the userinfo.
	"http://user@example.com",
	"http://user:pass@example.com",
	"http://@example.com",
	"http://user@@example.com",
	"http://a@b@example.com",
	"http://user name@example.com",
	"http://user{}@example.com",
	"http://user%20name@example.com",
	"http://example.com@",

	// Bracketed hosts. Go checks for a closing bracket and a valid port, NOT for a real IP.
	"http://[::1]",
	"http://[::1]/",
	"http://[::1]:8080",
	"http://[::1]:abc",
	"http://[::1",
	"http://[]",
	"http://[abc]",
	"http://[not an ip]",
	"http://]::1[",
	"http://[fe80::1%25eth0]",
	"http://[fe80::1%eth0]",

	// Fragments are NOT stripped by ParseRequestURI, so a # in the authority is a host byte.
	"http://example.com#frag",
	"http://example.com/#frag",
	"http://example.com/path#frag",
	"http://example.com/path?q=1#frag",

	// Percent escapes. In the host, only >= 0x80 (and %25) are permitted; in the path, any
	// well-formed escape is.
	"http://example.com/%20",
	"http://example.com/%2",
	"http://example.com/%",
	"http://example.com/%zz",
	"http://example.com/%GG",
	"http://example.com/a%2Fb",
	"http://exam%20ple.com",
	"http://exam%41ple.com",
	"http://exam%25ple.com",
	"http://exam%7Fple.com",
	"http://exam%80ple.com",
	"http://exam%FFple.com",
	"http://example.com/?q=%zz",
	"http://example.com/?q=%20",

	// Characters that are legal in a host, and characters that are not.
	"http://exa mple.com",
	"http://exa\tmple.com",
	"http://example.com/a b",
	"http://exam|ple.com",
	"http://exam^ple.com",
	"http://exam`ple.com",
	"http://exam{ple.com",
	"http://exam}ple.com",
	"http://exam\\ple.com",
	"http://exam!ple.com",
	"http://exam$ple.com",
	"http://exam&ple.com",
	"http://exam'ple.com",
	"http://exam(ple.com",
	"http://exam)ple.com",
	"http://exam*ple.com",
	"http://exam+ple.com",
	"http://exam,ple.com",
	"http://exam;ple.com",
	"http://exam=ple.com",
	"http://exam<ple.com",
	"http://exam>ple.com",
	"http://exam\"ple.com",

	// Control bytes are rejected wherever they appear.
	"http://example.com\n",
	"http://example.com\r",
	"http://example.com\x00",
	"http://example.com\x7f",
	"http://example.com/pa\nth",
	"http://exam\x01ple.com",

	// Non-ASCII passes through untouched — there is no IDNA step here.
	"http://日本.com",
	"http://例え.テスト",
	"http://exämple.com",
	"http://example.com/日本",
	"http://example.com/?q=日本",
	"http://😀.com",

	// Odd but well-formed.
	"http://example.com//",
	"http://example.com/../..",
	"http://example.com/?",
	"http://example.com/??",
	"http://example.com/?#",
	"https://example.com:443/a/b/c?d=e&f=g#h",
}

func httpURLAll() map[string]bool {
	res := make(map[string]bool, len(httpURLCorpus))
	for _, in := range httpURLCorpus {
		res[in] = model.IsValidHTTPURL(in)
	}
	return res
}

// httpURLFuzzAll generates a deterministic pseudo-random corpus, the same technique
// emailFuzzAll uses and for the same reason: a hand-written corpus only tests the cases its
// author already thought of.
//
// Two thirds of the inputs are given a real scheme prefix, because an unprefixed input fails at
// the first line and proves nothing about the parser behind it.
//
// The generator is a self-contained xorshift rather than math/rand so the output cannot drift
// with the Go release, and the fixture stays byte-stable across runs (see D-032).
func httpURLFuzzAll() map[string]bool {
	pool := []rune("abxy019.-_~/:@?#%[]!$&'()*+,;=<>\"| \t\\^`{}日é")
	state := uint64(0x9E3779B97F4A7C15)
	next := func(n int) int {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		return int(state % uint64(n))
	}

	res := make(map[string]bool, 3000)
	for range 3000 {
		length := 1 + next(14)
		buf := make([]rune, length)
		for i := range buf {
			buf[i] = pool[next(len(pool))]
		}
		in := string(buf)
		switch next(3) {
		case 0:
			in = "http://" + in
		case 1:
			in = "https://" + in
		}
		res[in] = model.IsValidHTTPURL(in)
	}
	return res
}

// --- SlackCompatibleBool ------------------------------------------------------------------

type slackBoolCase struct {
	// The raw JSON handed to UnmarshalJSON, as it would appear as a field value.
	JSON  string `json:"json"`
	OK    bool   `json:"ok"`
	Value bool   `json:"value"`
	// What the value marshals back to. There is no MarshalJSON, so it is a plain bool.
	Marshalled string `json:"marshalled"`
}

func slackBoolAll() []slackBoolCase {
	inputs := []string{
		"true", "false",
		`"true"`, `"false"`,
		// strings.ToLower is applied to the raw bytes, so every casing is accepted —
		// quoted and unquoted alike.
		"TRUE", "True", "tRuE", "FALSE", "False",
		`"TRUE"`, `"True"`, `"FALSE"`,
		// Everything else is an error, including the things a lenient parser would take.
		"1", "0", "-1", `"1"`, `"0"`,
		"null", `""`, `" true"`, `"true "`, "'true'",
		"yes", "no", `"yes"`, "t", "f", `"t"`,
		"{}", "[]", "[true]", `{"a":true}`,
		"truee", "tru", "TRUEE",
		// UnmarshalJSON sees the RAW token, so a string spelled with escapes does not match
		// even though it decodes to "true". This is the one place a serde port, which is
		// handed the decoded string, cannot follow without reaching for RawValue.
		`"\u0074rue"`,
		`"tr\u0075e"`,
		`"\u0054RUE"`,
		`"fals\u0065"`,
	}

	res := make([]slackBoolCase, 0, len(inputs))
	for _, in := range inputs {
		var b model.SlackCompatibleBool
		err := json.Unmarshal([]byte(in), &b)
		c := slackBoolCase{JSON: in, OK: err == nil, Value: bool(b)}
		if blob, mErr := json.Marshal(b); mErr == nil {
			c.Marshalled = string(blob)
		}
		res = append(res, c)
	}
	return res
}

// httpURLByteTables answers the question the hand corpus only samples: for each position in the
// URL, exactly which ASCII bytes are legal? Hand-picking characters finds the ones the author
// suspected; sweeping 0..127 finds the ones nobody suspected.
//
// Three positions matter, and they have three different rules:
//
//   - the **host**, which is validated against a character class;
//   - the **path**, which is only checked for well-formed `%` escapes;
//   - the **query**, which is not checked at all.
func httpURLByteTables() map[string]any {
	sweep := func(build func(c string) string) map[string]bool {
		res := make(map[string]bool, 128)
		for i := range 128 {
			res[fmt.Sprintf("%02x", i)] = model.IsValidHTTPURL(build(string(rune(i))))
		}
		return res
	}

	return map[string]any{
		"host":     sweep(func(c string) string { return "http://a" + c + "b.com" }),
		"path":     sweep(func(c string) string { return "http://example.com/a" + c + "b" }),
		"query":    sweep(func(c string) string { return "http://example.com/?q=a" + c + "b" }),
		"userinfo": sweep(func(c string) string { return "http://a" + c + "b@example.com" }),
		"colons":   httpURLColonAll(),
		"brackets": httpURLBracketAll(),
	}
}

// The multi-colon and port cases. `validOptionalPort` looks only at the text after the LAST
// colon, so the interesting question is what happens to the colons before it.
func httpURLColonAll() map[string]bool {
	inputs := []string{
		"http://a:1",
		"http://a:",
		"http://:1",
		"http://:",
		"http://a:1:2",
		"http://a:1:",
		"http://a::1",
		"http://a:b",
		"http://a:1/path",
		"http://a:1?q",
		"http://user:pass@a:1",
		"http://user:pass@a:1:2",
		"http://a:00080",
		"http://a:+80",
		"http://a:-80",
		"http://a:8 0",
		"http://[::1]:1:2",
	}
	res := make(map[string]bool, len(inputs))
	for _, in := range inputs {
		res[in] = model.IsValidHTTPURL(in)
	}
	return res
}

// Bracketed hosts. The hand corpus showed `[abc]` rejected and `[::1]` accepted, which the
// "Go only looks for a closing bracket" reading does not predict — so sweep the shapes.
func httpURLBracketAll() map[string]bool {
	inputs := []string{
		"http://[::1]",
		"http://[::]",
		"http://[0:0:0:0:0:0:0:1]",
		"http://[2001:db8::1]",
		"http://[::ffff:1.2.3.4]",
		"http://[1.2.3.4]",
		"http://[::G]",
		"http://[:::1]",
		"http://[abc]",
		"http://[]",
		"http://[ ]",
		"http://[::1]extra",
		"http://x[::1]",
		"http://[::1]:80",
		"http://[::1]:",
		"http://[::1]:abc",
		"http://[::1%25eth0]",
		"http://[::1%25eth0]:80",
		"http://[::1%eth0]",
		"http://[::1%25]",
		"http://[[::1]]",
		"http://[::1]]",
		"http://[[::1]",
	}
	res := make(map[string]bool, len(inputs))
	for _, in := range inputs {
		res[in] = model.IsValidHTTPURL(in)
	}
	return res
}

// httpURLDiagnostics records, for inputs where the accept/reject answer alone is ambiguous,
// *which* of the two checks in IsValidHTTPURL fired: a ParseRequestURI error, or a parse that
// succeeded with an empty Host. Without this the Rust port has to guess whether `a:1:2` fails
// on the port, the character class, or something else — and guessing is what the oracle exists
// to prevent.
func httpURLDiagnostics() []map[string]any {
	inputs := []string{
		"http://a:1", "http://a:1:2", "http://a::1", "http://a:b", "http://a:",
		"http://:1", "http://:", "http://a:1:", "http://a:12345:6",
		"http://a[b.com", "http://a]b.com", "http://a[b]", "http://a[b]c",
		"http://a[]b", "http://[::1]", "http://[::1]:80", "http://[abc]",
		"http://a<b.com", "http://a>b.com", "http://a\"b.com",
		"http://a%80b.com", "http://a%41b.com", "http://a%25b.com",
		"http://", "http:///p", "http://@x", "http://x@",
		"http://ab.com:80/p", "http://ab.com/p:q", "http://ab.com/p[q]",
	}

	res := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		row := map[string]any{"input": in, "valid": model.IsValidHTTPURL(in)}
		u, err := url.ParseRequestURI(in)
		if err != nil {
			row["parse_error"] = err.Error()
		} else {
			row["parse_error"] = ""
			row["scheme"] = u.Scheme
			row["host"] = u.Host
			row["hostname"] = u.Hostname()
			row["port"] = u.Port()
			row["path"] = u.Path
			row["raw_query"] = u.RawQuery
		}
		res = append(res, row)
	}
	return res
}
