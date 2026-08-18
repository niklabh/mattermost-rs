package main

// Behavioural oracle for model.AuthData / model.AuthorizeRequest, written to
// fixtures/behaviour_authorize.json.
//
// This is the OAuth **authorization code** surface, including PKCE. It decides whether a code
// redemption is allowed to proceed, so a branch translated the wrong way is an auth bypass rather
// than a wire-format wobble. Nothing here is asserted from a reading.
//
// # `AuthorizeRequest.IsValid` reports the wrong `Where` on every branch
//
//	func (ar *AuthorizeRequest) IsValid() *AppError {
//		if !IsValidId(ar.ClientId) {
//			return NewAppError("AuthData.IsValid", ...)
//
// All five of its own branches name **`AuthData.IsValid`** — a copy-paste from the function above.
// Its PKCE and resource branches, which delegate, correctly say `AuthorizeRequest.…`, so the
// `Where` is inconsistent *within one function*. Recorded per branch so the port cannot tidy it.
//
// # `IsExpired` multiplies in int32 and can overflow
//
//	return GetMillis() > ad.CreateAt+int64(ad.ExpiresIn*1000)
//
// `ExpiresIn` is an `int32`, so `ExpiresIn*1000` is evaluated **in int32** and only then widened.
// Anything above 2,147,483 seconds wraps negative, and the code is then treated as long expired.
// Go does not panic on non-constant integer overflow, so this is silent. The corpus records the
// wrapped product directly, which is pure arithmetic and therefore deterministic.
//
// # `VerifyPKCE` returns TRUE when no PKCE was used
//
//	if ad.CodeChallenge == "" && ad.CodeChallengeMethod == "" { return true }
//
// Deliberate backward compatibility, and the single most dangerous line in the file to get wrong
// in either direction: inverted, every legacy flow breaks; dropped, a stored record with no
// challenge accepts any verifier. `ValidatePKCEForClientType` is what stops that mattering for a
// public client, and its branch matrix is driven separately for both client types.
//
// # The two regexes are probed through the public API, not extracted
//
// `codeChallengeRegex` and `codeVerifierRegex` are unexported. The challenge one is observable
// through `validatePKCEParameters`' error id. The verifier one is **not** observable through
// `VerifyPKCE`'s bool on its own — a rejected verifier and a non-matching one both give false — so
// each probe sets the stored challenge to the correct S256 of the verifier being probed. Then the
// answer is `true` exactly when the regex and the length check both passed.
//
// Determinism: fixed inputs, and `GetMillis`-dependent values are recorded as arithmetic rather
// than as answers. No rand, no clock.

import (
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeAuthorizeBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":                   authorizeConstants(),
		"auth_data_is_valid":          authDataIsValidAll(),
		"authorize_request_is_valid":  authorizeRequestIsValidAll(),
		"where_is_copy_pasted":        authorizeWhereProbe(),
		"pre_save":                    authDataPreSaveAll(),
		"is_expired":                  authDataIsExpiredAll(),
		"verify_pkce":                 authDataVerifyPKCEAll(),
		"validate_pkce_for_client":    authDataValidatePKCEForClientTypeAll(),
		"validate_resource_parameter": authorizeResourceAll(),
		"code_challenge_charset":      authorizeChallengeCharset(),
		"code_verifier_charset":       authorizeVerifierCharset(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_authorize.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

func authorizeConstants() map[string]any {
	return map[string]any{
		"AuthCodeExpireTime":          model.AuthCodeExpireTime,
		"AuthCodeResponseType":        model.AuthCodeResponseType,
		"ImplicitResponseType":        model.ImplicitResponseType,
		"DefaultScope":                model.DefaultScope,
		"PKCECodeChallengeMethodS256": model.PKCECodeChallengeMethodS256,
		"PKCECodeChallengeMinLength":  model.PKCECodeChallengeMinLength,
		"PKCECodeChallengeMaxLength":  model.PKCECodeChallengeMaxLength,
		"PKCECodeVerifierMinLength":   model.PKCECodeVerifierMinLength,
		"PKCECodeVerifierMaxLength":   model.PKCECodeVerifierMaxLength,
		// AuthCodeExpireTime is SECONDS, and IsExpired multiplies by 1000. Recorded so a port
		// cannot read "600" as milliseconds and shorten every authorization code to 0.6s.
		"auth_code_expire_time_unit": "seconds",
	}
}

// azID is a valid 26-character Mattermost id.
const azID = "abcdefghijklmnopqrstuvwxyz"
const azID2 = "zyxwvutsrqponmlkjihgfedcba"

// s256 is the challenge a given verifier must produce.
func s256(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

// A 43-character verifier — the minimum length — made only of unreserved characters.
const azVerifier = "abcdefghijklmnopqrstuvwxyz0123456789-._~ABC"

func azValidAuthData() model.AuthData {
	return model.AuthData{
		ClientId:    azID,
		UserId:      azID2,
		Code:        "the-authorization-code",
		ExpiresIn:   600,
		CreateAt:    1700000000000,
		RedirectUri: "https://example.com/callback",
		State:       "opaque-state",
		Scope:       "user",
	}
}

func azValidAuthorizeRequest() model.AuthorizeRequest {
	return model.AuthorizeRequest{
		ResponseType: model.AuthCodeResponseType,
		ClientId:     azID,
		RedirectURI:  "https://example.com/callback",
		Scope:        "user",
		State:        "opaque-state",
	}
}

func azErrEntry(name string, err *model.AppError) map[string]any {
	entry := map[string]any{"name": name, "ok": err == nil}
	if err != nil {
		entry["id"] = err.Id
		entry["where"] = err.Where
		entry["status"] = err.StatusCode
		entry["detailed_error"] = err.DetailedError
	}
	return entry
}

func authDataIsValidAll() []map[string]any {
	// A challenge of exactly the minimum length, and one of the maximum.
	minChallenge := strings.Repeat("a", model.PKCECodeChallengeMinLength)
	maxChallenge := strings.Repeat("a", model.PKCECodeChallengeMaxLength)

	corpus := []struct {
		name string
		mut  func(*model.AuthData)
	}{
		{"valid", func(*model.AuthData) {}},
		{"bad_client_id", func(a *model.AuthData) { a.ClientId = "nope" }},
		{"empty_client_id", func(a *model.AuthData) { a.ClientId = "" }},
		{"bad_user_id", func(a *model.AuthData) { a.UserId = "nope" }},
		{"empty_code", func(a *model.AuthData) { a.Code = "" }},
		{"code_at_cap", func(a *model.AuthData) { a.Code = strings.Repeat("c", 128) }},
		{"code_over_cap", func(a *model.AuthData) { a.Code = strings.Repeat("c", 129) }},
		// The cap is len(), i.e. BYTES — 64 two-byte runes is 128 bytes and passes; 65 does not.
		{"code_multibyte_at_cap", func(a *model.AuthData) { a.Code = strings.Repeat("é", 64) }},
		{"code_multibyte_over_cap", func(a *model.AuthData) { a.Code = strings.Repeat("é", 65) }},
		{"zero_expires_in", func(a *model.AuthData) { a.ExpiresIn = 0 }},
		// IsValid only rejects ZERO, so a negative expiry validates.
		{"negative_expires_in", func(a *model.AuthData) { a.ExpiresIn = -1 }},
		{"zero_create_at", func(a *model.AuthData) { a.CreateAt = 0 }},
		{"negative_create_at", func(a *model.AuthData) { a.CreateAt = -1 }},
		{"empty_redirect_uri", func(a *model.AuthData) { a.RedirectUri = "" }},
		{"non_http_redirect_uri", func(a *model.AuthData) { a.RedirectUri = "ftp://example.com/x" }},
		{"relative_redirect_uri", func(a *model.AuthData) { a.RedirectUri = "/callback" }},
		{"redirect_uri_at_cap", func(a *model.AuthData) {
			a.RedirectUri = "https://example.com/" + strings.Repeat("a", 256-20)
		}},
		{"redirect_uri_over_cap", func(a *model.AuthData) {
			a.RedirectUri = "https://example.com/" + strings.Repeat("a", 257-20)
		}},
		{"state_at_cap", func(a *model.AuthData) { a.State = strings.Repeat("s", 1024) }},
		{"state_over_cap", func(a *model.AuthData) { a.State = strings.Repeat("s", 1025) }},
		{"scope_at_cap", func(a *model.AuthData) { a.Scope = strings.Repeat("s", 128) }},
		{"scope_over_cap", func(a *model.AuthData) { a.Scope = strings.Repeat("s", 129) }},

		// --- PKCE: the block runs only when at least one field is non-empty ------------------
		{"pkce_both_empty_is_skipped", func(*model.AuthData) {}},
		{"pkce_challenge_only", func(a *model.AuthData) { a.CodeChallenge = minChallenge }},
		{"pkce_method_only", func(a *model.AuthData) { a.CodeChallengeMethod = "S256" }},
		{"pkce_valid", func(a *model.AuthData) {
			a.CodeChallenge = minChallenge
			a.CodeChallengeMethod = "S256"
		}},
		{"pkce_plain_method", func(a *model.AuthData) {
			a.CodeChallenge = minChallenge
			a.CodeChallengeMethod = "plain"
		}},
		{"pkce_lowercase_method", func(a *model.AuthData) {
			a.CodeChallenge = minChallenge
			a.CodeChallengeMethod = "s256"
		}},
		{"pkce_challenge_under_min", func(a *model.AuthData) {
			a.CodeChallenge = strings.Repeat("a", model.PKCECodeChallengeMinLength-1)
			a.CodeChallengeMethod = "S256"
		}},
		{"pkce_challenge_at_max", func(a *model.AuthData) {
			a.CodeChallenge = maxChallenge
			a.CodeChallengeMethod = "S256"
		}},
		{"pkce_challenge_over_max", func(a *model.AuthData) {
			a.CodeChallenge = strings.Repeat("a", model.PKCECodeChallengeMaxLength+1)
			a.CodeChallengeMethod = "S256"
		}},
		{"pkce_challenge_bad_charset", func(a *model.AuthData) {
			a.CodeChallenge = strings.Repeat("a", 42) + "+"
			a.CodeChallengeMethod = "S256"
		}},
		{"pkce_challenge_with_padding", func(a *model.AuthData) {
			a.CodeChallenge = strings.Repeat("a", 42) + "="
			a.CodeChallengeMethod = "S256"
		}},

		// --- resource, RFC 8707 -----------------------------------------------------------------
		{"resource_valid", func(a *model.AuthData) { a.Resource = "https://api.example.com/v1" }},
		{"resource_relative", func(a *model.AuthData) { a.Resource = "/v1/resource" }},
		{"resource_with_fragment", func(a *model.AuthData) { a.Resource = "https://api.example.com/#frag" }},
		{"resource_at_cap", func(a *model.AuthData) {
			a.Resource = "https://api.example.com/" + strings.Repeat("r", 512-24)
		}},
		{"resource_over_cap", func(a *model.AuthData) {
			a.Resource = "https://api.example.com/" + strings.Repeat("r", 513-24)
		}},

		// --- ordering: which branch wins when two are broken -------------------------------------
		{"bad_client_id_and_bad_user_id", func(a *model.AuthData) {
			a.ClientId = "nope"
			a.UserId = "nope"
		}},
		{"bad_code_and_bad_pkce", func(a *model.AuthData) {
			a.Code = ""
			a.CodeChallenge = "+"
			a.CodeChallengeMethod = "plain"
		}},
		{"bad_pkce_and_bad_resource", func(a *model.AuthData) {
			a.CodeChallengeMethod = "plain"
			a.CodeChallenge = strings.Repeat("a", 43)
			a.Resource = "/relative"
		}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		ad := azValidAuthData()
		c.mut(&ad)
		out = append(out, azErrEntry(c.name, ad.IsValid()))
	}
	return out
}

func authorizeRequestIsValidAll() []map[string]any {
	minChallenge := strings.Repeat("a", model.PKCECodeChallengeMinLength)

	corpus := []struct {
		name string
		mut  func(*model.AuthorizeRequest)
	}{
		{"valid", func(*model.AuthorizeRequest) {}},
		{"implicit_response_type", func(r *model.AuthorizeRequest) { r.ResponseType = model.ImplicitResponseType }},
		// Any non-empty response type passes; it is not checked against the two constants.
		{"nonsense_response_type", func(r *model.AuthorizeRequest) { r.ResponseType = "banana" }},
		{"empty_response_type", func(r *model.AuthorizeRequest) { r.ResponseType = "" }},
		{"bad_client_id", func(r *model.AuthorizeRequest) { r.ClientId = "nope" }},
		{"empty_redirect_uri", func(r *model.AuthorizeRequest) { r.RedirectURI = "" }},
		{"relative_redirect_uri", func(r *model.AuthorizeRequest) { r.RedirectURI = "/callback" }},
		{"redirect_uri_over_cap", func(r *model.AuthorizeRequest) {
			r.RedirectURI = "https://example.com/" + strings.Repeat("a", 257-20)
		}},
		{"state_over_cap", func(r *model.AuthorizeRequest) { r.State = strings.Repeat("s", 1025) }},
		{"scope_over_cap", func(r *model.AuthorizeRequest) { r.Scope = strings.Repeat("s", 129) }},
		{"pkce_challenge_only", func(r *model.AuthorizeRequest) { r.CodeChallenge = minChallenge }},
		{"pkce_method_only", func(r *model.AuthorizeRequest) { r.CodeChallengeMethod = "S256" }},
		{"pkce_valid", func(r *model.AuthorizeRequest) {
			r.CodeChallenge = minChallenge
			r.CodeChallengeMethod = "S256"
		}},
		{"pkce_plain_method", func(r *model.AuthorizeRequest) {
			r.CodeChallenge = minChallenge
			r.CodeChallengeMethod = "plain"
		}},
		{"resource_relative", func(r *model.AuthorizeRequest) { r.Resource = "/v1/resource" }},
		{"resource_with_fragment", func(r *model.AuthorizeRequest) { r.Resource = "https://api.example.com/#f" }},
		// ClientId is checked BEFORE ResponseType, unlike the field order in the struct.
		{"bad_client_id_and_empty_response_type", func(r *model.AuthorizeRequest) {
			r.ClientId = "nope"
			r.ResponseType = ""
		}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		ar := azValidAuthorizeRequest()
		c.mut(&ar)
		out = append(out, azErrEntry(c.name, ar.IsValid()))
	}
	return out
}

// authorizeWhereProbe states the copy-paste as data.
//
// Every branch `AuthorizeRequest.IsValid` owns reports `AuthData.IsValid`; the two it delegates
// report their own names. Recorded as a comparison rather than as a list, so the assertion reads
// as the claim it is.
func authorizeWhereProbe() map[string]any {
	ar := azValidAuthorizeRequest()
	ar.ClientId = "nope"
	own := ar.IsValid()

	pkce := azValidAuthorizeRequest()
	pkce.CodeChallengeMethod = "plain"
	pkce.CodeChallenge = strings.Repeat("a", 43)
	delegatedPKCE := pkce.IsValid()

	res := azValidAuthorizeRequest()
	res.Resource = "/relative"
	delegatedResource := res.IsValid()

	ad := azValidAuthData()
	ad.ClientId = "nope"
	adOwn := ad.IsValid()

	return map[string]any{
		"authorize_request_own_branch":      own.Where,
		"authorize_request_pkce_branch":     delegatedPKCE.Where,
		"authorize_request_resource_branch": delegatedResource.Where,
		"auth_data_own_branch":              adOwn.Where,
		"own_branch_names_the_wrong_type":   own.Where == "AuthData.IsValid",
		"delegated_branches_name_the_right_one": delegatedPKCE.Where == "AuthorizeRequest.validatePKCE" &&
			delegatedResource.Where == "AuthorizeRequest.IsValid",
	}
}

func authDataPreSaveAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.AuthData
	}{
		{"all_zero", model.AuthData{}},
		{"expires_in_set", model.AuthData{ExpiresIn: 42}},
		{"create_at_set", model.AuthData{CreateAt: 1700000000000}},
		{"scope_set", model.AuthData{Scope: "custom"}},
		{"everything_set", model.AuthData{ExpiresIn: 42, CreateAt: 1700000000000, Scope: "custom"}},
		// A negative expiry is NOT defaulted — the guard is `== 0`.
		{"negative_expires_in", model.AuthData{ExpiresIn: -5, CreateAt: 1, Scope: "x"}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		ad := c.in
		ad.PreSave()
		entry := map[string]any{
			"name":           c.name,
			"in_expires_in":  c.in.ExpiresIn,
			"in_create_at":   c.in.CreateAt,
			"in_scope":       c.in.Scope,
			"out_expires_in": ad.ExpiresIn,
			"out_scope":      ad.Scope,
			// CreateAt derives from GetMillis when it was zero, so record the *fact* rather than
			// the value — the same treatment file_info's PreSave gets after [D-032].
			"create_at_uses_now": c.in.CreateAt == 0,
		}
		if c.in.CreateAt != 0 {
			entry["out_create_at"] = ad.CreateAt
		}
		out = append(out, entry)
	}
	return out
}

// authDataIsExpiredAll records the expiry arithmetic, not the answer.
//
// `IsExpired` reads the clock, so its bool is not fixture material. What IS deterministic is the
// threshold it compares against — and that is where the bug lives:
//
//	ad.CreateAt + int64(ad.ExpiresIn*1000)
//
// `ExpiresIn` is int32, so the multiply happens in int32 and wraps. The corpus records the wrapped
// product beside the widened one so the difference is visible rather than inferred.
func authDataIsExpiredAll() []map[string]any {
	var out []map[string]any
	for _, expiresIn := range []int32{
		0, 1, 600, model.AuthCodeExpireTime, -1,
		2147483, // 2147483 * 1000 = 2147483000, still inside int32
		2147484, // 2147484 * 1000 overflows
		2200000,
		2147483647, // int32 max
		-2147483648,
	} {
		createAt := int64(1700000000000)
		wrapped := int64(expiresIn * 1000) // Go's expression, int32 multiply then widen
		widened := int64(expiresIn) * 1000 // what a reader assumes it says
		out = append(out, map[string]any{
			"expires_in":       expiresIn,
			"create_at":        createAt,
			"wrapped_product":  wrapped,
			"widened_product":  widened,
			"overflows":        wrapped != widened,
			"expiry_threshold": createAt + wrapped,
			// A threshold in the past means the code is already expired whatever the clock says.
			"threshold_is_before_create_at": createAt+wrapped < createAt,
		})
	}
	return out
}

func authDataVerifyPKCEAll() []map[string]any {
	validChallenge := s256(azVerifier)

	corpus := []struct {
		name      string
		challenge string
		method    string
		verifier  string
	}{
		// The backward-compatibility door: no PKCE stored means ANY verifier is accepted.
		{"no_pkce_empty_verifier", "", "", ""},
		{"no_pkce_any_verifier", "", "", "anything at all"},
		// Exactly one empty is an invalid stored state and always fails.
		{"challenge_without_method", validChallenge, "", azVerifier},
		{"method_without_challenge", "", "S256", azVerifier},

		{"correct_verifier", validChallenge, "S256", azVerifier},
		{"wrong_verifier", validChallenge, "S256", "b" + azVerifier[1:]},
		{"verifier_under_min", validChallenge, "S256", strings.Repeat("a", 42)},
		{"verifier_at_min", s256(strings.Repeat("a", 43)), "S256", strings.Repeat("a", 43)},
		{"verifier_at_max", s256(strings.Repeat("a", 128)), "S256", strings.Repeat("a", 128)},
		{"verifier_over_max", s256(strings.Repeat("a", 129)), "S256", strings.Repeat("a", 129)},
		{"verifier_bad_charset", s256(strings.Repeat("a", 42) + "+"), "S256", strings.Repeat("a", 42) + "+"},
		{"verifier_with_slash", s256(strings.Repeat("a", 42) + "/"), "S256", strings.Repeat("a", 42) + "/"},
		{"plain_method_rejected", azVerifier, "plain", azVerifier},
		{"lowercase_method_rejected", validChallenge, "s256", azVerifier},
		// The challenge is base64URL with no padding; a standard-alphabet challenge cannot match.
		{"standard_base64_challenge", standardB64(azVerifier), "S256", azVerifier},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		ad := azValidAuthData()
		ad.CodeChallenge = c.challenge
		ad.CodeChallengeMethod = c.method
		out = append(out, map[string]any{
			"name":           c.name,
			"code_challenge": c.challenge,
			"method":         c.method,
			"code_verifier":  c.verifier,
			"verifier_bytes": len(c.verifier),
			"verified":       ad.VerifyPKCE(c.verifier),
		})
	}
	return out
}

// standardB64 encodes with the STANDARD alphabet, which differs from base64url on '+' and '/'.
func standardB64(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawStdEncoding.EncodeToString(sum[:])
}

func authDataValidatePKCEForClientTypeAll() []map[string]any {
	validChallenge := s256(azVerifier)

	corpus := []struct {
		name      string
		challenge string
		method    string
		verifier  string
	}{
		{"no_pkce_no_verifier", "", "", ""},
		{"no_pkce_with_verifier", "", "", azVerifier},
		{"pkce_correct_verifier", validChallenge, "S256", azVerifier},
		{"pkce_wrong_verifier", validChallenge, "S256", "b" + azVerifier[1:]},
		{"pkce_no_verifier", validChallenge, "S256", ""},
		{"challenge_without_method_correct_verifier", validChallenge, "", azVerifier},
		{"method_without_challenge", "", "S256", azVerifier},
		{"method_without_challenge_no_verifier", "", "S256", ""},
	}

	var out []map[string]any
	for _, c := range corpus {
		for _, isPublic := range []bool{true, false} {
			ad := azValidAuthData()
			ad.CodeChallenge = c.challenge
			ad.CodeChallengeMethod = c.method
			err := ad.ValidatePKCEForClientType(isPublic, c.verifier)

			entry := azErrEntry(c.name, err)
			entry["is_public_client"] = isPublic
			entry["code_challenge"] = c.challenge
			entry["method"] = c.method
			entry["code_verifier"] = c.verifier
			out = append(out, entry)
		}
	}
	return out
}

func authorizeResourceAll() []map[string]any {
	inputs := []struct{ name, resource string }{
		{"empty", ""},
		{"absolute_https", "https://api.example.com/v1"},
		{"absolute_http", "http://api.example.com/v1"},
		// Any scheme counts as absolute — RFC 8707 resource indicators are URIs, not URLs.
		{"custom_scheme", "urn:example:resource"},
		{"mailto", "mailto:someone@example.com"},
		{"relative", "/v1/resource"},
		{"scheme_relative", "//api.example.com/v1"},
		{"bare_host", "api.example.com"},
		{"with_fragment", "https://api.example.com/v1#section"},
		{"with_empty_fragment", "https://api.example.com/v1#"},
		{"with_query", "https://api.example.com/v1?a=b"},
		{"at_cap", "https://api.example.com/" + strings.Repeat("r", 512-24)},
		{"over_cap", "https://api.example.com/" + strings.Repeat("r", 513-24)},
		// url.Parse is lenient; these are the shapes that actually make it error.
		{"control_character", "https://api.example.com/\x7f"},
		{"bad_percent_escape", "https://api.example.com/%zz"},
		{"invalid_port", "https://api.example.com:port/v1"},
		{"space", "https://api.example.com/a b"},
		{"multibyte", "https://api.example.com/é"},
	}

	var out []map[string]any
	for _, in := range inputs {
		err := model.ValidateResourceParameter(in.resource, azID, "probe")
		entry := azErrEntry(in.name, err)
		entry["resource"] = in.resource
		entry["resource_bytes"] = len(in.resource)
		out = append(out, entry)
	}
	return out
}

// authorizeChallengeCharset probes codeChallengeRegex through validatePKCEParameters' error id.
//
// The regex is unexported, so it is measured where it is applied. A 43-character challenge with
// the probe codepoint at position 20 is the minimum length, so the length branch cannot fire and
// the only remaining verdict is the format one.
func authorizeChallengeCharset() []map[string]any {
	var out []map[string]any
	for _, r := range authorizeProbePoints() {
		challenge := strings.Repeat("a", 20) + string(r) + strings.Repeat("a", 22)
		ad := azValidAuthData()
		ad.CodeChallenge = challenge
		ad.CodeChallengeMethod = "S256"
		err := ad.IsValid()

		entry := map[string]any{
			"codepoint":       int(r),
			"challenge_bytes": len(challenge),
			"ok":              err == nil,
		}
		if err != nil {
			entry["id"] = err.Id
		}
		out = append(out, entry)
	}
	return out
}

// authorizeVerifierCharset probes codeVerifierRegex through VerifyPKCE.
//
// The bool alone cannot separate "rejected by the regex" from "did not match the challenge", so
// each probe stores the CORRECT S256 of the verifier it is testing. `verified` is then true
// exactly when the length and format checks both passed.
func authorizeVerifierCharset() []map[string]any {
	var out []map[string]any
	for _, r := range authorizeProbePoints() {
		verifier := strings.Repeat("a", 20) + string(r) + strings.Repeat("a", 22)
		ad := azValidAuthData()
		ad.CodeChallenge = s256(verifier)
		ad.CodeChallengeMethod = "S256"

		out = append(out, map[string]any{
			"codepoint":      int(r),
			"verifier_bytes": len(verifier),
			"verified":       ad.VerifyPKCE(verifier),
		})
	}
	return out
}

// The codepoints both charset sweeps use: all of ASCII, plus the non-ASCII shapes that trip a
// regex ported without care — and a newline, because Go's `$` is end-of-TEXT by default while
// several other engines let it match before a trailing newline.
func authorizeProbePoints() []rune {
	var points []rune
	for r := rune(0); r < 128; r++ {
		points = append(points, r)
	}
	return append(points, '\u00e9', '\u0130', '\u4e2d', '\u00a0', '\ufeff', '\u2028')
}
