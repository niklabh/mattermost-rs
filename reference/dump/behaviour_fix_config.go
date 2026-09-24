package main

// Behavioural oracle for `fixConfig` (config/utils.go:135) and `fixInvalidLocales` (:151), as
// `Store.Load` runs them — written to fixtures/behaviour_fix_config.json and asserted by
// `mm_app::config`'s `fix_config_go_parity` module.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-app`'s tests only.
//
// # The function under test is Go's own, reached through `Store.Load`
//
// `fixConfig` is unexported. Each row unmarshals a JSON patch over `SetDefaults`' config, stores
// that as the document in a `MemoryStore`, sets real process variables, and reads both
// `Store.Get()` (the running config) and the `MemoryStore`'s saved config (what `Load` writes
// back: the document with `fixConfig` applied once and no environment).
//
// `Load` runs `fixConfig` **twice** — on the document before the environment overlay and again
// after it (store.go:290, :293) — and several rows exist only because a single pass after the
// overlay answers differently: a local driver in the document gains its slash even when the
// environment then switches the driver away, and a document's client locale is appended to
// `AvailableLocales` before the environment's is.
//
// # Supported locales
//
// `fixInvalidLocales` tests membership of `i18n.GetSupportedLocales()`, which is the `locales`
// map `TranslationsPreInit` fills from the i18n directory — only for stems in the hard-coded
// `supportedLocales` slice. A booting server runs `TranslationsPreInit` before its config store
// (cmd/mattermost/commands/server.go:42 → :51), so this oracle does too; without it the map is
// empty and every locale, `en` included, is "unsupported".

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/shared/i18n"
	"github.com/mattermost/mattermost/server/v8/config"
)

type fixConfigCase struct {
	Name string
	// Doc is a JSON patch unmarshalled over `SetDefaults`' config; empty means none.
	Doc string
	Env [][2]string
}

const (
	envFixDir    = "MM_FILESETTINGS_DIRECTORY"
	envFixDriver = "MM_FILESETTINGS_DRIVERNAME"
	envFixServer = "MM_LOCALIZATIONSETTINGS_DEFAULTSERVERLOCALE"
	envFixClient = "MM_LOCALIZATIONSETTINGS_DEFAULTCLIENTLOCALE"
	envFixAvail  = "MM_LOCALIZATIONSETTINGS_AVAILABLELOCALES"
)

func fixDoc(section, field, value string) string {
	blob, _ := json.Marshal(map[string]any{section: map[string]any{field: value}})
	return string(blob)
}

var fixConfigCorpus = []fixConfigCase{
	{Name: "nothing set"},

	// SiteURL: strings.TrimRight(value, "/") only when it ends in a slash.
	{Name: "site url from the environment loses one trailing slash", Env: env(envSite, "http://env.example.com/")},
	{Name: "site url from the environment loses several trailing slashes", Env: env(envSite, "http://env.example.com///")},
	{Name: "site url from the environment without a slash is untouched", Env: env(envSite, "http://env.example.com")},
	{Name: "site url from the environment that is only a slash becomes empty", Env: env(envSite, "/")},
	{Name: "site url from the environment keeps its path, not its slash", Env: env(envSite, "http://env.example.com/sub/path/")},
	{Name: "site url from the environment keeps an inner doubled slash", Env: env(envSite, "http://env.example.com//sub")},
	{Name: "site url from the document loses its trailing slashes", Doc: fixDoc("ServiceSettings", "SiteURL", "http://doc.example.com//")},
	{Name: "site url from the document that is only slashes becomes empty", Doc: fixDoc("ServiceSettings", "SiteURL", "///")},
	{Name: "site url from the document without a slash is untouched", Doc: fixDoc("ServiceSettings", "SiteURL", "http://doc.example.com/sub")},
	{Name: "site url from the environment overrides a slashed document", Doc: fixDoc("ServiceSettings", "SiteURL", "http://doc.example.com/"), Env: env(envSite, "http://env.example.com/")},

	// FileSettings.Directory: a local driver's non-empty directory gains one slash.
	{Name: "a local directory from the environment gains a slash", Env: env(envFixDir, "/srv/mattermost")},
	{Name: "a local directory from the environment with a slash is untouched", Env: env(envFixDir, "/srv/mattermost/")},
	{Name: "a local directory from the environment with two slashes keeps both", Env: env(envFixDir, "/srv/mattermost//")},
	{Name: "a relative local directory from the document gains a slash", Doc: fixDoc("FileSettings", "Directory", "./files")},
	{Name: "an s3 directory from the document is untouched", Doc: `{"FileSettings":{"DriverName":"amazons3","Directory":"bucket-dir"}}`},
	{Name: "an azure directory from the environment is untouched", Env: env(envFixDriver, "azureblob", envFixDir, "container-dir", "MM_FILESETTINGS_AZURESTORAGEACCOUNT", "mmaccount1", "MM_FILESETTINGS_AZURECONTAINER", "mmcontainer")},
	{Name: "an s3 driver from the environment keeps the slash the document's local driver added", Doc: fixDoc("FileSettings", "Directory", "./files"), Env: env(envFixDriver, "amazons3")},
	{Name: "a local driver from the environment gives an s3 document directory its slash", Doc: `{"FileSettings":{"DriverName":"amazons3","Directory":"bucket-dir"}}`, Env: env(envFixDriver, "local")},

	// DefaultServerLocale / DefaultClientLocale: reset to "en" when not a supported locale.
	{Name: "a supported server locale is kept", Env: env(envFixServer, "de")},
	{Name: "an unknown server locale is reset", Env: env(envFixServer, "xx")},
	{Name: "a shipped but unsupported server locale is reset", Env: env(envFixServer, "am")},
	{Name: "an upper-cased server locale is reset", Env: env(envFixServer, "EN")},
	{Name: "an empty server locale is reset", Env: env(envFixServer, "")},
	{Name: "a regional server locale is kept", Env: env(envFixServer, "pt-BR")},
	{Name: "a regional server locale with an underscore is reset", Env: env(envFixServer, "pt_BR")},
	{Name: "a supported client locale is kept", Env: env(envFixClient, "ja")},
	{Name: "an unknown client locale is reset", Env: env(envFixClient, "klingon")},
	{Name: "a shipped but unsupported client locale is reset", Doc: fixDoc("LocalizationSettings", "DefaultClientLocale", "hi")},
	{Name: "a lower-cased regional client locale is reset", Doc: fixDoc("LocalizationSettings", "DefaultClientLocale", "zh-cn")},
	{Name: "both locales bad in the document", Doc: `{"LocalizationSettings":{"DefaultServerLocale":"qq","DefaultClientLocale":"zz"}}`},

	// AvailableLocales.
	{Name: "available locales that include the client locale are kept", Env: env(envFixAvail, "de,en,fr")},
	{Name: "available locales without the client locale gain it", Env: env(envFixAvail, "de,fr")},
	{Name: "available locales with an unsupported one are cleared", Env: env(envFixAvail, "de,xx,fr")},
	{Name: "available locales with an unsupported one after the client are cleared", Env: env(envFixAvail, "en,xx")},
	{Name: "available locales with a space are cleared", Env: env(envFixAvail, "de, fr")},
	{Name: "available locales with an empty piece are cleared", Env: env(envFixAvail, "de,,fr")},
	{Name: "available locales with a trailing comma are cleared", Env: env(envFixAvail, "de,en,")},
	{Name: "available locales lose duplicates", Env: env(envFixAvail, "de,fr,de,en,fr")},
	{Name: "available locales lose duplicates and gain the client", Env: env(envFixAvail, "ja,ja")},
	{Name: "available locales gain a client locale from the environment", Env: env(envFixAvail, "de,fr", envFixClient, "ko")},
	{Name: "available locales gain en for a reset client locale", Env: env(envFixAvail, "de", envFixClient, "xx")},
	{Name: "a single available locale that is the client", Env: env(envFixAvail, "en")},
	{Name: "available locales match the client exactly, not by prefix", Env: env(envFixAvail, "en-AU", envFixClient, "en")},

	// The two passes.
	{Name: "a document client locale and an environment client locale are both appended", Doc: `{"LocalizationSettings":{"DefaultClientLocale":"fr","AvailableLocales":"de"}}`, Env: env(envFixClient, "ja")},
	{Name: "an environment client locale is appended to fixed document locales", Doc: `{"LocalizationSettings":{"DefaultClientLocale":"fr","AvailableLocales":"de,fr,de"}}`, Env: env(envFixClient, "ru")},
	{Name: "environment available locales replace the fixed document ones", Doc: `{"LocalizationSettings":{"DefaultClientLocale":"fr","AvailableLocales":"de"}}`, Env: env(envFixAvail, "es,it")},
	{Name: "a bad document client locale is reset before the environment available locales", Doc: fixDoc("LocalizationSettings", "DefaultClientLocale", "zz"), Env: env(envFixAvail, "de")},
	{Name: "a bad document available list is cleared before an environment client locale", Doc: `{"LocalizationSettings":{"AvailableLocales":"de,xx"}}`, Env: env(envFixClient, "ja")},
}

// fixConfigProjection is every setting `fixConfig` reads or writes.
func fixConfigProjection(c *model.Config) map[string]any {
	return map[string]any{
		"ServiceSettings": map[string]any{
			"SiteURL": c.ServiceSettings.SiteURL,
		},
		"FileSettings": map[string]any{
			"DriverName": c.FileSettings.DriverName,
			"Directory":  c.FileSettings.Directory,
		},
		"LocalizationSettings": map[string]any{
			"DefaultServerLocale": c.LocalizationSettings.DefaultServerLocale,
			"DefaultClientLocale": c.LocalizationSettings.DefaultClientLocale,
			"AvailableLocales":    c.LocalizationSettings.AvailableLocales,
		},
	}
}

func loadWithFixConfig(c fixConfigCase) (map[string]any, error) {
	initial := &model.Config{}
	initial.SetDefaults()
	if c.Doc != "" {
		if err := json.Unmarshal([]byte(c.Doc), initial); err != nil {
			return nil, fmt.Errorf("doc: %w", err)
		}
	}
	doc := fixConfigProjection(initial)

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

	backing, err := config.NewMemoryStoreWithOptions(&config.MemoryStoreOptions{InitialConfig: initial})
	if err != nil {
		return nil, err
	}
	store, err := config.NewStoreFromBacking(backing, nil, false)
	if err != nil {
		return nil, err
	}
	defer store.Close()

	persistedBytes, err := backing.Load()
	if err != nil {
		return nil, err
	}
	persisted := &model.Config{}
	if err := json.Unmarshal(persistedBytes, persisted); err != nil {
		return nil, err
	}

	envRows := c.Env
	if envRows == nil {
		envRows = [][2]string{}
	}
	return map[string]any{
		"name":      c.Name,
		"doc":       doc,
		"env":       envRows,
		"persisted": fixConfigProjection(persisted),
		"want":      fixConfigProjection(store.Get()),
	}, nil
}

func writeFixConfigBehaviourFixture(outDir string) error {
	i18nDir, err := filepath.Abs("../mattermost/server/i18n")
	if err != nil {
		return err
	}
	if err := i18n.TranslationsPreInit(i18nDir); err != nil {
		return fmt.Errorf("TranslationsPreInit: %w", err)
	}
	supported := make([]string, 0, len(i18n.GetSupportedLocales()))
	for locale := range i18n.GetSupportedLocales() {
		supported = append(supported, locale)
	}

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

	rows := make([]map[string]any, 0, len(fixConfigCorpus))
	for _, c := range fixConfigCorpus {
		row, err := loadWithFixConfig(c)
		if err != nil {
			return fmt.Errorf("%s: %w", c.Name, err)
		}
		rows = append(rows, row)
	}
	sort.Strings(supported)
	return writeJSONFixture(outDir, "behaviour_fix_config.json", map[string]any{
		"supported_locales": supported,
		"fix_config":        rows,
	})
}
