package main

// Behavioural oracle for the **web client's wire format** — what `mm_api::web_static` and
// `mm_api::gzhttp` reproduce — written to fixtures/behaviour_web_static.json.
//
// The live parity suite (`parity::web_client`) compares the two servers on the assets a build
// happens to contain. This corpus pins the branches a build does not decide:
//
//   - `gzhttp`: the real `gzhttp.GzipHandler` (klauspost/compress v1.18.6, the version the pinned
//     server links) around a handler that sets a content type and length and writes a body, served
//     by a **real** `net/http` server so the framing a client sees (`Content-Length` or chunked) is
//     recorded, not `httptest.ResponseRecorder`'s idea of it. One row per (method,
//     Accept-Encoding, status, content type, declared length, body length, Content-Range).
//   - `framing`: a bare handler writing N bytes, GET and HEAD — `net/http`'s 2048-byte rule.
//   - `desktop_app_version`: `app.GetDesktopAppVersion` over user agents.
//   - `client_compatibility` and `static_script_hashes`: `web.CheckClientCompatibility` and
//     `utils.GetStaticScriptHashes`, **transcribed** (static.go / web.go:52, subpath.go:25-62)
//     because importing `channels/web` or `channels/utils` drags in goldmark, which this
//     generator's go.sum does not carry — the same arrangement behaviour_subpath.go explains. The
//     ingredients (`uasurfer.Parse`, `sha256`, `base64`) are Go's own. Copy any upstream change
//     character for character.
//
// Determinism: fixed corpora; compressed bodies are recorded only as "was it compressed, and does
// it decode to the input", never as bytes.

import (
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"github.com/avct/uasurfer"
	"github.com/klauspost/compress/gzhttp"
	"github.com/klauspost/compress/zstd"

	"github.com/mattermost/mattermost/server/v8/channels/app"
)

type gzhttpCase struct {
	Method         string `json:"method"`
	AcceptEncoding string `json:"accept_encoding"`
	Status         int    `json:"status"`
	ContentType    string `json:"content_type"`
	DeclaredLength int    `json:"declared_length"` // 0: no Content-Length set by the handler
	BodyLength     int    `json:"body_length"`
	ContentRange   string `json:"content_range"`
}

type recordedResponse struct {
	Status        int               `json:"status"`
	Headers       map[string]string `json:"headers"`
	Chunked       bool              `json:"chunked"`
	DecodedLength int               `json:"decoded_length"`
	DecodesToBody bool              `json:"decodes_to_body"`
}

func gzhttpCorpus() []gzhttpCase {
	var out []gzhttpCase
	encodings := []string{"", "gzip", "gzip, deflate, br, zstd", "zstd", "br", "gzip;q=0", "gzip;q=0.5, zstd;q=0.4",
		"zstd;q=0.5, gzip", "GZIP", "identity", "gzip;q=0, zstd", "gzip, zstd;q=0"}
	for _, ae := range encodings {
		for _, method := range []string{"GET", "HEAD"} {
			out = append(out, gzhttpCase{method, ae, 200, "text/javascript; charset=utf-8", 5000, 5000, ""})
		}
	}
	types := []string{"text/css; charset=utf-8", "image/png", "image/jpeg", "image/svg+xml", "audio/mpeg",
		"video/mp4", "application/zip", "font/woff2", "application/json", "text/plain; charset=utf-8",
		"application/x-compress", "application/octet-stream"}
	for _, ct := range types {
		out = append(out, gzhttpCase{"GET", "gzip, deflate, br, zstd", 200, ct, 5000, 5000, ""})
	}
	for _, n := range []int{0, 1, 512, 1023, 1024, 1025, 2048, 3000, 200000} {
		out = append(out, gzhttpCase{"GET", "gzip", 200, "text/javascript; charset=utf-8", n, n, ""})
		out = append(out, gzhttpCase{"GET", "gzip", 200, "text/javascript; charset=utf-8", 0, n, ""})
	}
	out = append(out,
		gzhttpCase{"GET", "gzip", 206, "text/javascript; charset=utf-8", 5000, 5000, "bytes 0-4999/9000"},
		gzhttpCase{"GET", "gzip", 304, "", 0, 0, ""},
		gzhttpCase{"GET", "gzip", 404, "text/plain; charset=utf-8", 0, 19, ""},
		gzhttpCase{"GET", "gzip", 404, "text/plain; charset=utf-8", 0, 3000, ""},
		gzhttpCase{"GET", "gzip", 301, "", 0, 0, ""},
		// A declared length and a shorter body: the declared one decides "long enough".
		gzhttpCase{"GET", "gzip", 200, "text/javascript; charset=utf-8", 5000, 500, ""},
	)
	return out
}

// serveOnce runs handler behind a real listener and returns the raw response for one request.
func serveOnce(handler http.Handler, method, acceptEncoding string) (*http.Response, []byte, error) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, nil, err
	}
	srv := &http.Server{Handler: handler}
	go srv.Serve(ln)
	defer srv.Close()

	req, err := http.NewRequest(method, "http://"+ln.Addr().String()+"/x", nil)
	if err != nil {
		return nil, nil, err
	}
	if acceptEncoding != "" {
		req.Header.Set("Accept-Encoding", acceptEncoding)
	}
	tr := &http.Transport{DisableCompression: true}
	defer tr.CloseIdleConnections()
	resp, err := tr.RoundTrip(req)
	if err != nil {
		return nil, nil, err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	return resp, raw, err
}

func record(resp *http.Response, raw []byte, want []byte) recordedResponse {
	// `Header` keeps `Content-Length` exactly when it was sent; `Transfer-Encoding` is moved
	// out of it into `TransferEncoding`.
	headers := map[string]string{}
	for k, v := range resp.Header {
		if k == "Date" {
			continue
		}
		headers[strings.ToLower(k)] = strings.Join(v, ", ")
	}
	decoded := raw
	switch resp.Header.Get("Content-Encoding") {
	case "gzip":
		r, err := gzip.NewReader(bytes.NewReader(raw))
		if err == nil {
			decoded, _ = io.ReadAll(r)
		}
	case "zstd":
		d, err := zstd.NewReader(nil)
		if err == nil {
			decoded, _ = d.DecodeAll(raw, nil)
			d.Close()
		}
	}
	return recordedResponse{
		Status:        resp.StatusCode,
		Headers:       headers,
		Chunked:       len(resp.TransferEncoding) > 0 && resp.TransferEncoding[0] == "chunked",
		DecodedLength: len(decoded),
		DecodesToBody: bytes.Equal(decoded, want),
	}
}

// body returns n bytes of compressible text.
func body(n int) []byte {
	pattern := []byte("function mattermost(){return 'static asset';}\n")
	out := make([]byte, 0, n)
	for len(out) < n {
		out = append(out, pattern...)
	}
	return out[:n]
}

func gzhttpAll() ([]map[string]any, error) {
	var rows []map[string]any
	for _, c := range gzhttpCorpus() {
		c := c
		payload := body(c.BodyLength)
		inner := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if c.ContentType != "" {
				w.Header().Set("Content-Type", c.ContentType)
			}
			if c.DeclaredLength > 0 {
				w.Header().Set("Content-Length", strconv.Itoa(c.DeclaredLength))
			}
			if c.ContentRange != "" {
				w.Header().Set("Content-Range", c.ContentRange)
			}
			w.WriteHeader(c.Status)
			if c.Status != 304 && c.Status != 301 {
				w.Write(payload)
			}
		})
		resp, raw, err := serveOnce(gzhttp.GzipHandler(inner), c.Method, c.AcceptEncoding)
		if err != nil {
			return nil, fmt.Errorf("gzhttp case %+v: %w", c, err)
		}
		want := payload
		if c.Method == "HEAD" || c.Status == 304 || c.Status == 301 {
			want = nil
		}
		rows = append(rows, map[string]any{"case": c, "response": record(resp, raw, want)})
	}
	return rows, nil
}

// gzhttpStreamCase is the API handlers' shape: a handler that may write its body in several
// `Write` calls (`http.ServeContent` copies a file 32 KiB at a time; an encoder writes as it
// goes), may set no Content-Type at all, and may declare a length it streams. `mm_api::gzhttp`
// sees the body as a stream, so these rows pin what the one-write corpus above cannot: the
// buffering up to `MinSize`, the content-type sniff, and `net/http`'s 2048-byte framing rule
// applied to what the compressor emits across many writes.
type gzhttpStreamCase struct {
	Name           string `json:"name"`
	AcceptEncoding string `json:"accept_encoding"`
	ContentType    string `json:"content_type"` // "": the handler sets none
	DeclaredLength int    `json:"declared_length"`
	BodyKind       string `json:"body_kind"` // text | noise | jpeg
	BodyLength     int    `json:"body_length"`
	WriteSize      int    `json:"write_size"` // bytes per Write; 0: one Write
}

func gzhttpStreamCorpus() []gzhttpStreamCase {
	const js = "application/json"
	return []gzhttpStreamCase{
		{"json_below_min_size", "gzip", js, 0, "text", 600, 0},
		{"json_one_write_zstd", "gzip, deflate, br, zstd", js, 0, "text", 5000, 0},
		{"json_many_small_writes", "gzip", js, 0, "text", 5000, 100},
		{"json_no_accept_encoding_streamed", "", js, 0, "text", 5000, 700},
		{"declared_file_compresses_to_a_length", "gzip", "text/plain; charset=utf-8", 50000, "text", 50000, 4096},
		{"undeclared_stream_compresses_to_a_length", "gzip", "text/plain; charset=utf-8", 0, "text", 50000, 4096},
		{"declared_noise_compresses_to_chunks", "gzip", "application/octet-stream", 200000, "noise", 200000, 4096},
		{"declared_jpeg_is_plain_with_its_length", "gzip", "image/jpeg", 200000, "noise", 200000, 4096},
		{"declared_below_min_size_streamed", "gzip", "text/plain; charset=utf-8", 800, "text", 800, 100},
		{"undeclared_plain_stream_totals_a_length", "gzip", "image/jpeg", 0, "noise", 1500, 100},
		{"undeclared_plain_stream_is_chunked", "gzip", "image/jpeg", 0, "noise", 5000, 700},
		{"no_type_text_is_sniffed_and_compressed", "gzip", "", 0, "text", 5000, 0},
		{"no_type_jpeg_is_sniffed_and_left_alone", "gzip", "", 0, "jpeg", 5000, 0},
		{"no_type_below_min_size_is_sniffed_at_close", "gzip", "", 0, "text", 500, 0},
		{"no_type_declared_is_sniffed_on_first_write", "gzip", "", 5000, "jpeg", 5000, 1000},
		{"no_type_streamed_zstd", "zstd", "", 0, "text", 5000, 300},
		{"empty_json_body", "gzip", js, 0, "text", 0, 0},
	}
}

// noise is n deterministic, incompressible bytes: the top byte of a 64-bit LCG (Knuth's MMIX
// constants). `mm_api::gzhttp`'s tests generate the same sequence.
func noise(n int) []byte {
	x := uint64(0x9E3779B97F4A7C15)
	out := make([]byte, n)
	for i := range out {
		x = x*6364136223846793005 + 1442695040888963407
		out[i] = byte(x >> 56)
	}
	return out
}

func streamPayload(kind string, n int) []byte {
	switch kind {
	case "noise":
		return noise(n)
	case "jpeg":
		out := noise(n)
		copy(out, []byte{0xFF, 0xD8, 0xFF, 0xE0})
		return out
	default:
		return body(n)
	}
}

func gzhttpStreamAll() ([]map[string]any, error) {
	var rows []map[string]any
	for _, c := range gzhttpStreamCorpus() {
		c := c
		payload := streamPayload(c.BodyKind, c.BodyLength)
		inner := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if c.ContentType != "" {
				w.Header().Set("Content-Type", c.ContentType)
			}
			if c.DeclaredLength > 0 {
				w.Header().Set("Content-Length", strconv.Itoa(c.DeclaredLength))
			}
			w.WriteHeader(200)
			if c.WriteSize == 0 {
				w.Write(payload)
				return
			}
			for rest := payload; len(rest) > 0; {
				n := min(c.WriteSize, len(rest))
				w.Write(rest[:n])
				rest = rest[n:]
			}
		})
		resp, raw, err := serveOnce(gzhttp.GzipHandler(inner), "GET", c.AcceptEncoding)
		if err != nil {
			return nil, fmt.Errorf("gzhttp stream case %s: %w", c.Name, err)
		}
		rows = append(rows, map[string]any{"case": c, "response": record(resp, raw, payload)})
	}
	return rows, nil
}

func framingAll() ([]map[string]any, error) {
	var rows []map[string]any
	for _, n := range []int{0, 1, 26, 2047, 2048, 2049, 5000} {
		for _, method := range []string{"GET", "HEAD"} {
			payload := body(n)
			inner := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				w.Header().Set("Content-Type", "text/html")
				w.Write(payload)
			})
			resp, _, err := serveOnce(inner, method, "")
			if err != nil {
				return nil, err
			}
			rows = append(rows, map[string]any{
				"method":         method,
				"body_length":    n,
				"content_length": resp.Header.Get("Content-Length"),
				"chunked":        len(resp.TransferEncoding) > 0,
			})
		}
	}
	return rows, nil
}

var webUserAgentCorpus = []string{
	"",
	"Mattermost/5.10.0",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Mattermost/5.10.0 Chrome/126.0.6478.127 Electron/31.2.1 Safari/537.36",
	"Mozilla/5.0 Mattermost/",
	"Mozilla/5.0 XMattermost/5.0",
	"Mattermost Mobile/2.0.0",
	"Mozilla/5.0 Mattermost/5.0\tChrome",
	"Mozilla/5.0 Mattermost/  5.9.0",
	"Mozilla/5.0 (Macintosh) XMattermost/1.0 Mattermost/2.0",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_13_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/11.1.2 Safari/605.1.15",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
	"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_14) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/12.0 Safari/605.1.15",
	"Mozilla/5.0 (iPhone; CPU iPhone OS 11_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148",
	"Mozilla/5.0 (iPhone; CPU iPhone OS 16_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148",
	"Mozilla/5.0 (Windows NT 10.0; WOW64; Trident/7.0; rv:11.0) like Gecko",
	"Mozilla/4.0 (compatible; MSIE 8.0; Windows NT 6.1; Trident/4.0)",
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.102 Safari/537.36 Edge/18.19582",
	"Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
	"Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
	"curl/8.5.0",
	"Googlebot/2.1 (+http://www.google.com/bot.html)",
}

// checkClientCompatibility is web.go:44-60, transcribed.
var browserMinimumSupported = map[string]int{
	"BrowserIE":     12,
	"BrowserSafari": 12,
}

func checkClientCompatibility(agentString string) bool {
	ua := uasurfer.Parse(agentString)
	if version, exist := browserMinimumSupported[ua.Browser.Name.String()]; exist && (ua.Browser.Version.Major < version || version < 0) {
		return false
	}
	return true
}

// getStaticScriptHashes is subpath.go:25-62, transcribed.
func getSubpathScript(subpath string) string {
	if subpath == "" || subpath == "/" {
		return ""
	}
	newPath := path.Join(subpath, "static") + "/"
	return fmt.Sprintf("window.publicPath='%s'", newPath)
}

func getConcurrentReactScript(enableConcurrentReact bool) string {
	if !enableConcurrentReact {
		return ""
	}
	return "window.enableConcurrentReact=true"
}

func getScriptHash(script string) string {
	if script == "" {
		return ""
	}
	scriptHash := sha256.Sum256([]byte(script))
	return fmt.Sprintf(" 'sha256-%s'", base64.StdEncoding.EncodeToString(scriptHash[:]))
}

func getStaticScriptHashes(subpath string, enableConcurrentReact bool) string {
	return getScriptHash(getSubpathScript(subpath)) + getScriptHash(getConcurrentReactScript(enableConcurrentReact))
}

func writeWebStaticBehaviourFixture(outDir string) error {
	gz, err := gzhttpAll()
	if err != nil {
		return err
	}
	gzStream, err := gzhttpStreamAll()
	if err != nil {
		return err
	}
	framing, err := framingAll()
	if err != nil {
		return err
	}
	var desktop []map[string]any
	var compat []map[string]any
	for _, ua := range webUserAgentCorpus {
		v, ok := app.GetDesktopAppVersion(ua)
		desktop = append(desktop, map[string]any{"user_agent": ua, "version": v, "ok": ok})
		compat = append(compat, map[string]any{"user_agent": ua, "compatible": checkClientCompatibility(ua)})
	}
	var hashes []map[string]any
	subpaths := []string{"", "/", "/chat", "/chat/", "/a/b", "/mattermost"}
	sort.Strings(subpaths)
	for _, sp := range subpaths {
		for _, react := range []bool{false, true} {
			hashes = append(hashes, map[string]any{"subpath": sp, "enable_concurrent_react": react, "directive": getStaticScriptHashes(sp, react)})
		}
	}

	out := map[string]any{
		"gzhttp":               gz,
		"gzhttp_stream":        gzStream,
		"framing":              framing,
		"desktop_app_version":  desktop,
		"client_compatibility": compat,
		"static_script_hashes": hashes,
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_web_static.json"), append(blob, '\n'), 0o644)
}
