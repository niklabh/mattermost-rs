package main

// Behavioural oracle for model/oauth_dcr.go, written to fixtures/behaviour_oauth_dcr.json.
//
// This is OAuth Dynamic Client Registration, and most of it is **redirect-URI allowlist
// matching** — which is where open-redirect vulnerabilities live. A glob matcher that is one
// case too permissive hands an attacker a token; one case too strict breaks a working client.
// Neither failure is visible without driving the real thing, so the corpus is deliberately
// large and adversarial rather than illustrative.
//
// Four things worth naming up front.
//
// # An empty allowlist permits everything
//
//	func RedirectURIMatchesAllowlist(uri string, allowlist []string) bool {
//	    if len(allowlist) == 0 {
//	        return true
//	    }
//
// "No restriction" is the default, not "deny". A port that fails closed here would break every
// deployment that has not configured an allowlist; one that gets the emptiness test wrong —
// treating a slice of blanks as non-empty, say — would deny everything. Both directions are
// driven.
//
// # The matcher works on BYTES
//
// `redirectURIMatchesGlobRecur` indexes `uri[ui]` and `pattern[pi]`, so a multi-byte character is
// several positions and `*` can match a fragment of one. Driven with non-ASCII input.
//
// # `*` stops at `/` and `**` does not
//
// That is the whole component-awareness claim, and it is enforced per URL component: host, path
// and query are matched separately, so a host wildcard cannot satisfy a path requirement.
//
// # Pattern validation substitutes placeholders before parsing
//
// `IsValidDCRRedirectURIPattern` replaces `**` and then `*` with the digit `1` and parses the
// result, so `https://localhost:*` is validated as `https://localhost:1`. The digit is chosen so
// a wildcarded *port* still parses. `***` is rejected outright before any of that.
//
// Determinism: fixed values only. See [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeOAuthDCRBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":      dcrConstants(),
		"keys":           dcrKeys(),
		"wire":           dcrWireAll(),
		"defaults":       dcrDefaultsAll(),
		"request_valid":  dcrRequestIsValidAll(),
		"pattern_valid":  dcrPatternValidAll(),
		"glob":           dcrGlobAll(),
		"allowlist":      dcrAllowlistAll(),
		"dcr_error":      dcrErrorAll(),
		"glob_generated": dcrGlobGeneratedAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_oauth_dcr.json"), append(blob, '\n'), 0o644)
}

func dcrConstants() map[string]any {
	return map[string]any{
		"DCRErrorInvalidRedirectURI":    model.DCRErrorInvalidRedirectURI,
		"DCRErrorInvalidClientMetadata": model.DCRErrorInvalidClientMetadata,
		"DCRErrorUnsupportedOperation":  model.DCRErrorUnsupportedOperation,
		// Borrowed from oauth_metadata.go, which this port has not reached.
		"GrantTypeAuthorizationCode": model.GrantTypeAuthorizationCode,
		"GrantTypeRefreshToken":      model.GrantTypeRefreshToken,
		"ResponseTypeCode":           model.ResponseTypeCode,
	}
}

func dcrKeys() map[string]any {
	return map[string]any{
		"request":  expectedKeys(reflect.TypeOf(model.ClientRegistrationRequest{})),
		"response": expectedKeys(reflect.TypeOf(model.ClientRegistrationResponse{})),
		"error":    expectedKeys(reflect.TypeOf(model.DCRError{})),
	}
}

func dcrStr(v string) *string { return &v }

// --- the wire format -------------------------------------------------------------------------

func dcrWireAll() []map[string]any {
	requests := []struct {
		name string
		in   model.ClientRegistrationRequest
	}{
		// Three of four fields are omitempty pointers, so the zero value is a single key with a
		// null — `redirect_uris` has no omitempty and a nil slice is not dropped.
		{"request_zero", model.ClientRegistrationRequest{}},
		{"request_full", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: dcrStr(model.ClientAuthMethodNone),
			ClientName:              dcrStr("My Client"),
			ClientURI:               dcrStr("https://example.com"),
		}},
		// A pointer to the empty string is NOT nil: the key survives with "".
		{"request_empty_pointers", model.ClientRegistrationRequest{
			RedirectURIs:            []string{},
			TokenEndpointAuthMethod: dcrStr(""),
			ClientName:              dcrStr(""),
			ClientURI:               dcrStr(""),
		}},
	}

	out := make([]map[string]any, 0)
	for _, c := range requests {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}

	responses := []struct {
		name string
		in   model.ClientRegistrationResponse
	}{
		{"response_zero", model.ClientRegistrationResponse{}},
		{"response_public", model.ClientRegistrationResponse{
			ClientID:                "y9i4er48tt8bukijy7i3u5y9ar",
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: model.ClientAuthMethodNone,
			GrantTypes:              model.GetDefaultGrantTypes(),
			ResponseTypes:           model.GetDefaultResponseTypes(),
			Scope:                   model.ScopeUser,
			ClientName:              dcrStr("My Client"),
		}},
		{"response_confidential", model.ClientRegistrationResponse{
			ClientID:                "y9i4er48tt8bukijy7i3u5y9ar",
			ClientSecret:            dcrStr("a-secret"),
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: model.ClientAuthMethodClientSecretPost,
			GrantTypes:              model.GetDefaultGrantTypes(),
			ResponseTypes:           model.GetDefaultResponseTypes(),
			Scope:                   model.ScopeUser,
			ClientURI:               dcrStr("https://example.com"),
		}},
	}
	for _, c := range responses {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}

	errs := []struct {
		name string
		in   model.DCRError
	}{
		{"error_full", *model.NewDCRError(model.DCRErrorInvalidRedirectURI, "bad uri")},
		// ErrorDescription is omitempty; Error is not.
		{"error_no_description", *model.NewDCRError(model.DCRErrorUnsupportedOperation, "")},
		{"error_zero", model.DCRError{}},
	}
	for _, c := range errs {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}

	return out
}

func dcrDefaultsAll() map[string]any {
	return map[string]any{
		"grant_types":    model.GetDefaultGrantTypes(),
		"response_types": model.GetDefaultResponseTypes(),
	}
}

func dcrErrorAll() []map[string]any {
	e := model.NewDCRError("some_type", "some description")
	return []map[string]any{{
		"name":              "new_dcr_error",
		"error":             e.Error,
		"error_description": e.ErrorDescription,
	}}
}

// --- ClientRegistrationRequest.IsValid -------------------------------------------------------

func dcrRequestIsValidAll() []map[string]any {
	long := func(n int) string {
		out := make([]byte, n)
		for i := range out {
			out[i] = 'a'
		}
		return string(out)
	}

	corpus := []struct {
		name string
		in   model.ClientRegistrationRequest
	}{
		{"valid_minimal", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
		}},
		{"no_redirect_uris", model.ClientRegistrationRequest{}},
		{"empty_redirect_uris", model.ClientRegistrationRequest{RedirectURIs: []string{}}},
		{"bad_redirect_uri", model.ClientRegistrationRequest{
			RedirectURIs: []string{"not a url"},
		}},
		{"second_redirect_uri_bad", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://ok.example.com", "nope"},
		}},
		{"client_name_at_cap", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientName:   dcrStr(long(64)),
		}},
		{"client_name_over_cap", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientName:   dcrStr(long(65)),
		}},
		// A nil ClientName skips the check entirely; a pointer to "" does not, and passes.
		{"client_name_empty_pointer", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientName:   dcrStr(""),
		}},
		{"client_uri_bad_format", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientURI:    dcrStr("not a url"),
		}},
		// ORDER MATTERS: the format check runs BEFORE the length check, so an over-long value
		// that is also malformed reports *format*, not length.
		{"client_uri_too_long_and_malformed", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientURI:    dcrStr("not a url " + long(300)),
		}},
		// ...and an over-long value that IS a valid URL reports length.
		{"client_uri_too_long_but_valid", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientURI:    dcrStr("https://example.com/" + long(256)),
		}},
		{"client_uri_at_cap", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientURI:    dcrStr("https://e.com/" + long(256-len("https://e.com/"))),
		}},
		{"auth_method_none", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: dcrStr(model.ClientAuthMethodNone),
		}},
		{"auth_method_secret_post", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: dcrStr(model.ClientAuthMethodClientSecretPost),
		}},
		{"auth_method_unsupported", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: dcrStr("client_secret_basic"),
		}},
		{"auth_method_empty_pointer", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: dcrStr(""),
		}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		err := in.IsValid()
		entry := map[string]any{"name": c.name}
		if err == nil {
			entry["ok"] = true
		} else {
			entry["ok"] = false
			entry["id"] = err.Id
			entry["where"] = err.Where
			entry["status"] = err.StatusCode
			entry["detailed_error"] = err.DetailedError
		}
		out = append(out, entry)
	}
	return out
}

// --- IsValidDCRRedirectURIPattern ------------------------------------------------------------

func dcrPatternValidAll() []map[string]any {
	patterns := []string{
		"https://example.com/cb",
		"http://localhost:8080/cb",
		// Just at the documented minimum lengths.
		"https://x",
		"http://x",
		"https://",
		"http://",
		"",
		// Wrong scheme, or none.
		"ftp://example.com",
		"example.com",
		"//example.com",
		"HTTPS://example.com",
		// Wildcards.
		"https://*.example.com/cb",
		"https://example.com/**",
		"https://example.com/*/cb",
		"https://localhost:*/cb",
		"https://example.com/**/deep/**",
		// Three or more stars are rejected outright.
		"https://example.com/***",
		"https://example.com/****",
		// Control characters are rejected.
		"https://example.com/\x00",
		"https://example.com/\x1f",
		"https://example.com/\x7f",
		"https://example.com/\ttab",
		// A space is not a control character, but the URL parse rejects it.
		"https://example.com/a b",
		// Query wildcards.
		"https://example.com/cb?x=*",
		"https://example.com/cb?*",
	}

	out := make([]map[string]any, 0, len(patterns))
	for _, p := range patterns {
		out = append(out, map[string]any{
			"pattern": p,
			"valid":   model.IsValidDCRRedirectURIPattern(p),
		})
	}
	return out
}

// --- RedirectURIMatchesGlob ------------------------------------------------------------------

func dcrGlobAll() []map[string]any {
	corpus := []struct {
		uri     string
		pattern string
	}{
		// Exact.
		{"https://example.com/cb", "https://example.com/cb"},
		{"https://example.com/cb", "https://example.com/other"},
		// Scheme must match exactly, and is compared after parsing so case is normalised by the
		// parser rather than by this function.
		{"http://example.com/cb", "https://example.com/cb"},
		{"HTTPS://example.com/cb", "https://example.com/cb"},
		// Host wildcards.
		{"https://a.example.com/cb", "https://*.example.com/cb"},
		{"https://a.b.example.com/cb", "https://*.example.com/cb"},
		{"https://example.com/cb", "https://*.example.com/cb"},
		// A host wildcard must not satisfy the path — the component-awareness claim.
		{"https://example.com/evil", "https://*/cb"},
		{"https://example.com/cb", "https://*/cb"},
		// `*` does not cross `/`.
		{"https://example.com/a/b", "https://example.com/*"},
		{"https://example.com/a", "https://example.com/*"},
		{"https://example.com/", "https://example.com/*"},
		// `**` does.
		{"https://example.com/a/b", "https://example.com/**"},
		{"https://example.com/a/b/c/d", "https://example.com/**"},
		{"https://example.com/", "https://example.com/**"},
		// `**` in the middle.
		{"https://example.com/x/y/deep/z", "https://example.com/**/deep/**"},
		{"https://example.com/deep/", "https://example.com/**/deep/**"},
		// Query semantics: an empty pattern query requires an empty candidate query.
		{"https://example.com/cb", "https://example.com/cb"},
		{"https://example.com/cb?a=1", "https://example.com/cb"},
		{"https://example.com/cb", "https://example.com/cb?a=1"},
		{"https://example.com/cb?a=1", "https://example.com/cb?a=1"},
		{"https://example.com/cb?a=2", "https://example.com/cb?a=1"},
		{"https://example.com/cb?a=2", "https://example.com/cb?a=*"},
		{"https://example.com/cb?a=1&b=2", "https://example.com/cb?*"},
		// Ports.
		{"http://localhost:8080/cb", "http://localhost:*/cb"},
		{"http://localhost/cb", "http://localhost:*/cb"},
		{"http://localhost:8080/cb", "http://localhost:8080/cb"},
		// Trailing-slash sensitivity.
		{"https://example.com/cb/", "https://example.com/cb"},
		{"https://example.com/cb", "https://example.com/cb/"},
		// Case sensitivity of the path.
		{"https://example.com/CB", "https://example.com/cb"},
		// Percent-encoding: the candidate's EscapedPath is matched, so the pattern must use the
		// same encoding.
		{"https://example.com/a%20b", "https://example.com/a%20b"},
		{"https://example.com/a%20b", "https://example.com/a b"},
		// Byte-wise matching: `*` can match part of a multi-byte character.
		{"https://example.com/café", "https://example.com/caf*"},
		{"https://example.com/café", "https://example.com/café"},
		// An invalid pattern makes everything fail, whatever the URI.
		{"https://example.com/cb", "***"},
		{"https://example.com/cb", "ftp://example.com/cb"},
		{"https://example.com/cb", ""},
		// An unparseable candidate fails too.
		{"not a url", "https://example.com/cb"},
		{"", "https://example.com/cb"},
		{"/relative/only", "https://example.com/cb"},
		// A candidate with no host.
		{"https:///cb", "https://*/cb"},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		out = append(out, map[string]any{
			"uri":     c.uri,
			"pattern": c.pattern,
			"matches": model.RedirectURIMatchesGlob(c.uri, c.pattern),
		})
	}
	return out
}

// --- RedirectURIMatchesAllowlist -------------------------------------------------------------

func dcrAllowlistAll() []map[string]any {
	corpus := []struct {
		name      string
		uri       string
		allowlist []string
	}{
		// The permissive default: no allowlist means no restriction.
		{"nil_allowlist", "https://anything.example.com/cb", nil},
		{"empty_allowlist", "https://anything.example.com/cb", []string{}},
		// A slice of blanks is NOT empty, so the restriction applies — and nothing matches,
		// because each entry is skipped after trimming. This denies everything.
		{"blank_entries_only", "https://example.com/cb", []string{"", "   ", "\t"}},
		{"single_match", "https://example.com/cb", []string{"https://example.com/cb"}},
		{"single_miss", "https://evil.example.com/cb", []string{"https://example.com/cb"}},
		{"second_matches", "https://b.example.com/cb", []string{
			"https://a.example.com/cb", "https://b.example.com/cb",
		}},
		// Entries are trimmed before matching, so surrounding whitespace is tolerated.
		{"entry_needs_trimming", "https://example.com/cb", []string{"  https://example.com/cb  "}},
		{"blank_then_match", "https://example.com/cb", []string{"", "https://example.com/cb"}},
		// An invalid pattern in the list never matches but does not disable the list either.
		{"invalid_pattern_then_match", "https://example.com/cb", []string{
			"***", "https://example.com/cb",
		}},
		{"only_invalid_patterns", "https://example.com/cb", []string{"***", "ftp://x"}},
		{"wildcard_entry", "https://a.example.com/cb", []string{"https://*.example.com/cb"}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		out = append(out, map[string]any{
			"name":    c.name,
			"uri":     c.uri,
			"allowed": model.RedirectURIMatchesAllowlist(c.uri, c.allowlist),
		})
	}
	return out
}

// --- the generated sweep ----------------------------------------------------------------------

// dcrGlobGeneratedAll is the systematic half of the glob corpus.
//
// The hand-written cases above are adversarial but finite, and a security boundary deserves the
// treatment IsValidHTTPURL got in [D-003]: enumerate the input space rather than imagining it.
// This crosses every URI built from a small component alphabet with every pattern built from a
// small wildcard alphabet — 3,240 pairs — and records Go's answer for each.
//
// Generation is **systematic, not random**: no rand, no seed, so the output is byte-identical on
// every run and the fixture diff stays empty unless behaviour actually changed. See [D-032].
//
// The alphabets are chosen so the interesting interactions are all reachable: a host that is a
// subdomain (does `*` cross a dot?), a path with several segments (does `*` cross a slash?), a
// percent-encoded path (is EscapedPath matched or the decoded form?), a multi-byte path (is
// matching byte-wise?), a port (does a wildcard cover it?), and every combination of present and
// absent query on both sides.
func dcrGlobGeneratedAll() []map[string]any {
	uriHosts := []string{"example.com", "a.example.com", "localhost:8080"}
	uriPaths := []string{"/", "/a", "/a/b", "/a%20b", "/café"}
	uriQueries := []string{"", "?x=1", "?x=2&y=3"}

	patternHosts := []string{"example.com", "*.example.com", "*", "localhost:*"}
	patternPaths := []string{"/", "/a", "/*", "/**", "/a/*", "/**/b"}
	patternQueries := []string{"", "?x=1", "?*"}

	uris := make([]string, 0, len(uriHosts)*len(uriPaths)*len(uriQueries))
	for _, h := range uriHosts {
		for _, p := range uriPaths {
			for _, q := range uriQueries {
				uris = append(uris, "https://"+h+p+q)
			}
		}
	}

	patterns := make([]string, 0, len(patternHosts)*len(patternPaths)*len(patternQueries))
	for _, h := range patternHosts {
		for _, p := range patternPaths {
			for _, q := range patternQueries {
				patterns = append(patterns, "https://"+h+p+q)
			}
		}
	}

	out := make([]map[string]any, 0, len(uris)*len(patterns))
	for _, uri := range uris {
		for _, pattern := range patterns {
			out = append(out, map[string]any{
				"uri":     uri,
				"pattern": pattern,
				"matches": model.RedirectURIMatchesGlob(uri, pattern),
			})
		}
	}
	return out
}
