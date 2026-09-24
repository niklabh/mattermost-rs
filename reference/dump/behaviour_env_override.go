package main

// Behavioural oracle for the environment overlay — `config.GetEnvironment` and `applyEnvKey`
// (config/environment.go:16 and :28) — written to fixtures/behaviour_env_override.json and
// asserted by `mm_app::config`'s `env_go_parity` module.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-app`'s tests only.
//
// # The function under test is Go's own, reached through `Store.Load`
//
// `applyEnvKey` is unexported, so each row sets real process variables with `os.Setenv`, builds
// a `config.Store` over a `MemoryStore` holding `SetDefaults`' config, and reads `Store.Get()` —
// the same `json.Unmarshal` → `SetDefaults` → `applyEnvironmentMap` → `fixConfig` → `IsValid`
// sequence a booting server runs. Every `MM*` variable the dump process inherited is cleared
// first and restored afterwards, so the rows cannot depend on the shell that ran the generator.
//
// # What a reader could get wrong, and so what the corpus covers
//
//   - A `[]string` setting is `strings.Split(value, " ")`: spaces, never commas, and an empty
//     variable is `[""]`, not `[]`.
//   - A bool is `strconv.ParseBool`, an int `strconv.ParseInt(value, 10, 0)`; a value that does
//     not parse leaves the setting alone.
//   - A map is `json.Unmarshal` into a **fresh** map that replaces the old one whole — the
//     `SetDefaults` plugin states included — and nothing at all is assigned when it fails.
//   - The variable name is upper-cased by `GetEnvironment`, so the match is case-insensitive, and
//     a leaf ignores whatever key parts are left over (`..._SITEURL_ANYTHING` sets `SiteURL`).
//   - A feature flag is an ordinary bool or string under `*FeatureFlags`.

import (
	"fmt"
	"os"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/config"
)

type envOverrideCase struct {
	Name string
	Env  [][2]string
}

func env(pairs ...string) [][2]string {
	out := make([][2]string, 0, len(pairs)/2)
	for i := 0; i+1 < len(pairs); i += 2 {
		out = append(out, [2]string{pairs[i], pairs[i+1]})
	}
	return out
}

const (
	envEDC    = "MM_TEAMSETTINGS_EXPERIMENTALDEFAULTCHANNELS"
	envTPIH   = "MM_SERVICESETTINGS_TRUSTEDPROXYIPHEADER"
	envSPKF   = "MM_PLUGINSETTINGS_SIGNATUREPUBLICKEYFILES"
	envStates = "MM_PLUGINSETTINGS_PLUGINSTATES"
	envPlugs  = "MM_PLUGINSETTINGS_PLUGINS"
	envEmoji  = "MM_SERVICESETTINGS_ENABLECUSTOMEMOJI"
	envGorout = "MM_SERVICESETTINGS_GOROUTINEHEALTHTHRESHOLD"
	envMaxCh  = "MM_TEAMSETTINGS_MAXCHANNELSPERTEAM"
	envSite   = "MM_SERVICESETTINGS_SITEURL"
	envBoR    = "MM_FEATUREFLAGS_BURNONREAD"
	envTestFF = "MM_FEATUREFLAGS_TESTFEATURE"
)

var envOverrideCorpus = []envOverrideCase{
	{Name: "nothing set"},

	// []string: strings.Split(value, " ").
	{Name: "slice splits on a space", Env: env(envEDC, "alpha beta")},
	{Name: "slice never splits on a comma", Env: env(envEDC, "alpha,beta")},
	{Name: "slice from an empty variable is one empty name", Env: env(envEDC, "")},
	{Name: "slice keeps empty pieces between doubled spaces", Env: env(envEDC, " alpha  beta ")},
	{Name: "slice does not split on a tab", Env: env(envEDC, "alpha\tbeta")},
	{Name: "proxy header list splits on a space", Env: env(envTPIH, "X-Real-IP X-Forwarded-For")},
	{Name: "proxy header list from an empty variable", Env: env(envTPIH, "")},
	{Name: "signature key files from an empty variable", Env: env(envSPKF, "")},
	{Name: "signature key files split on a space", Env: env(envSPKF, "/a.gpg /b.gpg")},

	// The variable name.
	{Name: "a lower-case variable is upper-cased first", Env: env(strings.ToLower(envEDC), "lower")},
	{Name: "a mixed-case variable is upper-cased first", Env: env("Mm_TeamSettings_ExperimentalDefaultChannels", "mixed")},
	{Name: "a leaf ignores leftover key parts", Env: env(envSite+"_ANYTHING", "http://suffix.example.com")},
	{Name: "a doubled underscore names no field", Env: env("MM_SERVICESETTINGS__SITEURL", "http://nope.example.com")},
	{Name: "the site url is a plain string", Env: env(envSite, "http://env.example.com")},

	// bool: strconv.ParseBool.
	{Name: "bool accepts 0", Env: env(envEmoji, "0")},
	{Name: "bool accepts F", Env: env(envEmoji, "F")},
	{Name: "bool accepts FALSE", Env: env(envEmoji, "FALSE")},
	{Name: "bool rejects no", Env: env(envEmoji, "no")},
	{Name: "bool rejects an empty value", Env: env(envEmoji, "")},
	{Name: "bool rejects a padded value", Env: env(envEmoji, "false ")},
	{Name: "bool rejects fAlSe", Env: env(envEmoji, "fAlSe")},

	// int (*int): strconv.ParseInt(value, 10, 0).
	{Name: "int parses", Env: env(envGorout, "42")},
	{Name: "int accepts a plus sign", Env: env(envGorout, "+42")},
	{Name: "int accepts a minus sign", Env: env(envGorout, "-3")},
	{Name: "int rejects hex", Env: env(envGorout, "0x10")},
	{Name: "int rejects an underscore", Env: env(envGorout, "1_000")},
	{Name: "int rejects a padded value", Env: env(envGorout, " 42")},
	{Name: "int rejects a float", Env: env(envGorout, "4.0")},
	{Name: "int rejects an empty value", Env: env(envGorout, "")},
	{Name: "int rejects an overflow", Env: env(envGorout, "9223372036854775808")},

	// int64 (*int64): the same parse.
	{Name: "int64 parses", Env: env(envMaxCh, "5000")},
	{Name: "int64 takes the maximum", Env: env(envMaxCh, "9223372036854775807")},
	{Name: "int64 rejects an overflow", Env: env(envMaxCh, "9223372036854775808")},

	// map[string]*PluginState: json.Unmarshal into a fresh map.
	{Name: "states replace the whole map", Env: env(envStates, `{"x":{"Enable":true}}`)},
	{Name: "states match the field name case-insensitively", Env: env(envStates, `{"x":{"enable":true},"y":{"ENABLE":true}}`)},
	{Name: "states keep a null entry", Env: env(envStates, `{"x":null,"y":{"Enable":true}}`)},
	{Name: "states: an empty object", Env: env(envStates, `{}`)},
	{Name: "states: a null map", Env: env(envStates, `null`)},
	{Name: "states: a null Enable is false", Env: env(envStates, `{"x":{"Enable":null}}`)},
	{Name: "states: unknown keys are ignored", Env: env(envStates, `{"x":{"Enable":true,"Other":[1]}}`)},
	{Name: "states: the last duplicate wins", Env: env(envStates, `{"x":{"Enable":true},"x":{"Enable":false}}`)},
	{Name: "states: the last fold-equal key wins", Env: env(envStates, `{"x":{"Enable":true,"enable":false}}`)},
	{Name: "states: a later null Enable keeps the earlier value", Env: env(envStates, `{"x":{"Enable":true,"enable":null}}`)},
	{Name: "states: an empty id is kept", Env: env(envStates, `{"":{"Enable":true}}`)},
	{Name: "states: a string Enable is an error", Env: env(envStates, `{"x":{"Enable":"true"},"y":{"Enable":true}}`)},
	{Name: "states: a number entry is an error", Env: env(envStates, `{"x":1}`)},
	{Name: "states: an array is an error", Env: env(envStates, `[{"Enable":true}]`)},
	{Name: "states: not json is an error", Env: env(envStates, `x`)},
	{Name: "states: trailing data is an error", Env: env(envStates, `{"x":{"Enable":true}} {}`)},
	{Name: "states: an empty value is an error", Env: env(envStates, ``)},
	{Name: "states: surrounding whitespace is fine", Env: env(envStates, " {\"x\":{\"Enable\":true}}\n")},

	// map[string]map[string]any.
	{Name: "plugins replace the whole map", Env: env(envPlugs, `{"p":{"k":"v","n":1.5}}`)},
	{Name: "plugins keep a null entry", Env: env(envPlugs, `{"p":null}`)},
	{Name: "plugins: a scalar entry is an error", Env: env(envPlugs, `{"p":1}`)},
	{Name: "plugins: a leftover key part is ignored", Env: env(envPlugs+"_ANYTHING", `{"q":{}}`)},

	// Feature flags live under a pointer to a struct; the overlay reaches them like any section.
	{Name: "a bool feature flag", Env: env(envBoR, "false")},
	{Name: "a bool feature flag rejects on", Env: env(envBoR, "on")},
	{Name: "a string feature flag", Env: env(envTestFF, "some value")},

	// Several at once.
	{Name: "independent variables compose", Env: env(envEDC, "a b", envEmoji, "f", envGorout, "7")},
}

// envOverrideProjection is the subset of the loaded config the rows record: one setting of every
// kind `applyEnvKey` switches on, plus the three `[]string` settings `mm_app::config` reads.
func envOverrideProjection(c *model.Config) map[string]any {
	return map[string]any{
		"TeamSettings": map[string]any{
			"ExperimentalDefaultChannels": c.TeamSettings.ExperimentalDefaultChannels,
			"MaxChannelsPerTeam":          c.TeamSettings.MaxChannelsPerTeam,
		},
		"ServiceSettings": map[string]any{
			"TrustedProxyIPHeader":     c.ServiceSettings.TrustedProxyIPHeader,
			"EnableCustomEmoji":        c.ServiceSettings.EnableCustomEmoji,
			"GoroutineHealthThreshold": c.ServiceSettings.GoroutineHealthThreshold,
			"SiteURL":                  c.ServiceSettings.SiteURL,
		},
		"PluginSettings": map[string]any{
			"SignaturePublicKeyFiles": c.PluginSettings.SignaturePublicKeyFiles,
			"PluginStates":            c.PluginSettings.PluginStates,
			"Plugins":                 c.PluginSettings.Plugins,
		},
		"FeatureFlags": map[string]any{
			"BurnOnRead":  c.FeatureFlags.BurnOnRead,
			"TestFeature": c.FeatureFlags.TestFeature,
		},
	}
}

func loadWithEnvironment(c envOverrideCase) (map[string]any, error) {
	for _, kv := range c.Env {
		if err := os.Setenv(kv[0], kv[1]); err != nil {
			return nil, err
		}
	}
	defer func() {
		for _, kv := range c.Env {
			os.Unsetenv(kv[0])
		}
	}()

	initial := &model.Config{}
	initial.SetDefaults()
	backing, err := config.NewMemoryStoreWithOptions(&config.MemoryStoreOptions{InitialConfig: initial})
	if err != nil {
		return nil, err
	}
	store, err := config.NewStoreFromBacking(backing, nil, false)
	if err != nil {
		return nil, err
	}
	defer store.Close()
	return envOverrideProjection(store.Get()), nil
}

func writeEnvOverrideBehaviourFixture(outDir string) error {
	// Clear the inherited `MM*` variables `GetEnvironment` would otherwise read.
	saved := map[string]string{}
	for _, kv := range os.Environ() {
		key, value, _ := strings.Cut(kv, "=")
		if strings.HasPrefix(strings.ToUpper(key), "MM") {
			saved[key] = value
			os.Unsetenv(key)
		}
	}
	defer func() {
		for key, value := range saved {
			os.Setenv(key, value)
		}
	}()

	rows := make([]map[string]any, 0, len(envOverrideCorpus))
	for _, c := range envOverrideCorpus {
		got, err := loadWithEnvironment(c)
		if err != nil {
			return fmt.Errorf("%s: %w", c.Name, err)
		}
		envRows := c.Env
		if envRows == nil {
			envRows = [][2]string{}
		}
		rows = append(rows, map[string]any{"name": c.Name, "env": envRows, "want": got})
	}
	return writeJSONFixture(outDir, "behaviour_env_override.json", map[string]any{"env_override": rows})
}
