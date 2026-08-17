package main

// Behavioural oracle for model/oauth.go, written to fixtures/behaviour_oauth.json.
//
// Fourteen functions, and three of them do something a careful reading gets wrong.
//
// # The callback length limit measures Go's slice formatting, not the URLs
//
//	if len(a.CallbackUrls) == 0 || len(fmt.Sprintf("%s", a.CallbackUrls)) > 1024 {
//
// `CallbackUrls` is a `StringArray` — a `[]string` — and `%s` on a slice renders it as
// `[first second third]`: square brackets, space-separated, no quotes and no commas. So the cap
// is on that rendering, which is the sum of the URL lengths **plus one separator between each
// plus two brackets**. A port that sums the URLs accepts payloads Go rejects at the boundary.
// The corpus records the rendered string and its length so the arithmetic is measured.
//
// # Name is capped in BYTES, Description in RUNES, in the same function
//
//	if a.Name == "" || len(a.Name) > 64 { ... }
//	if utf8.RuneCountInString(a.Description) > 512 { ... }
//
// Every other cap here — ClientSecret 128, Homepage 256, IconURL 512, MattermostAppID 32 — is
// `len`, i.e. bytes. Description is the only one counting runes. The corpus drives multi-byte
// strings at each boundary so the two rules are distinguished rather than assumed uniform.
//
// # Auditable emits a key with a trailing colon
//
//	"callback_urls:": a.CallbackUrls,
//
// A typo, and audit consumers read that key. Reproduced.
//
// # IsDynamicallyRegistered exempts two checks
//
// CreatorId and the empty-Homepage check are both skipped for a dynamically registered app, so
// the same object is valid or invalid depending on that one bool. Driven both ways.
//
// Determinism: PreSave calls NewId and GetMillis, so that section records invariants rather than
// values — the behaviour_custom_status.go pattern. See [D-032].

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeOAuthBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":         oauthConstants(),
		"keys":              oauthKeys(),
		"wire":              oauthWireAll(),
		"callback_format":   oauthCallbackFormatAll(),
		"is_valid":          oauthIsValidAll(),
		"pre_save":          oauthPreSaveAll(),
		"etag":              oauthEtagAll(),
		"sanitize":          oauthSanitizeAll(),
		"redirect_url":      oauthRedirectURLAll(),
		"auth_method":       oauthAuthMethodAll(),
		"validate_grant":    oauthValidateGrantAll(),
		"auditable":         oauthAuditableAll(),
		"from_registration": oauthFromRegistrationAll(),
		"to_registration":   oauthToRegistrationAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_oauth.json"), append(blob, '\n'), 0o644)
}

func oauthConstants() map[string]any {
	return map[string]any{
		"OAuthActionSignup":     model.OAuthActionSignup,
		"OAuthActionLogin":      model.OAuthActionLogin,
		"OAuthActionEmailToSSO": model.OAuthActionEmailToSSO,
		"OAuthActionSSOToEmail": model.OAuthActionSSOToEmail,
		"OAuthActionMobile":     model.OAuthActionMobile,
		// Borrowed from files this port has not reached: access.go and oauth_metadata.go.
		"AccessTokenGrantType":             model.AccessTokenGrantType,
		"RefreshTokenGrantType":            model.RefreshTokenGrantType,
		"ClientAuthMethodNone":             model.ClientAuthMethodNone,
		"ClientAuthMethodClientSecretPost": model.ClientAuthMethodClientSecretPost,
		"ScopeUser":                        model.ScopeUser,
	}
}

func oauthKeys() map[string]any {
	return map[string]any{
		"app":          expectedKeys(reflect.TypeOf(model.OAuthApp{})),
		"app_request":  expectedKeys(reflect.TypeOf(model.OAuthAppRequest{})),
		"intune_login": expectedKeys(reflect.TypeOf(model.IntuneLoginRequest{})),
	}
}

// --- helpers ---------------------------------------------------------------------------------

func validOAuthApp() model.OAuthApp {
	return model.OAuthApp{
		Id:              "y9i4er48tt8bukijy7i3u5y9ar",
		CreatorId:       "aaaaaaaaaaaaaaaaaaaaaaaaaa",
		CreateAt:        1600000000000,
		UpdateAt:        1650000000000,
		ClientSecret:    "a-client-secret",
		Name:            "My OAuth App",
		Description:     "does oauth things",
		IconURL:         "https://example.com/icon.png",
		CallbackUrls:    model.StringArray{"https://example.com/callback"},
		Homepage:        "https://example.com",
		IsTrusted:       true,
		MattermostAppID: "mmapp",
	}
}

func oauthRunes(n int) string {
	out := make([]rune, n)
	for i := range out {
		out[i] = 'a'
	}
	return string(out)
}

func oauthMultibyte(n int) string {
	out := make([]rune, n)
	for i := range out {
		out[i] = 'é' // two bytes each
	}
	return string(out)
}

// --- the wire format -------------------------------------------------------------------------

func oauthWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.OAuthApp
	}{
		// Only IsDynamicallyRegistered carries omitempty, so the zero value emits twelve keys —
		// including `callback_urls: null`, since a nil StringArray is not dropped.
		{"zero", model.OAuthApp{}},
		{"full", validOAuthApp()},
		{"dynamically_registered", func() model.OAuthApp {
			a := validOAuthApp()
			a.IsDynamicallyRegistered = true
			return a
		}()},
		{"empty_callbacks", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = model.StringArray{}
			return a
		}()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}

	req := model.OAuthAppRequest{
		Name:         "req",
		Description:  "d",
		IconURL:      "https://example.com/i.png",
		CallbackUrls: model.StringArray{"https://example.com/cb"},
		Homepage:     "https://example.com",
		IsTrusted:    true,
		IsPublic:     true,
	}
	reqBlob, err := json.Marshal(&req)
	if err != nil {
		panic(err)
	}
	intune := model.IntuneLoginRequest{AccessToken: "tok", DeviceId: "dev", VoIPDeviceId: "voip"}
	intuneBlob, err := json.Marshal(&intune)
	if err != nil {
		panic(err)
	}
	intuneZeroBlob, err := json.Marshal(&model.IntuneLoginRequest{})
	if err != nil {
		panic(err)
	}

	out = append(out,
		map[string]any{"name": "app_request", "json": string(reqBlob)},
		map[string]any{"name": "intune_login", "json": string(intuneBlob)},
		// VoIPDeviceId is the only omitempty field on IntuneLoginRequest.
		map[string]any{"name": "intune_login_zero", "json": string(intuneZeroBlob)},
	)
	return out
}

// --- the %s rendering the callback cap measures ----------------------------------------------

func oauthCallbackFormatAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.StringArray
	}{
		{"nil", nil},
		{"empty", model.StringArray{}},
		{"one", model.StringArray{"https://example.com/callback"}},
		{"two", model.StringArray{"https://a.example.com/cb", "https://b.example.com/cb"}},
		{"three", model.StringArray{"a", "b", "c"}},
		// An entry containing a space renders indistinguishably from two entries — the format is
		// not a parseable encoding, which is part of why measuring it matters.
		{"entry_with_space", model.StringArray{"a b"}},
		{"empty_entry", model.StringArray{"", ""}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		rendered := fmt.Sprintf("%s", c.in)
		out = append(out, map[string]any{
			"name":     c.name,
			"rendered": rendered,
			"length":   len(rendered),
			// The naive alternative, for contrast: the sum of the entries' lengths.
			"sum_of_entries": func() int {
				total := 0
				for _, s := range c.in {
					total += len(s)
				}
				return total
			}(),
		})
	}
	return out
}

// --- IsValid ---------------------------------------------------------------------------------

func oauthIsValidAll() []map[string]any {
	// A callback list whose *rendering* is exactly at and just over the 1024 cap. Built from the
	// rendered length rather than the sum, which is the whole point.
	atCap := model.StringArray{"https://e.com/" + oauthRunes(1024-2-len("https://e.com/"))}
	overCap := model.StringArray{"https://e.com/" + oauthRunes(1024-2-len("https://e.com/")+1)}

	corpus := []struct {
		name string
		in   model.OAuthApp
	}{
		{"valid", validOAuthApp()},
		{"bad_id", func() model.OAuthApp { a := validOAuthApp(); a.Id = "nope"; return a }()},
		{"zero_create_at", func() model.OAuthApp { a := validOAuthApp(); a.CreateAt = 0; return a }()},
		{"zero_update_at", func() model.OAuthApp { a := validOAuthApp(); a.UpdateAt = 0; return a }()},
		{"bad_creator_id", func() model.OAuthApp { a := validOAuthApp(); a.CreatorId = "nope"; return a }()},
		// ...unless dynamically registered, which exempts it.
		{"bad_creator_id_but_dynamic", func() model.OAuthApp {
			a := validOAuthApp()
			a.CreatorId = "nope"
			a.IsDynamicallyRegistered = true
			return a
		}()},
		{"client_secret_at_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.ClientSecret = oauthRunes(128)
			return a
		}()},
		{"client_secret_over_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.ClientSecret = oauthRunes(129)
			return a
		}()},
		{"empty_client_secret_is_allowed", func() model.OAuthApp {
			a := validOAuthApp()
			a.ClientSecret = ""
			return a
		}()},
		{"empty_name", func() model.OAuthApp { a := validOAuthApp(); a.Name = ""; return a }()},
		{"name_at_cap", func() model.OAuthApp { a := validOAuthApp(); a.Name = oauthRunes(64); return a }()},
		{"name_over_cap", func() model.OAuthApp { a := validOAuthApp(); a.Name = oauthRunes(65); return a }()},
		// Name is capped in BYTES: 33 two-byte runes is 66 bytes, so this fails despite being
		// well under 64 characters.
		{"name_multibyte_33_runes_66_bytes", func() model.OAuthApp {
			a := validOAuthApp()
			a.Name = oauthMultibyte(33)
			return a
		}()},
		{"name_multibyte_32_runes_64_bytes", func() model.OAuthApp {
			a := validOAuthApp()
			a.Name = oauthMultibyte(32)
			return a
		}()},
		{"no_callbacks", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = model.StringArray{}
			return a
		}()},
		{"nil_callbacks", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = nil
			return a
		}()},
		{"callbacks_rendering_at_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = atCap
			return a
		}()},
		{"callbacks_rendering_over_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = overCap
			return a
		}()},
		{"callback_not_a_url", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = model.StringArray{"not a url"}
			return a
		}()},
		{"second_callback_not_a_url", func() model.OAuthApp {
			a := validOAuthApp()
			a.CallbackUrls = model.StringArray{"https://ok.example.com", "nope"}
			return a
		}()},
		{"empty_homepage", func() model.OAuthApp { a := validOAuthApp(); a.Homepage = ""; return a }()},
		{"empty_homepage_but_dynamic", func() model.OAuthApp {
			a := validOAuthApp()
			a.Homepage = ""
			a.IsDynamicallyRegistered = true
			return a
		}()},
		{"homepage_not_a_url", func() model.OAuthApp {
			a := validOAuthApp()
			a.Homepage = "not a url"
			return a
		}()},
		{"homepage_over_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.Homepage = "https://e.com/" + oauthRunes(256)
			return a
		}()},
		// Description is capped in RUNES: 512 two-byte runes is 1024 bytes and still passes.
		{"description_512_multibyte_runes", func() model.OAuthApp {
			a := validOAuthApp()
			a.Description = oauthMultibyte(512)
			return a
		}()},
		{"description_513_runes", func() model.OAuthApp {
			a := validOAuthApp()
			a.Description = oauthRunes(513)
			return a
		}()},
		{"empty_icon_url_is_allowed", func() model.OAuthApp {
			a := validOAuthApp()
			a.IconURL = ""
			return a
		}()},
		{"icon_url_not_a_url", func() model.OAuthApp {
			a := validOAuthApp()
			a.IconURL = "not a url"
			return a
		}()},
		{"icon_url_over_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.IconURL = "https://e.com/" + oauthRunes(512)
			return a
		}()},
		{"mattermost_app_id_at_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.MattermostAppID = oauthRunes(32)
			return a
		}()},
		{"mattermost_app_id_over_cap", func() model.OAuthApp {
			a := validOAuthApp()
			a.MattermostAppID = oauthRunes(33)
			return a
		}()},
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
			// Several branches pass "app_id="+a.Id as the detail and two pass "" — the
			// difference is part of the response body.
			entry["detailed_error"] = err.DetailedError
		}
		out = append(out, entry)
	}
	return out
}

// --- PreSave / PreUpdate / Etag / Sanitize ---------------------------------------------------

func oauthPreSaveAll() []map[string]any {
	withID := validOAuthApp()
	withIDBefore := withID
	withID.PreSave()

	withoutID := validOAuthApp()
	withoutID.Id = ""
	withoutID.ClientSecret = ""
	withoutID.PreSave()

	updated := validOAuthApp()
	updatedBefore := updated
	updated.PreUpdate()

	return []map[string]any{
		{
			"name":                       "pre_save_with_id",
			"id_unchanged":               withID.Id == withIDBefore.Id,
			"create_at_equals_update_at": withID.CreateAt == withID.UpdateAt,
			"create_at_nonzero":          withID.CreateAt != 0,
			"client_secret_unchanged":    withID.ClientSecret == withIDBefore.ClientSecret,
		},
		{
			"name":         "pre_save_without_id",
			"id_generated": withoutID.Id != "",
			"id_length":    len(withoutID.Id),
			// The comment in Go is explicit: "PreSave no longer generates client secrets".
			"client_secret_still_empty": withoutID.ClientSecret == "",
		},
		{
			"name":                "pre_update",
			"create_at_unchanged": updated.CreateAt == updatedBefore.CreateAt,
			"update_at_nonzero":   updated.UpdateAt != 0,
			"id_unchanged":        updated.Id == updatedBefore.Id,
		},
	}
}

func oauthEtagAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.OAuthApp
	}{
		{"typical", validOAuthApp()},
		{"zero", model.OAuthApp{}},
	}
	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		out = append(out, map[string]any{"name": c.name, "etag": in.Etag()})
	}
	return out
}

func oauthSanitizeAll() []map[string]any {
	app := validOAuthApp()
	before := app
	app.Sanitize()
	blob, err := json.Marshal(&app)
	if err != nil {
		panic(err)
	}
	return []map[string]any{{
		"name":                 "sanitize",
		"client_secret_after":  app.ClientSecret,
		"client_secret_before": before.ClientSecret,
		"json_after":           string(blob),
		// Sanitize touches nothing else — notably the id and the callbacks survive.
		"id_unchanged":         app.Id == before.Id,
		"callbacks_unchanged":  len(app.CallbackUrls) == len(before.CallbackUrls),
		"is_trusted_unchanged": app.IsTrusted == before.IsTrusted,
	}}
}

func oauthRedirectURLAll() []map[string]any {
	app := validOAuthApp()
	app.CallbackUrls = model.StringArray{"https://a.example.com/cb", "https://b.example.com/cb"}

	corpus := []string{
		"https://a.example.com/cb",
		"https://b.example.com/cb",
		// Exact match only: no prefix, no case folding, no trailing-slash tolerance.
		"https://a.example.com/cb/",
		"https://A.example.com/cb",
		"https://a.example.com",
		"",
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, url := range corpus {
		out = append(out, map[string]any{"url": url, "valid": app.IsValidRedirectURL(url)})
	}
	return out
}

func oauthAuthMethodAll() []map[string]any {
	withSecret := validOAuthApp()
	withoutSecret := validOAuthApp()
	withoutSecret.ClientSecret = ""

	return []map[string]any{
		{
			"name":        "with_secret",
			"auth_method": withSecret.GetTokenEndpointAuthMethod(),
			"is_public":   withSecret.IsPublicClient(),
		},
		{
			"name":        "without_secret",
			"auth_method": withoutSecret.GetTokenEndpointAuthMethod(),
			"is_public":   withoutSecret.IsPublicClient(),
		},
	}
}

func oauthValidateGrantAll() []map[string]any {
	public := validOAuthApp()
	public.ClientSecret = ""
	confidential := validOAuthApp() // ClientSecret = "a-client-secret"

	corpus := []struct {
		name         string
		app          model.OAuthApp
		grantType    string
		clientSecret string
		codeVerifier string
	}{
		// Public client: no secret allowed, no refresh grant, PKCE required for auth code.
		{"public_auth_code_with_pkce", public, model.AccessTokenGrantType, "", "verifier"},
		{"public_auth_code_without_pkce", public, model.AccessTokenGrantType, "", ""},
		{"public_with_secret", public, model.AccessTokenGrantType, "some-secret", "verifier"},
		{"public_refresh_token", public, model.RefreshTokenGrantType, "", "verifier"},
		// A grant type that is neither: falls through every check and succeeds.
		{"public_unknown_grant", public, "something_else", "", ""},
		// Confidential client: the secret must match exactly.
		{"confidential_correct_secret", confidential, model.AccessTokenGrantType, "a-client-secret", ""},
		{"confidential_wrong_secret", confidential, model.AccessTokenGrantType, "wrong", ""},
		{"confidential_empty_secret", confidential, model.AccessTokenGrantType, "", ""},
		// ConstantTimeCompare returns 0 when the lengths differ, so a prefix fails too.
		{"confidential_prefix_secret", confidential, model.AccessTokenGrantType, "a-client-secre", ""},
		{"confidential_refresh_token_ok", confidential, model.RefreshTokenGrantType, "a-client-secret", ""},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		app := c.app
		err := app.ValidateForGrantType(c.grantType, c.clientSecret, c.codeVerifier)
		entry := map[string]any{"name": c.name}
		if err == nil {
			entry["ok"] = true
		} else {
			entry["ok"] = false
			entry["id"] = err.Id
			entry["where"] = err.Where
			entry["status"] = err.StatusCode
		}
		out = append(out, entry)
	}
	return out
}

func oauthAuditableAll() map[string]any {
	app := validOAuthApp()
	blob, err := json.Marshal(app.Auditable())
	if err != nil {
		panic(err)
	}
	public := validOAuthApp()
	public.ClientSecret = ""
	publicBlob, err := json.Marshal(public.Auditable())
	if err != nil {
		panic(err)
	}
	return map[string]any{
		// Note the "callback_urls:" key — the trailing colon is Go's typo.
		"confidential": string(blob),
		"public":       string(publicBlob),
	}
}

// --- the two DCR bridge functions ------------------------------------------------------------

func oauthFromRegistrationAll() []map[string]any {
	s := func(v string) *string { return &v }

	corpus := []struct {
		name string
		req  model.ClientRegistrationRequest
	}{
		// No ClientName: Go substitutes a literal default.
		{"minimal", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
		}},
		{"with_name", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientName:   s("My Client"),
		}},
		// A pointer to "" is not nil, so the name becomes empty rather than the default.
		{"empty_name_pointer", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientName:   s(""),
		}},
		// The auth method decides whether a secret is minted. Default (nil) is confidential.
		{"auth_method_nil_is_confidential", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
		}},
		{"auth_method_none_is_public", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: s(model.ClientAuthMethodNone),
		}},
		{"auth_method_secret_post", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: s(model.ClientAuthMethodClientSecretPost),
		}},
		// Anything that is not "none" mints a secret, including an unsupported method — IsValid
		// would have rejected it, but this function does not re-check.
		{"auth_method_unknown_still_mints", model.ClientRegistrationRequest{
			RedirectURIs:            []string{"https://example.com/cb"},
			TokenEndpointAuthMethod: s("client_secret_basic"),
		}},
		{"with_client_uri", model.ClientRegistrationRequest{
			RedirectURIs: []string{"https://example.com/cb"},
			ClientURI:    s("https://example.com"),
		}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		req := c.req
		app := model.NewOAuthAppFromClientRegistration(&req, "aaaaaaaaaaaaaaaaaaaaaaaaaa")
		out = append(out, map[string]any{
			"name":                      c.name,
			"creator_id":                app.CreatorId,
			"name_field":                app.Name,
			"homepage":                  app.Homepage,
			"is_dynamically_registered": app.IsDynamicallyRegistered,
			"callback_urls":             app.CallbackUrls,
			// The secret is generated, so record only whether one exists and how long it is.
			"has_client_secret":    app.ClientSecret != "",
			"client_secret_length": len(app.ClientSecret),
			// Everything the function does NOT set.
			"id":        app.Id,
			"create_at": app.CreateAt,
			"update_at": app.UpdateAt,
		})
	}
	return out
}

func oauthToRegistrationAll() []map[string]any {
	confidential := validOAuthApp()
	public := validOAuthApp()
	public.ClientSecret = ""
	noName := validOAuthApp()
	noName.Name = ""
	noHomepage := validOAuthApp()
	noHomepage.Homepage = ""

	corpus := []struct {
		name string
		app  model.OAuthApp
	}{
		{"confidential", confidential},
		{"public", public},
		{"no_name", noName},
		{"no_homepage", noHomepage},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		app := c.app
		// siteURL is a parameter the body never reads — passing two different values must give
		// the same answer, which is what the second call establishes.
		resp := app.ToClientRegistrationResponse("https://site.example.com")
		alt := app.ToClientRegistrationResponse("https://completely-different.example.org")

		blob, err := json.Marshal(resp)
		if err != nil {
			panic(err)
		}
		altBlob, err := json.Marshal(alt)
		if err != nil {
			panic(err)
		}

		out = append(out, map[string]any{
			"name":                c.name,
			"json":                string(blob),
			"site_url_is_ignored": string(blob) == string(altBlob),
			"has_client_secret":   resp.ClientSecret != nil,
			"has_client_name":     resp.ClientName != nil,
			"has_client_uri":      resp.ClientURI != nil,
		})
	}
	return out
}
