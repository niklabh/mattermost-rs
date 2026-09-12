package main

// Behavioural oracles for the two non-obvious predicates behind the session write family
// (`POST /users/{id}/sessions/revoke`, `/revoke/all`, `PUT /users/sessions/device`), written to
// fixtures/behaviour_session_write.json.
//
// Both call the **real** upstream function rather than a transcription, so there is nothing here
// to keep in sync by hand:
//
//   - `strict_semver` is `semver.StrictNewVersion` from Masterminds/semver/v3, which
//     `handleDeviceProps` (api4/user.go:2729) uses to validate `mobile_version`. Its acceptance
//     set is narrower than "looks like a version" in ways no reader guesses correctly — see the
//     corpus comments.
//   - `url_hostname` is `(*url.URL).Hostname()` from the standard library, the whole body of
//     `App.GetCookieDomain` (app/config.go:191) once the config flag is on.
//
// Derived from the AGPL half of the tree, so these feed `mm-app`'s tests, not `mm-model`'s — the
// rule behaviour_password.go states ([D-031]).

import (
	"encoding/json"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/Masterminds/semver/v3"
)

// strictSemverCorpus targets the decisions a hand-written validator gets wrong. In rough order of
// how surprising each one is:
//
//   - The split is `SplitN(v, ".", 3)`, so "1.2.3.4" is not "too many parts" — it is a patch
//     segment of "3.4", rejected later for containing a dot.
//   - Metadata is stripped **before** the prerelease, on the first "+". So a "-" inside metadata
//     is not a prerelease marker.
//   - A prerelease identifier may not be **empty**, which makes a bare trailing "-" invalid even
//     though a "-" is a legal identifier character.
//   - Leading zeroes are refused in the three numbers *and* in a numeric prerelease identifier,
//     but a metadata identifier may start with zero.
//   - There is a 256-byte length cap and an unsigned 64-bit overflow check, neither of which any
//     regex-shaped implementation has.
//   - No "v" prefix, and no surrounding whitespace: `StrictNewVersion` is the strict variant
//     precisely because `NewVersion` tolerates both.
var strictSemverCorpus = []string{
	"",
	"1",
	"1.2",
	"1.2.3",
	"1.2.3.4",
	"0.0.0",
	"0.0.4",
	"10.20.30",
	"v1.2.3",
	"V1.2.3",
	" 1.2.3",
	"1.2.3 ",
	"1.2.3\n",
	"01.2.3",
	"1.02.3",
	"1.2.03",
	"0.0.01",
	"1.2.x",
	"1.2.*",
	"a.b.c",
	"-1.2.3",
	"1.-2.3",
	"1.2.-3",
	"+1.2.3",
	"1.2.3-",
	"1.2.3+",
	"1.2.3-+",
	"1.2.3+-",
	"1.2.3-beta",
	"1.2.3-beta.1",
	"1.2.3-beta.01",
	"1.2.3-beta.0",
	"1.2.3-0",
	"1.2.3-00",
	"1.2.3-0a",
	"1.2.3-alpha.beta",
	"1.2.3-alpha..1",
	"1.2.3-alpha.",
	"1.2.3-alpha_1",
	"1.2.3-alpha-1",
	"1.2.3--alpha",
	"1.2.3+build",
	"1.2.3+build.1",
	"1.2.3+01",
	"1.2.3+build..1",
	"1.2.3+build_1",
	"1.2.3+build-x",
	"1.2.3-beta+build",
	"1.2.3-beta+bu+ild",
	"1.2.3+bu+ild",
	"1.2.3-rc.1+build.123",
	"1.2.3+21AF26D3----117B344092BD",
	"18446744073709551615.0.0",
	"18446744073709551616.0.0",
	"99999999999999999999.0.0",
	"1.2.3-β",
	"2.34.0",
	"2.34.0-rc1",
	strings.Repeat("1", 250) + ".0.0",
	strings.Repeat("1", 260) + ".0.0",
	// The 256-byte cap, at its boundary and on an input that is otherwise **valid**. The two
	// rows above cannot see the cap at all: a 260-digit major also overflows `ParseUint`, so
	// dropping the length check leaves them rejected for the other reason and the mutation
	// survives. Long metadata is the shape that isolates it — nothing else about these is wrong.
	"1.0.0+" + strings.Repeat("a", 249), // 255 bytes, accepted
	"1.0.0+" + strings.Repeat("a", 250), // 256 bytes exactly, accepted (the test is `> 256`)
	"1.0.0+" + strings.Repeat("a", 251), // 257 bytes, refused on length alone
	"1.0.0+" + strings.Repeat("a", 300), // comfortably over
}

// urlHostnameCorpus is the SiteURL shapes `GetCookieDomain` is handed in practice plus the ones
// that separate a correct `Hostname()` from a naive split on ":". The IPv6 rows are the point:
// brackets come off, and an unbracketed literal keeps every group because its trailing segment is
// not a valid (numeric) port.
var urlHostnameCorpus = []string{
	"",
	"http://localhost:8065",
	"http://localhost",
	"https://mattermost.example.com",
	"https://mattermost.example.com/",
	"https://mattermost.example.com:443/sub",
	"https://sub.domain.example.com:8065",
	"http://127.0.0.1:8065",
	"http://[::1]:8065",
	"http://[::1]",
	"http://[2001:db8::1]:8065/sub",
	"http://example.com:",
	"http://example.com:https",
	"http://user:pw@example.com:8065/sub",
	"http://EXAMPLE.com:8065",
	"http://xn--bcher-kva.example:8065",
	"//example.com/sub",
	"mattermost",
	"/mattermost",
	"http://%zz/sub",
	"http://exa mple.com/sub",
	"http://example.com%2fevil:8065",
}

// splitHostPortCorpus is raw `URL.Host` values — what `Hostname()` is actually handed, which is
// not always something `url.Parse` would have produced. The unbalanced-bracket rows are the
// point: the port split runs *before* the bracket strip, so it will happily cut inside an
// unclosed IPv6 literal.
var splitHostPortCorpus = []string{
	"",
	":",
	":8065",
	"example.com",
	"example.com:",
	"example.com:8065",
	"example.com:https",
	"example.com:80:90",
	"[::1]",
	"[::1]:8065",
	"[::1]:",
	"[2001:db8::1]:8065",
	"::1",
	"[::1",
	"::1]",
	"[]",
	"[]:8065",
	"127.0.0.1:8065",
	"-",
}

// cookieDomainFromSiteURL is app/config.go:191-197 transcribed, with the config flag hoisted to a
// parameter. `url.Parse` and `Hostname` are Go's own; only the three lines of glue are copied, and
// any upstream change to them must be copied here character for character.
func cookieDomainFromSiteURL(allowSubdomains bool, siteURL string) string {
	if allowSubdomains {
		if u, err := url.Parse(siteURL); err == nil {
			return u.Hostname()
		}
	}
	return ""
}

// sessionCookieCase is one `attachDeviceIds` cookie, described by the inputs the handler
// actually varies. `Expires` is pinned to a fixed instant rather than `time.Now()` so the
// fixture is deterministic.
type sessionCookieCase struct {
	Name         string `json:"name"`
	CookieName   string `json:"cookie_name"`
	HttpOnly     bool   `json:"http_only"`
	Token        string `json:"token"`
	Path         string `json:"path"`
	Domain       string `json:"domain"`
	MaxAge       int    `json:"max_age"`
	ExpiresUnix  int64  `json:"expires_unix"`
	Secure       bool   `json:"secure"`
	SameSiteNone bool   `json:"same_site_none"`
	SetCookie    string `json:"set_cookie"`
}

// The instant every cookie row expires at: 2026-09-05T19:28:10Z. Chosen so the rendered weekday,
// day-of-month and month name are all distinctive — a day under 10 would hide a missing zero pad
// and a Sunday would hide a weekday off-by-one.
const cookieExpiresUnix = 1788636490

// sessionCookieCorpus is every combination `attachDeviceIds` can emit, plus the two omission
// cases. `secure=false, same_site_none=true` is unreachable from the handler (`SameSite` is only
// set when `secure` is already true) and is recorded anyway, so the renderer is pinned
// independently of its caller.
var sessionCookieCorpus = []sessionCookieCase{
	{Name: "plain", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "cqjc7ec6bpy65jjamstkhpe6fr", Path: "/", MaxAge: 2592000},
	{Name: "subpath", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/mattermost", MaxAge: 3600},
	{Name: "empty path", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "", MaxAge: 3600},
	{Name: "domain", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/", Domain: "example.com", MaxAge: 3600},
	{Name: "secure", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/", MaxAge: 3600, Secure: true},
	{Name: "secure embedded", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/", MaxAge: 3600, Secure: true, SameSiteNone: true},
	{Name: "embedded not secure", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/", MaxAge: 3600, SameSiteNone: true},
	{Name: "domain and secure embedded", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/sub", Domain: "mm.example.com", MaxAge: 4320 * 3600, Secure: true, SameSiteNone: true},
	{Name: "zero max age", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/", MaxAge: 0},
	{Name: "negative max age", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "tok", Path: "/", MaxAge: -1},

	// `AttachSessionCookies` (app/login.go:270) emits three cookies, and only the first is
	// `HttpOnly`. The user-id and CSRF cookies are read by the webapp is own JavaScript, so the
	// attribute is absent by design rather than by omission — and a port that set it on all
	// three would break the client without failing any status-code assertion.
	{Name: "login token", CookieName: "MMAUTHTOKEN", HttpOnly: true, Token: "mxnmpiimcjbe7ets4qyckj7b3y", Path: "/", MaxAge: 4320 * 3600},
	{Name: "login user id", CookieName: "MMUSERID", Token: "opukwu61f7ft8exssjxf3huyjy", Path: "/", MaxAge: 4320 * 3600},
	{Name: "login csrf", CookieName: "MMCSRF", Token: "f9jnw7z47fbbzmkxt9ba9naene", Path: "/", MaxAge: 4320 * 3600},
	{Name: "login user id secure embedded", CookieName: "MMUSERID", Token: "opukwu61f7ft8exssjxf3huyjy", Path: "/sub", Domain: "mm.example.com", MaxAge: 3600, Secure: true, SameSiteNone: true},
	{Name: "login csrf subpath", CookieName: "MMCSRF", Token: "f9jnw7z47fbbzmkxt9ba9naene", Path: "/mattermost", MaxAge: 3600},
}

// renderSessionCookie is api4/user.go:2789-2807 transcribed: the struct literal `attachDeviceIds`
// builds and the `SameSite` line that follows it, rendered by `net/http`'s own `Cookie.String`.
// `http.SetCookie` is exactly `w.Header().Add("Set-Cookie", c.String())`, so this is the byte
// sequence that reaches the client. Copy any upstream change to that literal character for
// character.
func renderSessionCookie(c sessionCookieCase) string {
	cookie := &http.Cookie{
		Name:     c.CookieName,
		Value:    c.Token,
		Path:     c.Path,
		MaxAge:   c.MaxAge,
		Expires:  time.Unix(cookieExpiresUnix, 0),
		HttpOnly: c.HttpOnly,
		Domain:   c.Domain,
		Secure:   c.Secure,
	}
	if c.SameSiteNone {
		cookie.SameSite = http.SameSiteNoneMode
	}
	return cookie.String()
}

func writeSessionWriteBehaviourFixture(outDir string) error {
	strict := make(map[string]bool, len(strictSemverCorpus))
	for _, in := range strictSemverCorpus {
		_, err := semver.StrictNewVersion(in)
		strict[in] = err == nil
	}

	// Recorded as `Hostname()` on its own and as the full `GetCookieDomain` with the flag on, so
	// the Rust side can assert the split between "what the URL says" and "what the config gate
	// does with it" rather than only their product. The flag-off arm is a constant "" and needs
	// no corpus.
	hostnames := make(map[string]string, len(urlHostnameCorpus))
	for _, in := range urlHostnameCorpus {
		hostnames[in] = cookieDomainFromSiteURL(true, in)
	}

	cookies := make([]sessionCookieCase, 0, len(sessionCookieCorpus))
	for _, c := range sessionCookieCorpus {
		c.ExpiresUnix = cookieExpiresUnix
		c.SetCookie = renderSessionCookie(c)
		cookies = append(cookies, c)
	}

	// `Hostname()` on a `URL` whose `Host` is set directly, so the corpus is not limited to
	// authorities `url.Parse` accepts.
	splits := make(map[string]string, len(splitHostPortCorpus))
	for _, host := range splitHostPortCorpus {
		splits[host] = (&url.URL{Host: host}).Hostname()
	}

	out := map[string]any{
		"strict_semver":   strict,
		"url_hostname":    hostnames,
		"session_cookie":  cookies,
		"split_host_port": splits,
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(
		filepath.Join(outDir, "behaviour_session_write.json"),
		append(blob, '\n'),
		0o644,
	)
}
