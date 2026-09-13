package main

// Behavioural oracle for the licence logic: the tier ladder (`model.LicenseToLicenseTier` and the
// three `MinimumXLicense` gates), the client map (`utils.GetClientLicense` and its sanitized
// form), the pre-signature half of `utils.LicenseValidator.ValidateLicense`, and the pure
// predicates on `model.License` that route handlers branch on.
//
// Why these and not the signature: verification needs a key pair, and the reference's private
// keys are not in the tree. The parity harness covers the accepting branch with a stack-local key
// (`scripts/go-licensed.sh`); what this pins is everything either side of it — the decoding, the
// null strip, the length check, the message on a forged signature, and what the loaded licence
// then answers.
//
// The client-map cases run `Features.SetDefaults()` first, as `PlatformService.SetLicense` does
// before it builds the map — so a sparse `features` object in the licence body still produces
// every key, and the values a Rust port gets by defaulting are compared against Go's.

import (
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/utils"
)

func writeLicenseBehaviourFixture(outDir string) error {
	out := map[string]any{
		"tiers":          licenseTierAll(),
		"minimum_nil":    licenseMinimum(nil),
		"client_license": licenseClientMapAll(),
		"validate":       licenseValidateAll(),
		"predicates":     licensePredicateAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_license.json"), append(blob, '\n'), 0o644)
}

type licenseTierCase struct {
	Sku          string `json:"sku"`
	Tier         int    `json:"tier"`
	Professional bool   `json:"professional"`
	Enterprise   bool   `json:"enterprise"`
	Advanced     bool   `json:"advanced"`
}

type licenseMinimumCase struct {
	Professional bool `json:"professional"`
	Enterprise   bool `json:"enterprise"`
	Advanced     bool `json:"advanced"`
}

func licenseMinimum(l *model.License) licenseMinimumCase {
	return licenseMinimumCase{
		Professional: model.MinimumProfessionalLicense(l),
		Enterprise:   model.MinimumEnterpriseLicense(l),
		Advanced:     model.MinimumEnterpriseAdvancedLicense(l),
	}
}

// Every SKU the map names, the two legacy SKUs it does not, the empty string, and the case and
// whitespace variants a careless comparison would accept.
func licenseTierAll() []licenseTierCase {
	skus := []string{
		"", "E10", "E20", "professional", "enterprise", "advanced", "entry",
		"Professional", "ENTERPRISE", " enterprise", "enterprise ", "starter", "free", "cloud",
	}
	cases := make([]licenseTierCase, 0, len(skus))
	for _, sku := range skus {
		l := &model.License{SkuShortName: sku}
		m := licenseMinimum(l)
		cases = append(cases, licenseTierCase{
			Sku:          sku,
			Tier:         model.LicenseToLicenseTier[sku],
			Professional: m.Professional,
			Enterprise:   m.Enterprise,
			Advanced:     m.Advanced,
		})
	}
	return cases
}

// The licence `scripts/go-licensed.sh` signs for the oracle, verbatim, so the map the oracle
// serves over HTTP and the map this corpus records are built from one document.
const oracleLicenseJSON = `{"id":"mmrslicensedoracle00000001","issued_at":1767225600000,"starts_at":1767225600000,"expires_at":4102444800000,"customer":{"id":"mmrslicensecustomer0000001","name":"mattermost-rs parity oracle","email":"oracle@mmrs.invalid","company":"mattermost-rs"},"features":{"users":100000,"ldap":true,"ldap_groups":true,"mfa":true,"google_oauth":true,"office365_oauth":true,"openid":true,"compliance":true,"cluster":true,"metrics":true,"mhpns":true,"saml":true,"elastic_search":true,"announcement":true,"theme_management":true,"email_notification_contents":true,"data_retention":true,"message_export":true,"custom_permissions_schemes":true,"custom_terms_of_service":true,"guest_accounts":true,"guest_accounts_permissions":true,"id_loaded":true,"lock_teammate_name_display":true,"enterprise_plugins":true,"advanced_logging":true,"cloud":false,"shared_channels":true,"remote_cluster_service":true,"outgoing_oauth_connections":true,"auto_translation":true,"future_features":true},"sku_name":"Enterprise","sku_short_name":"enterprise","is_trial":false,"is_gov_sku":false,"is_non_production":false,"is_seat_count_enforced":false}`

// A licence that names almost nothing, so every value in its map is one `SetDefaults` chose —
// and `future_features:false` so those choices are visibly not "everything true".
const sparseLicenseJSON = `{"id":"mmrssparselicence000000001","issued_at":5,"starts_at":6,"expires_at":7,"customer":{"id":"c","name":"Sparse Name","email":"sparse@mmrs.invalid","company":"Sparse Co"},"features":{"users":3,"future_features":false,"saml":true},"sku_name":"Professional","sku_short_name":"professional","is_trial":true,"is_gov_sku":true,"is_non_production":true}`

type licenseClientMapCase struct {
	Name      string            `json:"name"`
	License   json.RawMessage   `json:"license"`
	Full      map[string]string `json:"full"`
	Sanitized map[string]string `json:"sanitized"`
}

func licenseClientMapAll() []licenseClientMapCase {
	cases := []licenseClientMapCase{{
		Name:      "nil",
		License:   json.RawMessage("null"),
		Full:      utils.GetClientLicense(nil),
		Sanitized: utils.GetSanitizedClientLicense(utils.GetClientLicense(nil)),
	}}
	for _, c := range []struct{ name, body string }{{"oracle", oracleLicenseJSON}, {"sparse", sparseLicenseJSON}} {
		var l model.License
		if err := json.Unmarshal([]byte(c.body), &l); err != nil {
			panic(err)
		}
		l.Features.SetDefaults()
		full := utils.GetClientLicense(&l)
		cases = append(cases, licenseClientMapCase{
			Name:      c.name,
			License:   json.RawMessage(c.body),
			Full:      full,
			Sanitized: utils.GetSanitizedClientLicense(full),
		})
	}
	return cases
}

type licenseValidateCase struct {
	Name  string `json:"name"`
	Input string `json:"input"`
	// The error's text, or "" on success. Nothing here succeeds — there is no key to sign with —
	// so every case is one of the refusals, and which one is the point.
	Error string `json:"error"`
	// The refusal's family, so a decoder whose inner message differs from Go's can still be
	// held to the same branch: "decode", "too_short", "invalid_signature".
	Kind string `json:"kind"`
}

func licenseValidateAll() []licenseValidateCase {
	b64 := base64.StdEncoding.EncodeToString
	raw := func(n int, b byte) []byte { return []byte(strings.Repeat(string([]byte{b}), n)) }
	inputs := []struct{ name, input string }{
		{"empty", ""},
		{"not_base64", "!!!!"},
		{"unpadded", "QUJ"},
		{"five_bytes", b64([]byte("short"))},
		{"exactly_256", b64(raw(256, 'A'))},
		{"257_bytes", b64(raw(257, 'A'))},
		// 300 'A' then 50 NULs: the strip removes the NULs and 300 > 256 remains.
		{"trailing_nuls_stripped_still_long", b64(append(raw(300, 'A'), raw(50, 0)...))},
		// 200 'A' then 100 NULs: after the strip only 200 remain, so it is short.
		{"trailing_nuls_make_it_short", b64(append(raw(200, 'A'), raw(100, 0)...))},
		// 257 bytes whose last is NUL: the strip eats one byte of "signature".
		{"last_byte_nul", b64(append(raw(256, 'A'), 0))},
		// Go's decoder skips newlines, so this is "ABCDEF", not an error.
		{"embedded_newline", "QUJD\nREVG"},
		{"crlf_wrapped_long", b64(raw(120, 'B'))[:80] + "\r\n" + b64(raw(120, 'B'))[80:]},
	}
	cases := make([]licenseValidateCase, 0, len(inputs))
	for _, in := range inputs {
		_, err := utils.LicenseValidator.ValidateLicense([]byte(in.input))
		c := licenseValidateCase{Name: in.name, Input: in.input}
		if err != nil {
			c.Error = err.Error()
			switch {
			case strings.HasPrefix(c.Error, "encountered error decoding license"):
				c.Kind = "decode"
			case c.Error == "Signed license not long enough":
				c.Kind = "too_short"
			case strings.HasPrefix(c.Error, "Invalid signature"):
				c.Kind = "invalid_signature"
			default:
				c.Kind = "other"
			}
		}
		cases = append(cases, c)
	}
	return cases
}

type licensePredicateCase struct {
	Name                            string             `json:"name"`
	License                         json.RawMessage    `json:"license"`
	IsTrialLicense                  bool               `json:"is_trial_license"`
	IsSanctionedTrial               bool               `json:"is_sanctioned_trial"`
	IsCloud                         bool               `json:"is_cloud"`
	IsCloudPreview                  bool               `json:"is_cloud_preview"`
	HasSharedChannels               bool               `json:"has_shared_channels"`
	HasRemoteClusterService         bool               `json:"has_remote_cluster_service"`
	HasEnterpriseMarketplacePlugins bool               `json:"has_enterprise_marketplace_plugins"`
	IsMattermostEntry               bool               `json:"is_mattermost_entry"`
	Minimum                         licenseMinimumCase `json:"minimum"`
}

// The predicates a handler branches on, over licences built to sit on each side of every
// comparison they make: the two exact trial durations and their off-by-ones, the sanctioned
// bounds, the cloud-preview hour, and the feature-or-tier disjunctions with the feature off.
func licensePredicateAll() []licensePredicateCase {
	const (
		hour       = int64(60 * 60 * 1000)
		day        = 24 * hour
		trial      = 30*day + 8*hour                         // trialDuration
		adminTrial = 30*day + 23*hour + 59*60*1000 + 59*1000 // adminTrialDuration
		lower      = 31*day + 23*hour + 59*60*1000 + 59*1000 // sanctionedTrialDurationLowerBound
		upper      = 29*day + 23*hour + 59*60*1000 + 59*1000 // sanctionedTrialDurationUpperBound
	)
	f := func(v bool) *bool { return &v }
	mk := func(name string, l model.License) licensePredicateCase {
		if l.Features == nil {
			l.Features = &model.Features{}
		}
		l.Features.SetDefaults()
		body, err := json.Marshal(l)
		if err != nil {
			panic(err)
		}
		return licensePredicateCase{
			Name:                            name,
			License:                         body,
			IsTrialLicense:                  l.IsTrialLicense(),
			IsSanctionedTrial:               l.IsSanctionedTrial(),
			IsCloud:                         l.IsCloud(),
			IsCloudPreview:                  l.IsCloudPreview(),
			HasSharedChannels:               l.HasSharedChannels(),
			HasRemoteClusterService:         l.HasRemoteClusterService(),
			HasEnterpriseMarketplacePlugins: l.HasEnterpriseMarketplacePlugins(),
			IsMattermostEntry:               l.IsMattermostEntry(),
			Minimum:                         licenseMinimum(&l),
		}
	}
	off := func(sku string) *model.Features {
		return &model.Features{
			SharedChannels: f(false), RemoteClusterService: f(false), EnterprisePlugins: f(false), Cloud: f(false),
		}
	}
	return []licensePredicateCase{
		mk("enterprise_all_on", model.License{SkuShortName: "enterprise", StartsAt: 1, ExpiresAt: 2}),
		mk("entry", model.License{SkuShortName: "entry", StartsAt: 1, ExpiresAt: 2, Features: off("entry")}),
		mk("e20_features_off", model.License{SkuShortName: "E20", StartsAt: 1, ExpiresAt: 2, Features: off("E20")}),
		mk("e10_features_off", model.License{SkuShortName: "E10", StartsAt: 1, ExpiresAt: 2, Features: off("E10")}),
		mk("professional_features_off", model.License{SkuShortName: "professional", StartsAt: 1, ExpiresAt: 2, Features: off("professional")}),
		mk("no_sku_shared_channels_only", model.License{StartsAt: 1, ExpiresAt: 2, Features: &model.Features{SharedChannels: f(true), RemoteClusterService: f(false), EnterprisePlugins: f(false), Cloud: f(false)}}),
		mk("no_sku_remote_cluster_only", model.License{StartsAt: 1, ExpiresAt: 2, Features: &model.Features{SharedChannels: f(false), RemoteClusterService: f(true), EnterprisePlugins: f(false), Cloud: f(false)}}),
		mk("is_trial_flag", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 1000, ExpiresAt: 2000}),
		mk("trial_duration_exact", model.License{SkuShortName: "professional", StartsAt: 1000, ExpiresAt: 1000 + trial}),
		mk("trial_duration_plus_one", model.License{SkuShortName: "professional", StartsAt: 1000, ExpiresAt: 1000 + trial + 1}),
		mk("admin_trial_duration_exact", model.License{SkuShortName: "professional", StartsAt: 1000, ExpiresAt: 1000 + adminTrial}),
		mk("admin_trial_duration_minus_one", model.License{SkuShortName: "professional", StartsAt: 1000, ExpiresAt: 1000 + adminTrial - 1}),
		mk("sanctioned_at_lower_bound", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 0, ExpiresAt: lower}),
		mk("sanctioned_below_lower_bound", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 0, ExpiresAt: lower - 1}),
		mk("sanctioned_at_upper_bound", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 0, ExpiresAt: upper}),
		mk("sanctioned_above_upper_bound", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 0, ExpiresAt: upper + 1}),
		mk("sanctioned_shape_but_not_trial", model.License{SkuShortName: "professional", StartsAt: 0, ExpiresAt: lower}),
		mk("cloud", model.License{SkuShortName: "professional", StartsAt: 1000, ExpiresAt: 2000, Features: &model.Features{Cloud: f(true)}}),
		mk("cloud_preview_hour", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 1000, ExpiresAt: 1000 + hour, Features: &model.Features{Cloud: f(true)}}),
		mk("cloud_hour_not_trial", model.License{SkuShortName: "professional", StartsAt: 1000, ExpiresAt: 1000 + hour, Features: &model.Features{Cloud: f(true)}}),
		mk("cloud_preview_wrong_length", model.License{SkuShortName: "professional", IsTrial: true, StartsAt: 1000, ExpiresAt: 1000 + hour + 1, Features: &model.Features{Cloud: f(true)}}),
	}
}
