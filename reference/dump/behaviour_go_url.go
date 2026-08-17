package main

// Behavioural oracle for Go's net/url — the subset model.MergeQueryIntoURL and
// model.IsValidHTTPURL need. Written to fixtures/behaviour_go_url.json.
//
// [D-003] already reproduced ParseRequestURI as a *predicate*, verified over 3,529 inputs. This
// oracle exists because MergeQueryIntoURL needs a *parser*: it takes a URL apart, edits the query
// and puts it back together, and every one of those three steps has its own escaping rules.
//
// Four things need Go's own answer:
//
//  1. **String() is not the identity on the input.** Parse decodes each component and String
//     re-encodes it with the canonical escaping for its position, so `http://x/a%41b` comes back
//     as `http://x/aAb` while `http://x/a%2fb` does not. RawPath and RawFragment are what decide
//     which; the `parse` section records both alongside the round trip.
//
//  2. **shouldEscape's table is per-position and seven positions disagree.** The `escape` section
//     runs all 256 byte values through every mode, which pins the whole table rather than the
//     handful of characters a hand-picked corpus would reach.
//
//  3. **A component can hold bytes no Rust String can.** `unescape("%80", encodePath)` is one
//     byte, 0x80. Every byte-valued field is recorded as []byte — i.e. base64 — so the fixture
//     can carry it and the Rust side can compare Vec<u8> to Vec<u8>.
//
//  4. **ParseQuery keeps what it can and reports the first failure.** URL.Query() discards the
//     error, so one bad escape costs one pair rather than the whole query. Recorded with the
//     error flag beside the surviving pairs.
//
// The error *strings* are recorded as diagnostics only. Nothing in the ported tree reads one, and
// netip.ParseAddr's wording is not worth reproducing — see [D-049].

import (
	"encoding/json"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"sort"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeGoURLBehaviourFixture(outDir string) error {
	out := map[string]any{
		"parse":                goURLParseAll(),
		"parse_request_uri":    goURLParseRequestURIAll(),
		"escape":               goURLEscapeAll(),
		"unescape":             goURLUnescapeAll(),
		"parse_query":          goURLParseQueryAll(),
		"encode":               goURLEncodeAll(),
		"merge_query_into_url": goURLMergeAll(),
		"is_valid_http_url":    goURLIsValidHTTPURLAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_go_url.json"), append(blob, '\n'), 0o644)
}

// goURLCorpus is the shared input list. Every entry is run through both Parse and
// ParseRequestURI, which is where the two differ visibly.
var goURLCorpus = []string{
	// Ordinary.
	"http://example.com",
	"http://example.com/",
	"https://example.com/a/b",
	"https://example.com/a/b?c=d",
	"https://example.com/a/b?c=d&e=f",
	"https://example.com/a/b?c=d#frag",
	"https://example.com/?",
	"https://example.com/??",
	"https://example.com?a=1",
	"https://example.com#f",
	"https://example.com/#f",

	// Scheme handling.
	"HTTPS://EXAMPLE.COM/A",
	"h+t-t.p1://x/y",
	"://x",
	"1http://x",
	"mailto:someone@example.com",
	"cache_object:foo/bar",
	"/plugins/x",
	"plugins/x",
	"./plugins/x",
	"a/b:c/d",
	"a:b/c",
	"",
	"*",
	"//example.com/a",
	"///a",
	"http:///a",
	"http:/a",
	"http:a",

	// Userinfo. The split is at the LAST '@'.
	"http://user@example.com/",
	"http://user:pass@example.com/",
	"http://user:p@ssword@example.com/",
	"http://@example.com/",
	"http://us%65r@example.com/",
	"http://user:@example.com/",
	"http://:pass@example.com/",
	"http://us er@example.com/",
	"http://us%2fer@example.com/",

	// Hosts.
	"http://:1/",
	"http://:/",
	"http://",
	"http://?q",
	"http://x@",
	"http://a:1:2/",
	"ftp://a:1:2/",
	"http://a]b.com/",
	"http://a[b.com/",
	"http://a[b]/",
	"http://[::1]/",
	"http://[::1]:8080/",
	"http://[::ffff:1.2.3.4]/",
	"http://[1.2.3.4]/",
	"http://[abc]/",
	"http://[]/",
	"http://[::1%25eth0]/",
	"http://[::1%25]/",
	"http://[fe80::1%25en0]:80/",
	"http://%80.com/",
	"http://%41.com/",
	"http://a%2eb.com/",
	"http://ex ample.com/",
	"http://exämple.com/",
	"http://EXAMPLE.com/",
	"http://example.com.:80/",
	"http://example.com:/",
	"http://example.com:0/",
	"http://example.com:99999999999999999999/",

	// Paths — where the encoding round trip is decided.
	"http://x/a%41b",
	"http://x/a%2fb",
	"http://x/a%2Fb",
	"http://x/a b",
	"http://x/a+b",
	"http://x/a%20b",
	"http://x/a?b",
	"http://x/a%3fb",
	"http://x/%80",
	"http://x/%c3%a9",
	"http://x/é",
	"http://x/a%zzb",
	"http://x/a%2",
	"http://x/a%",
	"http://x/../y",
	"http://x/./y",
	"http://x//y",
	"http://x/a;b,c=d:e@f",
	"http://x/a[b]c",
	"http://x/a'b(c)d!e*f",

	// Query and fragment.
	"http://x/?a=%zz",
	"http://x/?a=1;b=2",
	"http://x/?a=1&&b=2",
	"http://x/?=1",
	"http://x/?a",
	"http://x/?a=b%20c",
	"http://x/?a=b+c",
	"http://x/#a%20b",
	"http://x/#a b",
	"http://x/#a%zz",
	"http://x/#a#b",
	"http://x/#",

	// Control bytes are rejected everywhere, before parsing starts.
	"http://x/\n",
	"http://x/?a=\t",
	"http://x/\x7f",
}

// --- Parse / ParseRequestURI --------------------------------------------------------------

type goURLParseCase struct {
	Name string `json:"name"`
	Err  string `json:"err"`

	Scheme      string `json:"scheme"`
	Opaque      string `json:"opaque"`
	HasUser     bool   `json:"has_user"`
	Username    []byte `json:"username"`
	Password    []byte `json:"password"`
	PasswordSet bool   `json:"password_set"`
	Host        []byte `json:"host"`
	Path        []byte `json:"path"`
	RawPath     []byte `json:"raw_path"`
	RawQuery    string `json:"raw_query"`
	Fragment    []byte `json:"fragment"`
	RawFragment []byte `json:"raw_fragment"`
	ForceQuery  bool   `json:"force_query"`
	OmitHost    bool   `json:"omit_host"`

	// String is the round trip — the observable MergeQueryIntoURL actually returns.
	String string `json:"string"`
	// EscapedPath and EscapedFragment are recorded because String's use of them is the only
	// thing that makes RawPath/RawFragment visible.
	EscapedPath     []byte `json:"escaped_path"`
	EscapedFragment []byte `json:"escaped_fragment"`
}

func goURLRecord(u *url.URL, err error) goURLParseCase {
	if err != nil {
		return goURLParseCase{Err: err.Error()}
	}
	c := goURLParseCase{
		Scheme:          u.Scheme,
		Opaque:          u.Opaque,
		Host:            []byte(u.Host),
		Path:            []byte(u.Path),
		RawPath:         []byte(u.RawPath),
		RawQuery:        u.RawQuery,
		Fragment:        []byte(u.Fragment),
		RawFragment:     []byte(u.RawFragment),
		ForceQuery:      u.ForceQuery,
		OmitHost:        u.OmitHost,
		String:          u.String(),
		EscapedPath:     []byte(u.EscapedPath()),
		EscapedFragment: []byte(u.EscapedFragment()),
	}
	if u.User != nil {
		password, set := u.User.Password()
		c.HasUser = true
		c.Username = []byte(u.User.Username())
		c.Password = []byte(password)
		c.PasswordSet = set
	}
	return c
}

func goURLParseAll() []goURLParseCase {
	res := make([]goURLParseCase, 0, len(goURLCorpus))
	for _, raw := range goURLCorpus {
		c := goURLRecord(url.Parse(raw))
		c.Name = raw
		res = append(res, c)
	}
	return res
}

func goURLParseRequestURIAll() []goURLParseCase {
	res := make([]goURLParseCase, 0, len(goURLCorpus))
	for _, raw := range goURLCorpus {
		c := goURLRecord(url.ParseRequestURI(raw))
		c.Name = raw
		res = append(res, c)
	}
	return res
}

// --- escape / unescape ---------------------------------------------------------------------

// allBytes is every byte value 0..255, which is what makes the escape section a pin on the whole
// shouldEscape table rather than on the characters a hand-picked corpus happens to contain.
func allBytes() []byte {
	b := make([]byte, 256)
	for i := range b {
		b[i] = byte(i)
	}
	return b
}

type goURLEscapeCase struct {
	Mode string `json:"mode"`
	Out  []byte `json:"out"`
}

// goURLEscapeAll reaches the unexported escape() through its two exported wrappers plus the four
// positions Parse and String drive. QueryEscape and PathEscape are direct; the rest are recovered
// by building a URL whose component holds the bytes and reading String()'s output back, which is
// exactly how the Rust port's escape() is reached too.
func goURLEscapeAll() []goURLEscapeCase {
	in := allBytes()

	// encodeHost, encodeUserPassword and encodeFragment are reachable only through String().
	hostURL := url.URL{Scheme: "s", Host: string(in)}
	host := hostURL.String()[len("s://"):]

	userURL := url.URL{Scheme: "s", Host: "h", User: url.User(string(in))}
	user := userURL.String()[len("s://") : len(userURL.String())-len("@h")]

	fragURL := url.URL{Scheme: "s", Host: "h", Fragment: string(in)}
	frag := fragURL.String()[len("s://h#"):]

	pathURL := url.URL{Scheme: "s", Host: "h", Path: string(in)}
	path := pathURL.EscapedPath()

	return []goURLEscapeCase{
		{"query_component", []byte(url.QueryEscape(string(in)))},
		{"path_segment", []byte(url.PathEscape(string(in)))},
		{"path", []byte(path)},
		{"host", []byte(host)},
		{"user_password", []byte(user)},
		{"fragment", []byte(frag)},
	}
}

type goURLUnescapeCase struct {
	Name string `json:"name"`
	In   string `json:"in"`
	Mode string `json:"mode"`
	Out  []byte `json:"out"`
	Err  string `json:"err"`
	Ok   bool   `json:"ok"`
}

// goURLUnescapeAll drives the two exported unescape wrappers. The host and zone rules — which are
// where the surprises are — are covered by the Parse section instead, since neither mode has an
// exported entry point.
func goURLUnescapeAll() []goURLUnescapeCase {
	cases := []struct{ name, in string }{
		{"plain", "abc"},
		{"escaped_ascii", "a%41b"},
		{"escaped_high", "%80%ff"},
		{"escaped_percent", "%25"},
		{"lowercase_hex", "%c3%a9"},
		{"plus", "a+b"},
		{"plus_and_escape", "a+b%2Bc"},
		{"space_escape", "a%20b"},
		{"bad_short", "a%2"},
		{"bad_trailing", "a%"},
		{"bad_hex", "a%zzb"},
		{"bad_hex_one", "a%2zb"},
		{"empty", ""},
		{"only_percent", "%"},
		{"double_escape", "%2525"},
	}

	res := make([]goURLUnescapeCase, 0, len(cases)*2)
	for _, c := range cases {
		out, err := url.QueryUnescape(c.in)
		res = append(res, goURLUnescapeCase{
			Name: c.name, In: c.in, Mode: "query_component",
			Out: []byte(out), Err: errString(err), Ok: err == nil,
		})
		out, err = url.PathUnescape(c.in)
		res = append(res, goURLUnescapeCase{
			Name: c.name, In: c.in, Mode: "path_segment",
			Out: []byte(out), Err: errString(err), Ok: err == nil,
		})
	}
	return res
}

// --- ParseQuery / Encode ---------------------------------------------------------------------

type goURLQueryPair struct {
	Key    []byte   `json:"key"`
	Values [][]byte `json:"values"`
}

type goURLParseQueryCase struct {
	Name   string           `json:"name"`
	Query  string           `json:"query"`
	Pairs  []goURLQueryPair `json:"pairs"`
	Err    string           `json:"err"`
	Ok     bool             `json:"ok"`
	Encode string           `json:"encode"`
}

func goURLParseQueryAll() []goURLParseQueryCase {
	cases := []struct{ name, query string }{
		{"empty", ""},
		{"one", "a=1"},
		{"two", "a=1&b=2"},
		{"unsorted", "z=1&a=2&m=3"},
		{"repeat", "a=1&a=2"},
		{"no_equals", "a"},
		{"empty_value", "a="},
		{"empty_key", "=1"},
		{"empty_setting", "a=1&&b=2"},
		{"trailing_amp", "a=1&"},
		{"semicolon", "a=1;b=2"},
		{"semicolon_then_good", "a=1;b=2&c=3"},
		{"encoded_semicolon", "a=1%3Bb=2"},
		{"plus_is_space", "a=b+c"},
		{"escaped_space", "a=b%20c"},
		{"bad_escape_key", "a%zz=1&b=2"},
		{"bad_escape_value", "a=%zz&b=2"},
		{"high_bytes", "a=%80"},
		{"equals_in_value", "a=b=c"},
		{"key_with_space", "a+b=1"},
		{"unicode", "ké=vé"},
	}

	res := make([]goURLParseQueryCase, 0, len(cases))
	for _, c := range cases {
		v, err := url.ParseQuery(c.query)
		res = append(res, goURLParseQueryCase{
			Name:   c.name,
			Query:  c.query,
			Pairs:  goURLPairs(v),
			Err:    errString(err),
			Ok:     err == nil,
			Encode: v.Encode(),
		})
	}
	return res
}

func goURLPairs(v url.Values) []goURLQueryPair {
	keys := make([]string, 0, len(v))
	for k := range v {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	pairs := make([]goURLQueryPair, 0, len(keys))
	for _, k := range keys {
		values := make([][]byte, 0, len(v[k]))
		for _, s := range v[k] {
			values = append(values, []byte(s))
		}
		pairs = append(pairs, goURLQueryPair{Key: []byte(k), Values: values})
	}
	return pairs
}

type goURLEncodeCase struct {
	Name  string            `json:"name"`
	Input map[string]string `json:"input"`
	Out   string            `json:"out"`
}

// goURLEncodeAll pins Values.Encode's sort and escaping through Set, which is the shape
// MergeQueryIntoURL uses.
func goURLEncodeAll() []goURLEncodeCase {
	cases := []struct {
		name  string
		input map[string]string
	}{
		{"empty", map[string]string{}},
		{"one", map[string]string{"a": "1"}},
		{"sorted", map[string]string{"z": "1", "a": "2", "M": "3", "_": "4"}},
		{"space_becomes_plus", map[string]string{"a b": "c d"}},
		{"reserved", map[string]string{"a&b": "c=d", "e?f": "g#h"}},
		{"unicode", map[string]string{"ké": "vé"}},
		{"empty_value", map[string]string{"a": ""}},
		{"empty_key", map[string]string{"": "v"}},
		{"plus_literal", map[string]string{"a+b": "c+d"}},
		{"tilde_and_star", map[string]string{"a~b": "c*d"}},
	}

	res := make([]goURLEncodeCase, 0, len(cases))
	for _, c := range cases {
		v := url.Values{}
		for k, val := range c.input {
			v.Set(k, val)
		}
		res = append(res, goURLEncodeCase{Name: c.name, Input: c.input, Out: v.Encode()})
	}
	return res
}

// --- MergeQueryIntoURL / IsValidHTTPURL -------------------------------------------------------

type goURLMergeCase struct {
	Name  string            `json:"name"`
	URL   string            `json:"url"`
	Query map[string]string `json:"query"`
	Out   string            `json:"out"`
	Err   string            `json:"err"`
	Ok    bool              `json:"ok"`
}

func goURLMergeAll() []goURLMergeCase {
	cases := []struct {
		name  string
		url   string
		query map[string]string
	}{
		// An empty map short-circuits: the URL is returned verbatim, un-normalised. This is the
		// most important case in the section, because it is the one where String() is skipped.
		{"empty_map_returns_input_verbatim", "http://x/a%41b?z=1", map[string]string{}},
		{"nil_map_returns_input_verbatim", "not a url at all", nil},

		{"adds_to_empty_query", "https://example.com/hook", map[string]string{"k": "v"}},
		{"adds_to_existing", "https://example.com/hook?a=1", map[string]string{"k": "v"}},
		{"overwrites_existing_key", "https://example.com/hook?k=old", map[string]string{"k": "new"}},
		{"overwrites_repeated_key", "https://example.com/hook?k=1&k=2", map[string]string{"k": "3"}},
		{"sorts_the_result", "https://example.com/hook?z=1", map[string]string{"a": "2"}},
		{"space_in_value", "https://example.com/hook", map[string]string{"k": "a b"}},
		{"reserved_in_value", "https://example.com/hook", map[string]string{"k": "a&b=c"}},
		{"unicode", "https://example.com/hook", map[string]string{"ké": "vé"}},

		// The round trip normalises the rest of the URL, not only the query.
		{"normalises_the_path", "https://example.com/a%41b", map[string]string{"k": "v"}},
		{"keeps_an_escaped_slash", "https://example.com/a%2fb", map[string]string{"k": "v"}},
		{"drops_a_bad_existing_pair", "https://example.com/h?bad=%zz&good=1", map[string]string{"k": "v"}},
		{"semicolon_pair_is_dropped", "https://example.com/h?a=1;b=2", map[string]string{"k": "v"}},
		{"force_query_is_replaced", "https://example.com/h?", map[string]string{"k": "v"}},
		{"fragment_survives", "https://example.com/h#frag", map[string]string{"k": "v"}},
		{"userinfo_survives", "https://u:p@example.com/h", map[string]string{"k": "v"}},
		{"port_survives", "https://example.com:8443/h", map[string]string{"k": "v"}},
		{"ipv6_host", "https://[::1]:8443/h", map[string]string{"k": "v"}},

		// Relative and plugin-shaped URLs: Parse accepts them where ParseRequestURI would not.
		{"plugin_relative", "/plugins/com.example/hook", map[string]string{"k": "v"}},
		{"rootless_relative", "plugins/hook", map[string]string{"k": "v"}},
		{"opaque", "mailto:someone@example.com", map[string]string{"k": "v"}},

		// The error path — this is what makes GetAction return nil.
		{"control_character", "https://example.com/\n", map[string]string{"k": "v"}},
		{"bad_host", "https://a[b/", map[string]string{"k": "v"}},
		{"bad_port", "https://a:1:2/", map[string]string{"k": "v"}},
		{"bad_path_escape", "https://x/a%zz", map[string]string{"k": "v"}},
		{"empty_url", "", map[string]string{"k": "v"}},
	}

	res := make([]goURLMergeCase, 0, len(cases))
	for _, c := range cases {
		out, err := model.MergeQueryIntoURL(c.url, c.query)
		res = append(res, goURLMergeCase{
			Name: c.name, URL: c.url, Query: c.query,
			Out: out, Err: errString(err), Ok: err == nil,
		})
	}
	return res
}

type goURLIsValidCase struct {
	Name  string `json:"name"`
	Valid bool   `json:"valid"`
}

// goURLIsValidHTTPURLAll re-runs the shared corpus through the model predicate. The dedicated
// 3,529-case corpus lives in behaviour_url.json and stays there; this section exists so the same
// inputs the parser section records components for also record the predicate's answer, which is
// what lets the Rust port rebuild IsValidHTTPURL on top of the parser and prove it did not drift.
func goURLIsValidHTTPURLAll() []goURLIsValidCase {
	res := make([]goURLIsValidCase, 0, len(goURLCorpus))
	for _, raw := range goURLCorpus {
		res = append(res, goURLIsValidCase{Name: raw, Valid: model.IsValidHTTPURL(raw)})
	}
	return res
}

var _ = fmt.Sprintf
