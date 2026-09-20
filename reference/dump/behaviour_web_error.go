package main

// Behavioural oracle for **the web error page and the request's translation** — what
// `mm_api::web_error` and `mm_app::i18n` reproduce — written to fixtures/behaviour_web_error.json.
//
// A handler that is not under `/api/` answers its errors with `utils.RenderWebAppError`
// (web/handlers.go:452): a page that sends the browser to `<subpath>/error?message=…&s=<ECDSA
// signature>`. `GET /manualtest` is the first such handler this port serves. Two things decide
// that page's bytes and neither is visible from a live server with a random signature:
//
//   - `render_web_error`: `utils.RenderWebError` (utils/api.go:56), **transcribed** because
//     importing `channels/utils` drags in goldmark, which this generator's go.sum does not carry —
//     the arrangement behaviour_web_static.go explains. Its ingredients are Go's own:
//     `url.Values.Encode`, `path.Join`, `sha256`, `base64.URLEncoding`, and the two escapers
//     `template.JSEscapeString` and `template.HTMLEscapeString`, which are what the corpus is
//     aimed at. Copy any upstream change character for character. The signer is a fake that
//     returns `digest || digest || 0xfb 0xff`, so the recorded body is deterministic **and** pins
//     the signed input: a port that hashed anything else would write a different `s=`.
//   - `translations`: the real `i18n.TranslationsPreInit` over the pinned tree's `i18n/`, then
//     `i18n.InitTranslations("en", <default client locale>)` and
//     `i18n.GetTranslationsAndLocaleFromRequest` — the translate function `web.Handler` picks from
//     `Accept-Language` (handlers.go:191) and `AppError.Translate` applies to the message.
//
// Determinism: fixed corpora, a fixed signer, a fixed key.

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"html/template"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/shared/i18n"
)

// fixedSigner is a crypto.Signer whose signature is `digest || digest || 0xfb 0xff`: the last two
// bytes put `-` and `_` into the URL-safe base64, and the digest halves make the signed input part
// of the output.
type fixedSigner struct{}

func (fixedSigner) Public() crypto.PublicKey { return nil }

func (fixedSigner) Sign(_ io.Reader, digest []byte, _ crypto.SignerOpts) ([]byte, error) {
	out := append([]byte{}, digest...)
	out = append(out, digest...)
	return append(out, 0xfb, 0xff), nil
}

// renderWebError is utils.RenderWebError (utils/api.go:56) with the subpath passed in rather than
// read from a config — `GetSubpathFromConfig` is the caller's half.
func renderWebError(subpath string, w http.ResponseWriter, r *http.Request, status int, params url.Values, s crypto.Signer) {
	queryString := params.Encode()

	h := crypto.SHA256
	sum := h.New()
	sum.Write([]byte(path.Join(subpath, "error") + "?" + queryString))
	signature, err := s.Sign(nil, sum.Sum(nil), h)
	if err != nil {
		http.Error(w, "", http.StatusInternalServerError)
		return
	}
	destination := path.Join(subpath, "error") + "?" + queryString + "&s=" + base64.URLEncoding.EncodeToString(signature)

	if status >= 300 && status < 400 {
		http.Redirect(w, r, destination, status)
		return
	}

	w.Header().Set("Content-Type", "text/html")
	w.WriteHeader(status)
	fmt.Fprintln(w, `<!DOCTYPE html><html><head></head>`)
	fmt.Fprintln(w, `<body onload="window.location = '`+template.HTMLEscapeString(template.JSEscapeString(destination))+`'">`)
	fmt.Fprintln(w, `<noscript><meta http-equiv="refresh" content="0; url=`+template.HTMLEscapeString(destination)+`"></noscript>`)
	fmt.Fprintln(w, `<!-- web error message -->`)
	fmt.Fprintln(w, `<a href="`+template.HTMLEscapeString(destination)+`" style="color: #c0c0c0;">...</a>`)
	fmt.Fprintln(w, `</body></html>`)
}

// Subpaths reach the page raw — `path.Join` cleans but escapes nothing — so they carry the
// characters the two escapers treat specially. The message is query-escaped before either sees it.
var webErrorSubpaths = []string{
	"/", "", "/sub", "/a/b", "/sub/", "/a'b", "/q\"x", "/<t>&=", "/a\\b", "/sp ace",
	"/\u00e9", "/\u2028", "/\x01", "/\x1b", "/\x00", "/\U0001F600", "/\u0378", "/\u00ad", "/a/../b",
}

var webErrorMessages = []string{
	"Unable to parse URL.", "Invalid email.", "N\u00e3o \u00e9 poss\u00edvel", "a&b=c",
	"<script>'\"", "", "line\nbreak", "100% +sure",
}

var translationIDs = []string{
	"manaultesting.manual_test.parse.app_error",
	"manaultesting.test_autolink.unable.app_error",
	"model.team.is_valid.email.app_error",
	"app.team.save.existing.app_error",
	"app.team.save.app_error",
	"api.context.session_expired.app_error",
	"api.context.token_provided.app_error",
	"basic_security_check.url.too_long_error",
	"no.such.translation.id",
}

var acceptLanguages = []string{
	"", "es", "es-ES", "es-419", "pt-BR", "pt-br", "pt", "PT-BR", "zh-CN,en;q=0.8", "zh-TW", "zh",
	"es;q=0.9", " es", "es ,en", "fr-CA,fr;q=0.9", "en-AU", "en-GB", "xx", "de-DE,de", "ja-JP",
	"sv-SE", "-", ",es",
}

// Not only supported locales: `LocalizationSettings.isValid` accepts any value contained in
// `AvailableLocales` (or anything at all when that is empty), and a value that is not a key of the
// loaded files goes through go-i18n's own tag parsing and its **fallback** tags — `pt` and `zh`
// resolve to a regional file, and which of `zh-CN` and `zh-TW` wins is load order.
var defaultClientLocales = []string{"en", "es", "pt-BR", "xx", "en-AU", "", "pt", "zh", "PT_br", "en-GB", "EN", "de,fr", " es", "zh-tw", "fr.x", "xx;de"}

func writeWebErrorBehaviourFixture(outDir string) error {
	var pages []map[string]any
	emit := func(subpath string, status int, message string) {
		rec := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodGet, "/manualtest", nil)
		renderWebError(subpath, rec, req, status, url.Values{"message": []string{message}}, fixedSigner{})
		pages = append(pages, map[string]any{
			"subpath":      subpath,
			"status":       status,
			"message":      message,
			"out_status":   rec.Code,
			"content_type": rec.Header().Get("Content-Type"),
			"body":         rec.Body.String(),
		})
	}
	for _, sp := range webErrorSubpaths {
		for _, msg := range webErrorMessages {
			emit(sp, 400, msg)
		}
	}
	for _, status := range []int{401, 403, 404, 413, 414, 500, 501, 503} {
		emit("/", status, "Invalid email.")
	}

	i18nDir, err := filepath.Abs("../mattermost/server/i18n")
	if err != nil {
		return err
	}
	if err := i18n.TranslationsPreInit(i18nDir); err != nil {
		return fmt.Errorf("TranslationsPreInit: %w", err)
	}
	var translations []map[string]any
	for _, def := range defaultClientLocales {
		if err := i18n.InitTranslations("en", def); err != nil {
			return fmt.Errorf("InitTranslations(%q): %w", def, err)
		}
		for _, al := range acceptLanguages {
			req := httptest.NewRequest(http.MethodGet, "/manualtest", nil)
			if al != "" {
				req.Header.Set("Accept-Language", al)
			}
			t, _ := i18n.GetTranslationsAndLocaleFromRequest(req)
			for _, id := range translationIDs {
				translations = append(translations, map[string]any{
					"default_client_locale": def,
					"accept_language":       al,
					"id":                    id,
					"message":               t(id),
				})
			}
		}
	}
	// Leave the package as another oracle would expect to find it.
	if err := i18n.InitTranslations("en", "en"); err != nil {
		return err
	}

	sum := sha256.Sum256([]byte("/error?message=x"))

	// A signing-key row as `ensureAsymmetricSigningKey` stores it — `json.Marshal` of
	// `model.SystemAsymmetricSigningKey`, big integers bare — for a **fixed** private scalar, so the
	// fixture is stable. (A real `ecdsa` signature is random, so none is recorded: the Rust side signs
	// with this row and verifies with its public half.)
	seed := sha256.Sum256([]byte("mmrs web error key"))
	d := new(big.Int).SetBytes(seed[:])
	d.Mod(d, elliptic.P256().Params().N)
	x, y := elliptic.P256().ScalarBaseMult(d.Bytes())
	priv := &ecdsa.PrivateKey{PublicKey: ecdsa.PublicKey{Curve: elliptic.P256(), X: x, Y: y}, D: d}
	row, err := json.Marshal(&model.SystemAsymmetricSigningKey{ECDSAKey: &model.SystemECDSAKey{Curve: "P-256", X: x, Y: y, D: d}})
	if err != nil {
		return err
	}
	if !priv.PublicKey.Curve.IsOnCurve(x, y) {
		return fmt.Errorf("the fixed signing key is not on P-256")
	}

	out := map[string]any{
		"signing_key_row":  string(row),
		"render_web_error": pages,
		"translations":     translations,
		// One signature spelled out, so the Rust side can check its fake signer is this one.
		"fixed_signature_of_error_message_x": base64.URLEncoding.EncodeToString(append(append(append([]byte{}, sum[:]...), sum[:]...), 0xfb, 0xff)),
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_web_error.json"), append(blob, '\n'), 0o644)
}
