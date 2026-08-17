package main

// Behavioural oracle for model/preference.go, written to fixtures/behaviour_preference.json.
//
// Three things here need Go's own answer rather than a reading of the source.
//
//  1. **IsValid mixes bytes and runes.** Category and Name are length-checked with `len()`, i.e.
//     bytes, while Value goes through `utf8.RuneCountInString`. Two limits, two units, four
//     lines apart.
//
//  2. **The theme check uses `json.Decoder.Decode`, not `json.Unmarshal`.** A Decoder reads the
//     *first* JSON value in the stream and does not require EOF afterwards, so trailing garbage
//     is accepted where Unmarshal would reject it. `null` also decodes into a map without error.
//     Both are easy to get backwards from a Rust port, where `serde_json::from_str` is the
//     Unmarshal-shaped function and rejects both.
//
//  3. **PreUpdate rewrites Value even when the decode fails.** The error is deliberately
//     ignored ("the invalid preference value should get caught by IsValid before saving"), so a
//     theme preference holding unparseable JSON has `props` left nil — and `json.Marshal` of a
//     nil map is the four bytes `null`, which is then written back into Value. A theme
//     preference can therefore come out of PreUpdate holding the literal string "null".
//     Marshalling also sorts the keys and applies Go's HTML escaping, so PreUpdate is a
//     normalising step, not just a sanitiser.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePreferenceBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":  preferenceConstants(),
		"is_valid":   preferenceIsValidAll(),
		"pre_update": preferencePreUpdateAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_preference.json"), append(blob, '\n'), 0o644)
}

// preferenceConstants pins every exported constant in the file. They are plain strings a Rust
// port must transcribe, and a transcription error in a category name silently misroutes a
// preference rather than failing loudly.
func preferenceConstants() map[string]any {
	return map[string]any{
		"category_direct_channel_show":    model.PreferenceCategoryDirectChannelShow,
		"category_group_channel_show":     model.PreferenceCategoryGroupChannelShow,
		"category_tutorial_steps":         model.PreferenceCategoryTutorialSteps,
		"category_advanced_settings":      model.PreferenceCategoryAdvancedSettings,
		"category_flagged_post":           model.PreferenceCategoryFlaggedPost,
		"category_favorite_channel":       model.PreferenceCategoryFavoriteChannel,
		"category_sidebar_settings":       model.PreferenceCategorySidebarSettings,
		"category_display_settings":       model.PreferenceCategoryDisplaySettings,
		"category_system_notice":          model.PreferenceCategorySystemNotice,
		"category_last":                   model.PreferenceCategoryLast,
		"category_custom_status":          model.PreferenceCategoryCustomStatus,
		"category_notifications":          model.PreferenceCategoryNotifications,
		"category_recommended_next_steps": model.PreferenceCategoryRecommendedNextSteps,
		"recommended_next_steps":          model.PreferenceRecommendedNextSteps,
		"category_theme":                  model.PreferenceCategoryTheme,
		"category_authorized_oauth_app":   model.PreferenceCategoryAuthorizedOAuthApp,

		"name_attach_app_logs":             model.PreferenceNameAttachAppLogs,
		"name_collapsed_threads_enabled":   model.PreferenceNameCollapsedThreadsEnabled,
		"name_channel_display_mode":        model.PreferenceNameChannelDisplayMode,
		"name_collapse_setting":            model.PreferenceNameCollapseSetting,
		"name_message_display":             model.PreferenceNameMessageDisplay,
		"name_collapse_consecutive":        model.PreferenceNameCollapseConsecutive,
		"name_colorize_usernames":          model.PreferenceNameColorizeUsernames,
		"name_name_format":                 model.PreferenceNameNameFormat,
		"name_use_military_time":           model.PreferenceNameUseMilitaryTime,
		"name_show_unread_section":         model.PreferenceNameShowUnreadSection,
		"limit_visible_dms_gms":            model.PreferenceLimitVisibleDmsGms,
		"name_last_channel":                model.PreferenceNameLastChannel,
		"name_last_team":                   model.PreferenceNameLastTeam,
		"name_recent_custom_statuses":      model.PreferenceNameRecentCustomStatuses,
		"name_custom_status_tutorial":      model.PreferenceNameCustomStatusTutorialState,
		"custom_status_modal_viewed":       model.PreferenceCustomStatusModalViewed,
		"name_email_interval":              model.PreferenceNameEmailInterval,
		"name_recommended_next_steps_hide": model.PreferenceNameRecommendedNextStepsHide,

		"email_interval_no_batching_seconds": model.PreferenceEmailIntervalNoBatchingSeconds,
		"email_interval_batching_seconds":    model.PreferenceEmailIntervalBatchingSeconds,
		"email_interval_immediately":         model.PreferenceEmailIntervalImmediately,
		"email_interval_fifteen":             model.PreferenceEmailIntervalFifteen,
		"email_interval_fifteen_as_seconds":  model.PreferenceEmailIntervalFifteenAsSeconds,
		"email_interval_hour":                model.PreferenceEmailIntervalHour,
		"email_interval_hour_as_seconds":     model.PreferenceEmailIntervalHourAsSeconds,
		"cloud_user_ephemeral_info":          model.PreferenceCloudUserEphemeralInfo,

		"max_limit_visible_dms_gms_value": model.PreferenceMaxLimitVisibleDmsGmsValue,
		"max_preference_value_length":     model.MaxPreferenceValueLength,
	}
}

// --- IsValid ---------------------------------------------------------------------

type preferenceValidCase struct {
	Name       string          `json:"name"`
	Preference json.RawMessage `json:"preference"`
	ErrorID    string          `json:"error_id"`
	Detailed   string          `json:"detailed"`
}

func preferenceIsValidAll() []preferenceValidCase {
	type mut struct {
		name string
		fn   func(p *model.Preference)
	}

	muts := []mut{
		{"valid", func(p *model.Preference) {}},
		{"name_empty", func(p *model.Preference) { p.Name = "" }},

		// UserId goes through IsValidId: 26 bytes whose runes are all letters or numbers.
		{"user_id_empty", func(p *model.Preference) { p.UserId = "" }},
		{"user_id_short", func(p *model.Preference) { p.UserId = repeat("a", 25) }},
		{"user_id_long", func(p *model.Preference) { p.UserId = repeat("a", 27) }},
		{"user_id_multibyte_26_bytes", func(p *model.Preference) { p.UserId = repeat("é", 13) }},

		// Category: empty is rejected, and the limit is bytes.
		{"category_empty", func(p *model.Preference) { p.Category = "" }},
		{"category_32", func(p *model.Preference) { p.Category = repeat("a", 32) }},
		{"category_33", func(p *model.Preference) { p.Category = repeat("a", 33) }},
		{"category_32_runes_multibyte", func(p *model.Preference) { p.Category = repeat("é", 32) }},
		{"category_16_runes_multibyte", func(p *model.Preference) { p.Category = repeat("é", 16) }},

		// Name: no minimum, same byte limit.
		{"name_32", func(p *model.Preference) { p.Name = repeat("a", 32) }},
		{"name_33", func(p *model.Preference) { p.Name = repeat("a", 33) }},
		{"name_17_runes_multibyte", func(p *model.Preference) { p.Name = repeat("é", 17) }},

		// Value: runes, not bytes — the one length check in the file that counts differently.
		{"value_20000", func(p *model.Preference) { p.Value = repeat("a", 20000) }},
		{"value_20001", func(p *model.Preference) { p.Value = repeat("a", 20001) }},
		{"value_20000_runes_multibyte", func(p *model.Preference) { p.Value = repeat("é", 20000) }},
		{"value_20001_runes_multibyte", func(p *model.Preference) { p.Value = repeat("é", 20001) }},

		// Theme: json.Decoder.Decode into map[string]string.
		{"theme_object", themeValue(`{"sidebarBg":"#ffffff"}`)},
		{"theme_empty_object", themeValue(`{}`)},
		{"theme_empty_string", themeValue(``)},
		{"theme_whitespace", themeValue(`   `)},
		{"theme_null", themeValue(`null`)},
		{"theme_number_value", themeValue(`{"a":1}`)},
		{"theme_nested_object_value", themeValue(`{"a":{}}`)},
		{"theme_null_value", themeValue(`{"a":null}`)},
		{"theme_array", themeValue(`[]`)},
		{"theme_string", themeValue(`"a string"`)},
		{"theme_number", themeValue(`0`)},
		{"theme_true", themeValue(`true`)},
		{"theme_truncated", themeValue(`{"a":`)},
		{"theme_garbage", themeValue(`garbage`)},
		// A Decoder reads one value and stops; Unmarshal would reject the remainder.
		{"theme_trailing_data", themeValue(`{"a":"b"} {"c":"d"}`)},
		{"theme_trailing_garbage", themeValue(`{"a":"b"} garbage`)},
		{"theme_leading_whitespace", themeValue(`   {"a":"b"}`)},
		// The category check is exact: a near-miss is not a theme and skips the decode.
		{"theme_wrong_case_category", func(p *model.Preference) { p.Category = "Theme"; p.Value = `garbage` }},

		// Sidebar limit: strconv.Atoi, then a 1..40 range.
		{"limit_1", limitValue("1")},
		{"limit_40", limitValue("40")},
		{"limit_0", limitValue("0")},
		{"limit_41", limitValue("41")},
		{"limit_negative", limitValue("-1")},
		{"limit_plus_signed", limitValue("+5")},
		{"limit_empty", limitValue("")},
		{"limit_spaces", limitValue(" 5")},
		{"limit_float", limitValue("5.0")},
		{"limit_hex", limitValue("0x5")},
		{"limit_leading_zero", limitValue("05")},
		{"limit_overflow", limitValue("99999999999999999999")},
		{"limit_underscore", limitValue("1_0")},
		// The name must match too — the same value under another name is unchecked.
		{"limit_other_name", func(p *model.Preference) {
			p.Category = model.PreferenceCategorySidebarSettings
			p.Name = model.PreferenceNameShowUnreadSection
			p.Value = "999"
		}},
		// ...and so must the category.
		{"limit_other_category", func(p *model.Preference) {
			p.Category = model.PreferenceCategoryDisplaySettings
			p.Name = model.PreferenceLimitVisibleDmsGms
			p.Value = "999"
		}},
	}

	var res []preferenceValidCase
	for _, m := range muts {
		p := &model.Preference{
			UserId:   idA,
			Category: model.PreferenceCategoryDisplaySettings,
			Name:     model.PreferenceNameUseMilitaryTime,
			Value:    "true",
		}
		m.fn(p)

		blob, err := json.Marshal(p)
		if err != nil {
			panic(err)
		}

		c := preferenceValidCase{Name: m.name, Preference: blob}
		if appErr := p.IsValid(); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

func themeValue(value string) func(p *model.Preference) {
	return func(p *model.Preference) {
		p.Category = model.PreferenceCategoryTheme
		p.Name = idB
		p.Value = value
	}
}

func limitValue(value string) func(p *model.Preference) {
	return func(p *model.Preference) {
		p.Category = model.PreferenceCategorySidebarSettings
		p.Name = model.PreferenceLimitVisibleDmsGms
		p.Value = value
	}
}

// --- PreUpdate --------------------------------------------------------------------

type preferencePreUpdateCase struct {
	Name     string `json:"name"`
	Category string `json:"category"`
	In       string `json:"in"`
	Out      string `json:"out"`
}

func preferencePreUpdateAll() []preferencePreUpdateCase {
	cases := []struct {
		name     string
		category string
		value    string
	}{
		// Not a theme: the value is never touched, whatever it holds.
		{"non_theme_untouched", model.PreferenceCategoryDisplaySettings, `garbage`},
		{"non_theme_json_untouched", model.PreferenceCategoryDisplaySettings, `{"z":"1","a":"2"}`},

		// The three exempt keys keep any value at all.
		{"exempt_keys", model.PreferenceCategoryTheme, `{"image":"not a color","type":"Mattermost","codeTheme":"github"}`},
		// Everything else must match ^#[0-9a-fA-F]{3}([0-9a-fA-F]{3})?$ or become #ffffff.
		{"valid_six_digit", model.PreferenceCategoryTheme, `{"sidebarBg":"#1153ab"}`},
		{"valid_three_digit", model.PreferenceCategoryTheme, `{"sidebarBg":"#abc"}`},
		{"valid_uppercase", model.PreferenceCategoryTheme, `{"sidebarBg":"#ABCDEF"}`},
		{"invalid_four_digit", model.PreferenceCategoryTheme, `{"sidebarBg":"#abcd"}`},
		{"invalid_no_hash", model.PreferenceCategoryTheme, `{"sidebarBg":"1153ab"}`},
		{"invalid_eight_digit", model.PreferenceCategoryTheme, `{"sidebarBg":"#1153abcd"}`},
		{"invalid_named_color", model.PreferenceCategoryTheme, `{"sidebarBg":"red"}`},
		{"invalid_empty", model.PreferenceCategoryTheme, `{"sidebarBg":""}`},
		{"invalid_trailing_newline", model.PreferenceCategoryTheme, "{\"sidebarBg\":\"#abc\\n\"}"},

		// Key order and escaping are normalised by the re-marshal.
		{"keys_sorted", model.PreferenceCategoryTheme, `{"z":"#abc","a":"#def","m":"#123"}`},
		{"html_escaped_key", model.PreferenceCategoryTheme, `{"a<b":"#abc"}`},

		// The decode error is ignored, so props stays nil and Marshal writes "null".
		{"undecodable_becomes_null", model.PreferenceCategoryTheme, `garbage`},
		{"empty_becomes_null", model.PreferenceCategoryTheme, ``},
		{"array_becomes_null", model.PreferenceCategoryTheme, `[]`},
		{"json_null_becomes_null", model.PreferenceCategoryTheme, `null`},
		{"empty_object_stays", model.PreferenceCategoryTheme, `{}`},
		// A type error part-way through keeps what decoded before it.
		{"partial_decode", model.PreferenceCategoryTheme, `{"a":"#abc","b":1,"c":"#def"}`},
		{"trailing_data", model.PreferenceCategoryTheme, `{"a":"#abc"} {"b":"#def"}`},
	}

	var res []preferencePreUpdateCase
	for _, c := range cases {
		p := &model.Preference{
			UserId:   idA,
			Category: c.category,
			Name:     idB,
			Value:    c.value,
		}
		p.PreUpdate()
		res = append(res, preferencePreUpdateCase{
			Name: c.name, Category: c.category, In: c.value, Out: p.Value,
		})
	}
	return res
}
