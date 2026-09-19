package main

// Behavioural oracle for **`AppError.Translate`** — the step `web.Handler.handleContextError`
// (handlers.go:431) runs on every error before it is written, and which this port reproduces in
// `mm_app::i18n` and `mm_api::error`. Written to fixtures/behaviour_i18n.json.
//
// `behaviour_web_error.go` already records `T(id)` for ids with **no** template. This oracle is
// aimed at the two things that were missing and that no amount of reading go-i18n makes safe to
// guess:
//
//   - **`T(id, params)`** — go-i18n hands the params map to Go's `text/template`, so
//     `{{.Name}}` substitution, a *missing* key, a `nil` value and every scalar type's `%v`
//     spelling are the oracle's subject. `template_render` is the raw `text/template` corpus and
//     `translations` is the same thing through the real bundle over the pinned `i18n/` tree.
//   - **`i18n.T`, the server-locale function**. `model.NewAppError` translates at construction
//     with it (utils.go:374), which is the message a websocket error frame carries — nothing
//     re-translates that one. `server_translations` records it per `DefaultServerLocale`.
//
// Determinism: fixed corpora, fixed locales, no clock, no randomness. The package-level i18n
// state is left at `InitTranslations("en", "en")`, as `behaviour_web_error.go` leaves it.

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"text/template"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/shared/i18n"
)

// i18nTemplateIDs are every id **this port can raise** whose `en` translation carries a
// `{{...}}` action, so the rendered message depends on the params map. Collected by intersecting
// the ids appearing in `crates/*/src` with the templated entries of `i18n/en.json`; keep it that
// way, because an id with a template and no fixture row is exactly the one that drifts.
var i18nTemplateIDs = []string{
	"api.channel.create_channel.max_channel_limit.app_error",
	"api.channel.delete_channel.cannot.app_error",
	"api.channel.remove.default.app_error",
	"api.channel.update_channel.tried.app_error",
	"api.command.execute_command.not_found.app_error",
	"api.command_remote.remote_add_remove.help",
	"api.command_share.available_actions",
	"api.config.update_config.not_allowed_security.app_error",
	"api.context.invalid_body_param.app_error",
	"api.context.invalid_param.app_error",
	"api.context.invalid_token.error",
	"api.context.invalid_url_param.app_error",
	"api.emoji.get_multiple_by_name_too_many.request_error",
	"api.emoji.upload.large_image.too_large.app_error",
	"api.file.test_connection_unsupported_driver.app_error",
	"api.file.upload_file.incorrect_number_of_client_ids.app_error",
	"api.file.upload_file.large_image_detailed.app_error",
	"api.file.upload_file.too_large_detailed.app_error",
	"api.post.posts_by_ids.invalid_body.request_error",
	"api.post.update_post.permissions_time_limit.app_error",
	"api.property_value.patch.field_not_found.app_error",
	"api.property_value.patch.too_many_items.request_error",
	"api.roles.get_multiple_by_name_too_many.request_error",
	"api.team.invite_members.invalid_email.app_error",
	"api.team.update_restricted_domains.mismatch.app_error",
	"api.user.login.use_auth_service.app_error",
	"api.user.patch_user.login_provider_attribute_set.app_error",
	"api.user.patch_user.profile_field_locked.app_error",
	"api.user.update_user.login_provider_attribute_set.app_error",
	"api.user.update_user.profile_field_locked.app_error",
	"api.websocket_handler.invalid_param.app_error",
	"app.channel.get.existing.app_error",
	"app.channel.get.find.app_error",
	"app.group.permanent_delete_members_by_user.app_error",
	"app.group.user_not_found",
	"app.group.username_conflict",
	"app.plugin.invalid_id.app_error",
	"app.plugin.skip_installation.app_error",
	"app.property_value.resolve_broadcast_params.unknown_object_type.app_error",
	"app.property_value.upsert.field_not_found.app_error",
	"app.submit_interactive_dialog.file_not_owned",
	"app.submit_interactive_dialog.invalid_file_id",
	"app.submit_interactive_dialog.too_many_file_ids",
	"app.submit_interactive_dialog.too_many_submission_ids",
	"app.team.access_policies.channel_wrong_team.app_error",
	"app.upload.upload_data.first_part_too_small.app_error",
	"app.upload.upload_data.large_image.app_error",
	"app.user.update_auth_data.email_exists.app_error",
	"app.user_access_token.expires_at_too_far.app_error",
	"app.webhooks.get_incoming_count.app_error",
	"model.channel.is_valid.banner_info.text.invalid_length.app_error",
	"model.channel_member.is_valid.roles_limit.app_error",
	"model.incoming_hook.id.app_error",
	"model.property_field.is_valid.app_error",
	"model.property_group.is_valid.app_error",
	"model.property_value.is_valid.app_error",
	"model.session.is_valid.roles_limit.app_error",
	"model.team_member.is_valid.roles_limit.app_error",
	"model.user.is_valid.pwd_lowercase.app_error",
	"model.user.is_valid.pwd_lowercase_uppercase_number.app_error",
	"model.user.is_valid.pwd_min_length.app_error",
	"model.user.is_valid.pwd_number.app_error",
	"model.user.is_valid.pwd_symbol.app_error",
	"model.user.is_valid.pwd_uppercase.app_error",
	"model.user.is_valid.roles_limit.app_error",
}

// i18nPlainIDs are ids with no template, one per status family this server answers, plus the
// sentinel cases: an id present in `en` only, an id present in no file at all, and
// `model.NoTranslation`, whose "translation" is the sentinel string itself.
var i18nPlainIDs = []string{
	"api.context.invalid_body_param.app_error",
	"api.context.session_expired.app_error",
	"api.context.permissions.app_error",
	"api.context.404.app_error",
	"api.context.request_body_too_large.app_error",
	"api.license.upgrade_needed.app_error",
	"api.user.create_user.disabled.app_error",
	"store.sql_channel.get.existing.app_error",
	"model.user.is_valid.email.app_error",
	"no.such.translation.id",
	model.NoTranslation,
}

// i18nAcceptLanguages covers the three branches of `GetTranslationsAndLocaleFromRequest`: the
// full tag, the language-only prefix, and neither (which falls through to the default client
// locale). `fr-CA,fr;q=0.9` is the must-fall-back case — `fr-CA` is not a loaded file, `fr` is.
var i18nAcceptLanguages = []string{"", "en", "es", "pt-BR", "pt", "fr-CA,fr;q=0.9", "zh-CN", "ja", "de", "xx-YY"}

// i18nDefaultClientLocales — the second half of that choice.
var i18nDefaultClientLocales = []string{"en", "es"}

// i18nServerLocales for `i18n.T`, the function `model.NewAppError` translates with.
var i18nServerLocales = []string{"en", "es", "ja", "xx"}

var i18nActionRe = regexp.MustCompile(`\{\{\s*\.([A-Za-z_][A-Za-z0-9_]*)\s*\}\}`)

// i18nParamsFor builds a fully populated params map for an id by reading the field names out of
// its **English** template. Deriving the keys rather than listing them is what keeps the corpus
// honest: a template that gains a field gains a fixture row for it on the next generator run.
func i18nParamsFor(enTranslation string) map[string]any {
	fields := map[string]bool{}
	for _, m := range i18nActionRe.FindAllStringSubmatch(enTranslation, -1) {
		fields[m[1]] = true
	}
	if len(fields) == 0 {
		return nil
	}
	names := make([]string, 0, len(fields))
	for f := range fields {
		names = append(names, f)
	}
	sort.Strings(names)
	params := make(map[string]any, len(names))
	for i, f := range names {
		// Distinctive per field and per position, so a port that substituted the wrong key would
		// produce a different sentence rather than an identical one.
		params[f] = fmt.Sprintf("v%d-%s", i+1, f)
	}
	return params
}

// enTranslations reads `i18n/en.json` for the template sources the corpus is derived from. It is
// the same file the bundle loaded; reading it again only recovers the raw strings.
func enTranslations(dir string) (map[string]string, error) {
	blob, err := os.ReadFile(filepath.Join(dir, "en.json"))
	if err != nil {
		return nil, err
	}
	var entries []struct {
		ID          string `json:"id"`
		Translation any    `json:"translation"`
	}
	if err := json.Unmarshal(blob, &entries); err != nil {
		return nil, err
	}
	out := make(map[string]string, len(entries))
	for _, e := range entries {
		if s, ok := e.Translation.(string); ok {
			out[e.ID] = s
		}
	}
	return out, nil
}

// i18nTemplateCorpus is the raw `text/template` corpus: go-i18n's `translation.template.Execute`
// is `text/template` with the translation's own source as the template name, so this pins the
// `%v` spelling of every value kind a params map can hold and what a *missing* key renders as.
var i18nTemplateCorpus = []struct {
	Src  string
	Data any
}{
	{"Invalid or missing {{.Name}} parameter in request URL.", map[string]any{"Name": "user_id"}},
	{"Invalid or missing {{.Name}} parameter in request URL.", map[string]any{}},
	{"Invalid or missing {{.Name}} parameter in request URL.", nil},
	{"Restricting team to {{ .Domain }} is not allowed.", map[string]any{"Domain": "example.com"}},
	{"{{.A}} then {{.B}} then {{.A}}", map[string]any{"A": "first", "B": "second"}},
	{"{{.A}} and {{.B}}", map[string]any{"A": "only"}},
	{"no actions at all", map[string]any{"A": "unused"}},
	{"", map[string]any{"A": "unused"}},
	{"{{.N}}", map[string]any{"N": 42}},
	{"{{.N}}", map[string]any{"N": int64(-9007199254740993)}},
	{"{{.N}}", map[string]any{"N": 0}},
	{"{{.F}}", map[string]any{"F": 1.5}},
	{"{{.F}}", map[string]any{"F": float64(3)}},
	{"{{.F}}", map[string]any{"F": 1e21}},
	{"{{.F}}", map[string]any{"F": 1e-7}},
	{"{{.F}}", map[string]any{"F": -0.125}},
	{"{{.B}}/{{.C}}", map[string]any{"B": true, "C": false}},
	{"{{.Z}}", map[string]any{"Z": nil}},
	{"{{.S}}", map[string]any{"S": ""}},
	{"{{.S}}", map[string]any{"S": "<b>&amp;</b>"}},
	{"{{.A}}", map[string]any{"A": []any{1, "x", true}}},
	{"{{.M}}", map[string]any{"M": map[string]any{"b": 2, "a": 1}}},
	{"leading {{.X}} trailing", nil},
	{"{{.X}}", nil},
	{"multi\nline {{.X}} here", map[string]any{"X": "v"}},
	{"unicode {{.X}} é ", map[string]any{"X": "café"}},
}

// executeGoTemplate is `translation.template.Execute` (go-i18n translation/template.go:35): the
// source is both the template's name and its text, an unparsed source (no `{{`) is returned as
// is, and an execution error becomes the *message* of that error.
func executeGoTemplate(src string, data any) string {
	if !bytes.Contains([]byte(src), []byte("{{")) {
		return src
	}
	tmpl, err := template.New(src).Parse(src)
	if err != nil {
		return "PARSE ERROR: " + err.Error()
	}
	var buf bytes.Buffer
	if err := tmpl.Execute(&buf, data); err != nil {
		return err.Error()
	}
	return buf.String()
}

func writeI18nBehaviourFixture(outDir string) error {
	i18nDir, err := filepath.Abs("../mattermost/server/i18n")
	if err != nil {
		return err
	}
	if err := i18n.TranslationsPreInit(i18nDir); err != nil {
		return fmt.Errorf("TranslationsPreInit: %w", err)
	}
	en, err := enTranslations(i18nDir)
	if err != nil {
		return err
	}

	var renders []map[string]any
	for _, c := range i18nTemplateCorpus {
		renders = append(renders, map[string]any{
			"src":    c.Src,
			"data":   c.Data,
			"output": executeGoTemplate(c.Src, c.Data),
		})
	}

	// `AppError.Translate(T)` per (default client locale, Accept-Language, id, params variant).
	// Recorded through `model.NewAppError` + `Translate` rather than by calling `T` directly, so
	// the `params == nil` branch (utils.go:291) is the fixture's as well as go-i18n's.
	var translations []map[string]any
	emit := func(def, al, id string, variant string, params map[string]any) {
		req := httptest.NewRequest(http.MethodGet, "/api/v4/users/me", nil)
		if al != "" {
			req.Header.Set("Accept-Language", al)
		}
		t, locale := i18n.GetTranslationsAndLocaleFromRequest(req)
		err := model.NewAppError("Oracle", id, params, "detail", http.StatusBadRequest)
		err.Translate(t)
		translations = append(translations, map[string]any{
			"default_client_locale": def,
			"accept_language":       al,
			"locale":                locale,
			"id":                    id,
			"variant":               variant,
			"params":                params,
			"message":               err.Message,
		})
	}
	for _, def := range i18nDefaultClientLocales {
		if err := i18n.InitTranslations("en", def); err != nil {
			return fmt.Errorf("InitTranslations(%q): %w", def, err)
		}
		for _, al := range i18nAcceptLanguages {
			for _, id := range i18nPlainIDs {
				emit(def, al, id, "none", nil)
			}
			for _, id := range i18nTemplateIDs {
				params := i18nParamsFor(en[id])
				emit(def, al, id, "full", params)
				emit(def, al, id, "empty", map[string]any{})
				emit(def, al, id, "none", nil)
				// One key dropped: what a port that filled the map from the wrong place emits.
				if len(params) > 1 {
					partial := map[string]any{}
					names := make([]string, 0, len(params))
					for k := range params {
						names = append(names, k)
					}
					sort.Strings(names)
					for _, k := range names[1:] {
						partial[k] = params[k]
					}
					emit(def, al, id, "partial", partial)
				}
			}
		}
	}

	// `i18n.T`, the server-locale function `model.NewAppError` translates with — the message a
	// websocket error frame carries, since nothing re-translates one.
	//
	// `Translate(i18n.T)` is called explicitly rather than relying on `NewAppError` to do it,
	// because `NewAppError` uses the package var `AppErrorInit` sets **once** per process
	// (utils.go:225). Calling `AppErrorInit` here would translate every `AppError` every *other*
	// oracle in this generator constructs, rewriting committed fixtures for a reason that has
	// nothing to do with them. The two paths are otherwise the same line of code.
	var serverTranslations []map[string]any
	for _, server := range i18nServerLocales {
		// `InitTranslations` returns an error for a locale with no file and falls back to `en`;
		// that fallback is part of what is being recorded, so the error is kept, not returned.
		initErr := i18n.InitTranslations(server, "en")
		for _, id := range i18nPlainIDs {
			err := model.NewAppError("Oracle", id, nil, "detail", http.StatusBadRequest)
			err.Translate(i18n.T)
			serverTranslations = append(serverTranslations, map[string]any{
				"default_server_locale": server,
				"init_error":            i18nErrString(initErr),
				"id":                    id,
				"variant":               "none",
				"params":                nil,
				"message":               err.Message,
			})
		}
		for _, id := range i18nTemplateIDs {
			params := i18nParamsFor(en[id])
			err := model.NewAppError("Oracle", id, params, "detail", http.StatusBadRequest)
			err.Translate(i18n.T)
			serverTranslations = append(serverTranslations, map[string]any{
				"default_server_locale": server,
				"init_error":            i18nErrString(initErr),
				"id":                    id,
				"variant":               "full",
				"params":                params,
				"message":               err.Message,
			})
		}
	}

	// `SkipTranslation` short-circuits `Translate` (utils.go:282); nothing in the reference tree
	// sets it, so the row exists to pin that the port's branch is the same no-op.
	skipped := model.NewAppError("Oracle", "api.context.session_expired.app_error", nil, "", http.StatusUnauthorized)
	skipped.Message = "left alone"
	skipped.SkipTranslation = true
	if err := i18n.InitTranslations("en", "en"); err != nil {
		return err
	}
	skipped.Translate(i18n.T)

	// Every `{{` action the loaded files contain, over all 22 supported locales — the set the
	// port's template parser must handle. A new upstream construct shows up here as a new key.
	actions, err := i18nActionsInLoadedFiles(i18nDir)
	if err != nil {
		return err
	}

	out := map[string]any{
		"template_render":      renders,
		"translations":         translations,
		"server_translations":  serverTranslations,
		"skip_translation":     skipped.Message,
		"loaded_file_actions":  actions,
		"supported_locales":    supportedLocalesInOrder(),
		"no_translation_token": model.NoTranslation,
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_i18n.json"), append(blob, '\n'), 0o644)
}

// supportedLocalesInOrder is `i18n.GetSupportedLocales()`' key set, sorted — the files the bundle
// actually loaded, as opposed to the 55 the directory ships.
func supportedLocalesInOrder() []string {
	locales := i18n.GetSupportedLocales()
	out := make([]string, 0, len(locales))
	for k := range locales {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

var i18nAnyActionRe = regexp.MustCompile(`\{\{(.*?)\}\}`)

// i18nActionsInLoadedFiles collects the distinct `{{...}}` action bodies of every **loaded**
// locale file, with a count, and separately the ids whose translation is a plural object. A port
// that renders only `{{.Field}}` is correct exactly as long as these two stay as they are.
func i18nActionsInLoadedFiles(dir string) (map[string]any, error) {
	counts := map[string]int{}
	var pluralIDs []string
	seenPlural := map[string]bool{}
	locales := i18n.GetSupportedLocales()
	names := make([]string, 0, len(locales))
	for k := range locales {
		names = append(names, k)
	}
	sort.Strings(names)
	for _, locale := range names {
		blob, err := os.ReadFile(filepath.Join(dir, locale+".json"))
		if err != nil {
			return nil, err
		}
		var entries []struct {
			ID          string `json:"id"`
			Translation any    `json:"translation"`
		}
		if err := json.Unmarshal(blob, &entries); err != nil {
			return nil, err
		}
		for _, e := range entries {
			s, ok := e.Translation.(string)
			if !ok {
				if !seenPlural[e.ID] {
					seenPlural[e.ID] = true
					pluralIDs = append(pluralIDs, e.ID)
				}
				continue
			}
			for _, m := range i18nAnyActionRe.FindAllStringSubmatch(s, -1) {
				counts[m[1]]++
			}
		}
	}
	sort.Strings(pluralIDs)
	return map[string]any{"actions": counts, "plural_ids": pluralIDs}, nil
}

// i18nErrString is `errString` for a nullable error — the fixture records `null` rather than the
// empty string, because "no error" and "an error whose text is empty" are different facts.
func i18nErrString(err error) any {
	if err == nil {
		return nil
	}
	return err.Error()
}
