package main

// Behavioural oracle for app/plugin_requests.go's `servePluginRequest` and
// `validateCSRFForPluginRequest`, and for the part of net/http's server that frames what a
// plugin's ServeHTTP wrote — written to fixtures/behaviour_plugin_requests.json and asserted by
// `mm_app::plugin_requests::go_parity`.
//
// The two app functions are unexported and need a server, so their request-side blocks are
// reproduced here line for line (the token order, the scrub, the CSRF check); everything they call
// is the real standard library: http.ReadRequest's header canonicalisation, r.Cookie, r.Cookies,
// AddCookie, URL.Query/Encode/String, ParseForm and FormValue. The framing cases run a real
// httptest server, calling w.Header() before every WriteHeader and Write as the plugin RPC
// client's SyncHeader does, and record what a client receives.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path"
	"path/filepath"
	"strconv"
	"strings"
)

func writePluginRequestsBehaviourFixture(outDir string) error {
	framing, err := pluginFramingAll()
	if err != nil {
		return err
	}
	out := map[string]any{
		"requests": pluginRequestsAll(),
		"csrf":     pluginCSRFAll(),
		"framing":  framing,
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_plugin_requests.json"), append(blob, '\n'), 0o644)
}

// --- the request half -----------------------------------------------------------------------

type pluginRequestCase struct {
	Name    string   `json:"name"`
	Target  string   `json:"target"`
	Subpath string   `json:"subpath"`
	Headers []string `json:"headers"`
}

func readRawRequest(method, target string, headers []string, body string) (*http.Request, error) {
	var raw bytes.Buffer
	raw.WriteString(method + " " + target + " HTTP/1.1\r\nHost: example.com\r\n")
	for _, h := range headers {
		raw.WriteString(h + "\r\n")
	}
	if body != "" {
		raw.WriteString("Content-Length: " + strconv.Itoa(len(body)) + "\r\n")
	}
	raw.WriteString("\r\n")
	raw.WriteString(body)
	return http.ReadRequest(bufio.NewReader(&raw))
}

func pluginRequestsAll() []map[string]any {
	manyCookies := "Cookie: " + strings.Repeat("x=1; ", 3000) + "MMAUTHTOKEN=many"
	cases := []pluginRequestCase{
		{"bearer beats cookie and query", "/plugins/p/x?access_token=q", "/", []string{"Authorization: Bearer tokbearer", "Cookie: MMAUTHTOKEN=ck"}},
		{"bearer in any case", "/plugins/p/x", "/", []string{"Authorization: bEaReR tokmixed"}},
		{"token scheme", "/plugins/p/x", "/", []string{"Authorization: token toktoken", "Cookie: MMAUTHTOKEN=ck"}},
		{"token scheme in capitals", "/plugins/p/x", "/", []string{"Authorization: TOKEN tokcaps"}},
		{"a bare Bearer falls to the cookie", "/plugins/p/x?access_token=q", "/", []string{"Authorization: Bearer ", "Cookie: MMAUTHTOKEN=ck"}},
		{"bearer without the space is not one", "/plugins/p/x", "/", []string{"Authorization: Bearerx", "Cookie: MMAUTHTOKEN=ck"}},
		{"another scheme falls to the cookie", "/plugins/p/x?access_token=q", "/", []string{"Authorization: Basic abc", "Cookie: MMAUTHTOKEN=ck"}},
		{"the kelvin sign folds to k", "/plugins/p/x", "/", []string{"Authorization: toKen abc"}},
		{"cookies are rebuilt without the session", "/plugins/p/x", "/", []string{
			"Cookie: a=1; MMAUTHTOKEN=first; b=\"x y\"; c=has,comma; bad name=1; e=\"q\"; MMAUTHTOKEN=second",
			"Cookie: d=4;; f",
		}},
		{"a quoted session cookie", "/plugins/p/x", "/", []string{"Cookie: MMAUTHTOKEN=\"quoted\""}},
		{"an invalid session cookie is skipped", "/plugins/p/x?access_token=fromquery", "/", []string{"Cookie: MMAUTHTOKEN=a\\b; z=1"}},
		{"an empty session cookie is a cookie", "/plugins/p/x?access_token=fromquery", "/", []string{"Cookie: MMAUTHTOKEN="}},
		{"too many cookies are none", "/plugins/p/x?access_token=fromquery", "/", []string{manyCookies}},
		{"the query token, and the query re-encoded", "/plugins/p/x?b=2&access_token=qq&a=1&a=0&sp=a+b%20c", "/", nil},
		{"a query the parser half refuses", "/plugins/p/x?access_token=q%20x&x=1;y=2&z=%zz&w=3", "/", nil},
		{"no token at all", "/plugins/p/x?y=1", "/", []string{"Accept-Language: fr"}},
		{"the server headers go", "/plugins/p/x", "/", []string{
			"Mattermost-User-Id: spoofed", "mattermost-plugin-id: spoofed", "Referer: http://elsewhere/", "X-Kept: 1", "X-Kept: 2",
		}},
		{"the bare plugin path", "/plugins/p", "/", nil},
		{"the bare plugin path with a query", "/plugins/p?a=1", "/", nil},
		{"a trailing slash", "/plugins/p/", "/", nil},
		{"an escaped slash", "/plugins/p/a%2Fb/c%20d", "/", nil},
		{"a forced empty query", "/plugins/p/x?", "/", nil},
		{"under a subpath", "/sub/plugins/p/x/y?z=1", "/sub", nil},
	}
	var out []map[string]any
	for _, c := range cases {
		r, err := readRawRequest("POST", c.Target, c.Headers, "")
		if err != nil {
			out = append(out, map[string]any{"case": c, "error": err.Error()})
			continue
		}
		token, cookieAuth := pluginRequestToken(r)
		scrubPluginRequest(r, "p", c.Subpath)
		header := map[string][]string{}
		for k, v := range r.Header {
			header[k] = v
		}
		out = append(out, map[string]any{
			"case":        c,
			"token":       token,
			"cookie_auth": cookieAuth,
			"header":      header,
			"url":         r.URL.String(),
			"request_uri": r.RequestURI,
			"host":        r.Host,
		})
	}
	return out
}

// plugin_requests.go:167-185.
func pluginRequestToken(r *http.Request) (string, bool) {
	const headerBearerPrefix = "BEARER" + " "
	const headerTokenPrefix = "token" + " "
	var cookieAuth bool
	var token string
	authHeader := r.Header.Get("Authorization")
	if strings.HasPrefix(strings.ToUpper(authHeader), headerBearerPrefix) {
		token = authHeader[len(headerBearerPrefix):]
	} else if strings.HasPrefix(strings.ToLower(authHeader), headerTokenPrefix) {
		token = authHeader[len(headerTokenPrefix):]
	} else if cookie, _ := r.Cookie("MMAUTHTOKEN"); cookie != nil {
		token = cookie.Value
		cookieAuth = true
	} else {
		token = r.URL.Query().Get("access_token")
	}
	return token, cookieAuth
}

// plugin_requests.go:187-213 and :229.
func scrubPluginRequest(r *http.Request, pluginID, subpath string) {
	r.Header.Del("Mattermost-Plugin-ID")
	r.Header.Del("Mattermost-User-Id")
	cookies := r.Cookies()
	r.Header.Del("Cookie")
	for _, c := range cookies {
		if c.Name != "MMAUTHTOKEN" {
			r.AddCookie(c)
		}
	}
	r.Header.Del("Referer")
	newQuery := r.URL.Query()
	newQuery.Del("access_token")
	r.URL.RawQuery = newQuery.Encode()
	r.URL.Path = strings.TrimPrefix(r.URL.Path, path.Join(subpath, "plugins", pluginID))
}

// --- the CSRF check -------------------------------------------------------------------------

type pluginCSRFCase struct {
	Name        string   `json:"name"`
	Method      string   `json:"method"`
	Target      string   `json:"target"`
	Headers     []string `json:"headers"`
	Body        string   `json:"body"`
	BodyPad     int      `json:"body_pad"`
	SessionCSRF string   `json:"session_csrf"`
	CookieAuth  bool     `json:"cookie_auth"`
	Strict      bool     `json:"strict"`
}

func pluginCSRFAll() []map[string]any {
	const form = "Content-Type: application/x-www-form-urlencoded"
	const tenMB = 10 << 20
	cases := []pluginCSRFCase{
		{"a bearer request is not checked", "POST", "/x", nil, "", 0, "right", false, false},
		{"a cookie GET is not checked", "GET", "/x", nil, "", 0, "right", true, false},
		{"a cookie HEAD is checked", "HEAD", "/x", nil, "", 0, "right", true, false},
		{"the header matches", "POST", "/x", []string{"X-CSRF-Token: right"}, "", 0, "right", true, false},
		{"the header is wrong and the form is not read", "POST", "/x", []string{"X-CSRF-Token: wrong", form}, "csrf=right", 0, "right", true, false},
		{"the form field", "POST", "/x", []string{form}, "a=1&csrf=right", 0, "right", true, false},
		{"the form field under a charset", "POST", "/x", []string{"Content-Type: Application/X-WWW-Form-Urlencoded; charset=utf-8"}, "csrf=right", 0, "right", true, false},
		{"a JSON body is no form", "POST", "/x", []string{"Content-Type: application/json"}, "csrf=right", 0, "right", true, false},
		{"no type is no form", "POST", "/x", nil, "csrf=right", 0, "right", true, false},
		{"a multipart body is not parsed", "POST", "/x", []string{"Content-Type: multipart/form-data; boundary=b"}, "--b\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\nright\r\n--b--\r\n", 0, "right", true, false},
		{"the query field", "POST", "/x?csrf=right", nil, "", 0, "right", true, false},
		{"the body before the query", "POST", "/x?csrf=right", []string{form}, "csrf=wrong", 0, "right", true, false},
		{"a PUT form", "PUT", "/x", []string{form}, "csrf=right", 0, "right", true, false},
		{"a PATCH form", "PATCH", "/x", []string{form}, "csrf=right", 0, "right", true, false},
		{"a DELETE body is not a form", "DELETE", "/x", []string{form}, "csrf=right", 0, "right", true, false},
		{"a DELETE query", "DELETE", "/x?csrf=right", nil, "", 0, "right", true, false},
		{"XMLHttpRequest passes when lenient", "POST", "/x", []string{"X-Requested-With: XMLHttpRequest"}, "", 0, "right", true, false},
		{"XMLHttpRequest fails when strict", "POST", "/x", []string{"X-Requested-With: XMLHttpRequest"}, "", 0, "right", true, true},
		{"XMLHttpRequest is case-sensitive", "POST", "/x", []string{"X-Requested-With: xmlhttprequest"}, "", 0, "right", true, false},
		{"a matching token beats strictness", "POST", "/x", []string{"X-Requested-With: XMLHttpRequest", "X-CSRF-Token: right"}, "", 0, "right", true, true},
		{"no csrf on either side", "POST", "/x", nil, "", 0, "", true, false},
		{"a conflicting duplicate parameter is no type", "POST", "/x", []string{"Content-Type: application/x-www-form-urlencoded; a=1; a=2"}, "csrf=right", 0, "right", true, false},
		{"an equal duplicate parameter is fine", "POST", "/x", []string{"Content-Type: application/x-www-form-urlencoded; a=1; a=\"1\""}, "csrf=right", 0, "right", true, false},
		{"an unparseable parameter keeps the type", "POST", "/x", []string{"Content-Type: application/x-www-form-urlencoded; =x"}, "csrf=right", 0, "right", true, false},
		{"a malformed type is no type", "POST", "/x", []string{"Content-Type: application/x-www-form-urlencoded/extra"}, "csrf=right", 0, "right", true, false},
		{"a bad escape keeps the rest", "POST", "/x", []string{form}, "x=%zz&csrf=right", 0, "right", true, false},
		{"a semicolon pair is dropped", "POST", "/x", []string{form}, "csrf=right;x=1", 0, "right", true, false},
		{"a form of exactly ten megabytes", "POST", "/x", []string{form}, "csrf=right&pad=", tenMB - len("csrf=right&pad="), "right", true, false},
		{"a form past ten megabytes", "POST", "/x", []string{form}, "csrf=right&pad=", tenMB - len("csrf=right&pad=") + 1, "right", true, false},
		{"a form past ten megabytes still has the query", "POST", "/x?csrf=right", []string{form}, "csrf=wrong&pad=", tenMB, "right", true, false},
	}
	var out []map[string]any
	for _, c := range cases {
		body := c.Body + strings.Repeat("a", c.BodyPad)
		r, err := readRawRequest(c.Method, c.Target, c.Headers, body)
		if err != nil {
			out = append(out, map[string]any{"case": c, "error": err.Error()})
			continue
		}
		passed := validateCSRFForPluginRequestCopy(r, c.SessionCSRF, c.CookieAuth, c.Strict)
		handed, _ := io.ReadAll(r.Body)
		prefix := handed
		if len(prefix) > 64 {
			prefix = prefix[:64]
		}
		out = append(out, map[string]any{
			"case":        c,
			"passed":      passed,
			"body_len":    len(handed),
			"body_prefix": string(prefix),
		})
	}
	return out
}

// plugin_requests.go:273-313, logging removed.
func validateCSRFForPluginRequestCopy(r *http.Request, sessionCSRF string, cookieAuth bool, strictCSRFEnforcement bool) bool {
	if !cookieAuth || r.Method == http.MethodGet {
		return true
	}
	csrfTokenFromClient := r.Header.Get("X-CSRF-Token")
	if csrfTokenFromClient == "" {
		bodyBytes, _ := io.ReadAll(r.Body)
		r.Body = io.NopCloser(bytes.NewBuffer(bodyBytes))
		_ = r.ParseForm()
		csrfTokenFromClient = r.FormValue("csrf")
		r.Body = io.NopCloser(bytes.NewBuffer(bodyBytes))
	}
	if csrfTokenFromClient == sessionCSRF {
		return true
	}
	if r.Header.Get("X-Requested-With") == "XMLHttpRequest" {
		return !strictCSRFEnforcement
	}
	return false
}

// --- net/http's framing ---------------------------------------------------------------------

type pluginFramingOp struct {
	// "set" (Key, Value), "status" (Code), "write" (Text, or Repeat bytes of 'a'), "flush".
	Op     string `json:"op"`
	Key    string `json:"key,omitempty"`
	Value  string `json:"value,omitempty"`
	Code   int    `json:"code,omitempty"`
	Text   string `json:"text,omitempty"`
	Repeat int    `json:"repeat,omitempty"`
}

type pluginFramingCase struct {
	Name   string            `json:"name"`
	Method string            `json:"method"`
	Ops    []pluginFramingOp `json:"ops"`
}

func pluginFramingAll() ([]map[string]any, error) {
	set := func(k, v string) pluginFramingOp { return pluginFramingOp{Op: "set", Key: k, Value: v} }
	status := func(c int) pluginFramingOp { return pluginFramingOp{Op: "status", Code: c} }
	write := func(t string) pluginFramingOp { return pluginFramingOp{Op: "write", Text: t} }
	fill := func(n int) pluginFramingOp { return pluginFramingOp{Op: "write", Repeat: n} }
	flush := pluginFramingOp{Op: "flush"}
	cases := []pluginFramingCase{
		{"nothing written", "GET", nil},
		{"a short text body", "GET", []pluginFramingOp{write("hello")}},
		{"html is sniffed", "GET", []pluginFramingOp{write("<html><body>x</body></html>")}},
		{"a JSON body is sniffed as text", "GET", []pluginFramingOp{write(`{"a":1}`)}},
		{"a set type is kept", "GET", []pluginFramingOp{set("Content-Type", "application/json"), write(`{}`)}},
		{"an encoded body is not sniffed", "GET", []pluginFramingOp{set("Content-Encoding", "gzip"), write("abc")}},
		{"a status and nothing else", "GET", []pluginFramingOp{status(418)}},
		{"no content is no body", "GET", []pluginFramingOp{set("Content-Type", "text/plain"), set("Content-Length", "5"), status(204), write("x")}},
		{"not modified keeps no type", "GET", []pluginFramingOp{set("Content-Type", "text/plain"), status(304)}},
		{"exactly the buffer", "GET", []pluginFramingOp{fill(2048)}},
		{"one byte past the buffer", "GET", []pluginFramingOp{fill(2049)}},
		{"two writes past the buffer", "GET", []pluginFramingOp{fill(1000), fill(1049)}},
		{"two writes within the buffer", "GET", []pluginFramingOp{fill(1000), write("<html>")}},
		{"a flush sends the head", "GET", []pluginFramingOp{write("<html>"), flush, write("rest")}},
		{"a flush before any byte", "GET", []pluginFramingOp{flush, write("<html>")}},
		{"a late status is lost", "GET", []pluginFramingOp{write("early"), status(500)}},
		{"a header after the status is lost", "GET", []pluginFramingOp{set("X-A", "1"), status(201), set("X-B", "2"), write("x")}},
		{"a header after the first byte is lost", "GET", []pluginFramingOp{write("x"), set("X-C", "3"), write("y")}},
		{"a declared length is kept", "GET", []pluginFramingOp{set("Content-Length", "3"), write("abc")}},
		{"a declared length on a long body", "GET", []pluginFramingOp{set("Content-Length", "3000"), fill(3000)}},
		{"an informational status does not count", "GET", []pluginFramingOp{status(103), status(202), write("x")}},
		{"a HEAD with nothing", "HEAD", nil},
		{"a HEAD with a body", "HEAD", []pluginFramingOp{write("abc")}},
		{"a HEAD past the buffer", "HEAD", []pluginFramingOp{fill(3000)}},
		{"a HEAD after a flush", "HEAD", []pluginFramingOp{write("abc"), flush}},
	}

	current := make(chan pluginFramingCase, 1)
	errs := make(chan []string, 1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		c := <-current
		var opErrors []string
		for _, op := range c.Ops {
			// The RPC client syncs the header before each call, so Header() precedes it.
			h := w.Header()
			switch op.Op {
			case "set":
				h.Set(op.Key, op.Value)
			case "status":
				w.WriteHeader(op.Code)
			case "write":
				text := []byte(op.Text)
				if op.Repeat > 0 {
					text = bytes.Repeat([]byte("a"), op.Repeat)
				}
				if _, err := w.Write(text); err != nil {
					opErrors = append(opErrors, err.Error())
				}
			case "flush":
				w.(http.Flusher).Flush()
			}
		}
		errs <- opErrors
	}))
	defer server.Close()

	transport := &http.Transport{DisableCompression: true}
	client := &http.Client{Transport: transport}
	var out []map[string]any
	for _, c := range cases {
		current <- c
		req, err := http.NewRequest(c.Method, server.URL+"/x", nil)
		if err != nil {
			return nil, err
		}
		resp, err := client.Do(req)
		if err != nil {
			return nil, err
		}
		body, err := io.ReadAll(resp.Body)
		resp.Body.Close()
		if err != nil {
			return nil, err
		}
		opErrors := <-errs
		header := map[string][]string{}
		for k, v := range resp.Header {
			if k != "Date" {
				header[k] = v
			}
		}
		out = append(out, map[string]any{
			"case":      c,
			"status":    resp.StatusCode,
			"header":    header,
			"chunked":   len(resp.TransferEncoding) > 0 && resp.TransferEncoding[0] == "chunked",
			"body_len":  len(body),
			"body":      string(body[:min(len(body), 64)]),
			"op_errors": opErrors,
		})
	}
	return out, nil
}
