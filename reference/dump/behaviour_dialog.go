package main

// Behavioural oracle for the Dialog family — integration_action.go chunk 2.
// Written to fixtures/behaviour_dialog.json.
//
// Interactive dialogs are the largest validator in the model package: DialogElement.IsValid is a
// 156-line switch whose every arm has its own length caps, its own allowed values and its own
// error wording, and the whole family accumulates into a *multierror.Error — so the **count and
// order of the messages** is the output, not merely the fact of failure. Each case therefore
// records the ordered message list *and* the joined Error() string.
//
// Five things need Go's own answer:
//
//  1. **`time.Parse` decides which date strings are valid**, across five layouts. Go accepts a
//     fractional second the layout never mentions, requires fixed-width digits, and rejects
//     lowercase `t`/`z`. The `date_formats` section runs one corpus through the three call sites
//     that differ (a date default, a datetime default, and a datetime min/max).
//
//  2. **A valid *datetime* in a date field is an error, not a pass** — `validateDateFormat`
//     returns a "warning" phrased as an error, carrying the truncated date back to the caller.
//
//  3. **`%q` is `strconv.Quote`, not Rust's `{:?}`.** Half these messages interpolate user input
//     with `%q`. The `quote` section pins the shim; it is a shared helper measured here because
//     this is the file that needed it, the same way go_format_v was measured in the message
//     attachment oracle.
//
//  4. **Dialog.IsValid wraps each element failure with errors.Wrapf**, which nests a whole
//     multierror layout inside one parent message rather than flattening it the way
//     multierror.Prefix does in PostAction.IsValid. Two different composition rules in one file.
//
//  5. **The `text`/`textarea` subtype failure reports the element's *type*, not its subtype**
//     (`errors.Errorf("invalid subtype %q", e.Type)`), which is an upstream copy-paste bug that
//     reaches integration developers.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"

	"github.com/hashicorp/go-multierror"
	"github.com/mattermost/mattermost/server/public/model"
)

func writeDialogBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":                         dialogWireAll(),
		"constants":                    dialogConstants(),
		"quote":                        goQuoteAll(),
		"lookup_url":                   lookupURLAll(),
		"effective_datetime_config":    effectiveDateTimeConfigAll(),
		"element_is_valid":             dialogElementIsValidAll(),
		"date_formats":                 dialogDateFormatsAll(),
		"dialog_is_valid":              dialogIsValidAll(),
		"open_dialog_request_is_valid": openDialogRequestIsValidAll(),
		"submit_response_is_valid":     submitDialogResponseIsValidAll(),
		"time_interval":                dialogTimeIntervalAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_dialog.json"), append(blob, '\n'), 0o644)
}

// errorMessages flattens a validator's result into the ordered list a multierror holds, plus the
// rendered Error() string. A non-multierror is a single message.
func errorMessages(err error) ([]string, string) {
	if err == nil {
		return []string{}, ""
	}
	if m, ok := err.(*multierror.Error); ok {
		msgs := make([]string, 0, len(m.Errors))
		for _, e := range m.Errors {
			msgs = append(msgs, e.Error())
		}
		return msgs, err.Error()
	}
	return []string{err.Error()}, err.Error()
}

// --- constants --------------------------------------------------------------------------------

func dialogConstants() map[string]any {
	return map[string]any{
		"dialog_title_max_length":                    model.DialogTitleMaxLength,
		"dialog_element_display_name_max_length":     model.DialogElementDisplayNameMaxLength,
		"dialog_element_name_max_length":             model.DialogElementNameMaxLength,
		"dialog_element_help_text_max_length":        model.DialogElementHelpTextMaxLength,
		"dialog_element_text_max_length":             model.DialogElementTextMaxLength,
		"dialog_element_textarea_max_length":         model.DialogElementTextareaMaxLength,
		"dialog_element_select_max_length":           model.DialogElementSelectMaxLength,
		"dialog_element_bool_max_length":             model.DialogElementBoolMaxLength,
		"dialog_element_file_max_length":             model.DialogElementFileMaxLength,
		"default_time_interval_minutes":              model.DefaultTimeIntervalMinutes,
		"max_dialog_file_ids":                        model.MaxDialogFileIds,
		"max_dialog_submission_id_shaped_token_scan": model.MaxDialogSubmissionIDShapedTokenScan,
		"iso_date_format":                            model.ISODateFormat,
		"iso_date_time_format":                       model.ISODateTimeFormat,
		"iso_date_time_with_timezone_format":         model.ISODateTimeWithTimezoneFormat,
		"iso_date_time_no_timezone_format":           model.ISODateTimeNoTimezoneFormat,
		"iso_date_time_no_seconds_format":            model.ISODateTimeNoSecondsFormat,
		"submit_dialog_response_type_empty":          string(model.SubmitDialogResponseTypeEmpty),
		"submit_dialog_response_type_ok":             string(model.SubmitDialogResponseTypeOK),
		"submit_dialog_response_type_form":           string(model.SubmitDialogResponseTypeForm),
		"submit_dialog_response_type_navigate":       string(model.SubmitDialogResponseTypeNavigate),
	}
}

// --- strconv.Quote ----------------------------------------------------------------------------

// goQuoteAll pins fmt's %q verb for a string, which is strconv.Quote. Rust's {:?} agrees on
// ordinary text and diverges on control characters and non-printables, so every message that
// interpolates user input with %q needs this.
func goQuoteAll() []map[string]string {
	inputs := []string{
		"", "abc", "a b", `a"b`, `a\\b`, "a'b",
		"tab\there", "nl\nhere", "cr\rhere",
		"\a", "\b", "\f", "\v", "\x00", "\x1b", "\x7f",
		"\u00e9", "\u65e5\u672c\u8a9e", "\U0001F600",
		"nbsp\u00a0here", "ideographic\u3000space", "zwsp\u200bhere",
		"nel\u0085here", "bom\ufeffhere", "combining e\u0301",
		"flag \U0001F1E8\U0001F1E6",
		"\xff", "\xc3\x28",
		"mixed \x01 \u00e9 \n end",
	}

	res := make([]map[string]string, 0, len(inputs))
	for _, in := range inputs {
		res = append(res, map[string]string{
			"input": in,
			// The input is recorded twice: raw (which JSON may itself escape) and quoted, so a
			// reader can tell the two layers apart.
			"input_quoted": strconv.Quote(in),
			"quoted":       strconv.Quote(in),
		})
	}
	return res
}

// --- IsValidLookupURL -------------------------------------------------------------------------

func lookupURLAll() []map[string]any {
	inputs := []string{
		"", " ", "https://example.com", "http://example.com/x?q=1", "ftp://example.com",
		"example.com", "/api/v4/x",
		"/plugins/x", "/plugins/", "/plugins/x/y", "plugins/x", "./plugins/x",
		"/plugins/../etc", "/plugins/x/../y", "/plugins//x", "/plugins/x//y",
		"/plugins/x..y", "/plugins/x.y",
		"https://example.com/../x", "https://example.com//x",
		"https://example.com/plugins/x",
	}

	res := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		res = append(res, map[string]any{"url": in, "valid": model.IsValidLookupURL(in)})
	}
	return res
}

// --- EffectiveDateTimeConfig ------------------------------------------------------------------

func effectiveDateTimeConfigAll() []map[string]any {
	cases := []struct {
		name string
		e    model.DialogElement
	}{
		{"zero", model.DialogElement{}},
		{"deprecated_only", model.DialogElement{
			MinDate: "2020-01-01", MaxDate: "2020-12-31", TimeInterval: 15,
		}},
		{"config_wins", model.DialogElement{
			MinDate: "2020-01-01", MaxDate: "2020-12-31", TimeInterval: 15,
			DateTimeConfig: &model.DialogDateTimeConfig{
				MinDate: "2021-01-01", MaxDate: "2021-12-31", TimeInterval: 30,
			},
		}},
		// An empty string and a zero interval do NOT override, so the deprecated value survives.
		{"config_empty_keeps_deprecated", model.DialogElement{
			MinDate: "2020-01-01", MaxDate: "2020-12-31", TimeInterval: 15,
			DateTimeConfig: &model.DialogDateTimeConfig{},
		}},
		{"config_zero_interval_keeps_deprecated", model.DialogElement{
			TimeInterval:   15,
			DateTimeConfig: &model.DialogDateTimeConfig{TimeInterval: 0},
		}},
		// LocationTimezone has no deprecated counterpart, so it is copied unconditionally —
		// which means a non-nil config *clears* nothing and a nil config leaves it empty.
		{"location_timezone", model.DialogElement{
			DateTimeConfig: &model.DialogDateTimeConfig{LocationTimezone: "Asia/Tokyo"},
		}},
		{"manual_time_entry", model.DialogElement{
			DateTimeConfig: &model.DialogDateTimeConfig{ManualTimeEntry: true},
		}},
		{"allow_manual_time_entry_deprecated", model.DialogElement{
			DateTimeConfig: &model.DialogDateTimeConfig{AllowManualTimeEntry: true},
		}},
		{"manual_time_entry_ored", model.DialogElement{
			DateTimeConfig: &model.DialogDateTimeConfig{
				ManualTimeEntry: false, AllowManualTimeEntry: true,
			},
		}},
	}

	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		element, err := json.Marshal(&c.e)
		if err != nil {
			panic(err)
		}
		cfg := c.e.EffectiveDateTimeConfig()
		out, err := json.Marshal(&cfg)
		if err != nil {
			panic(err)
		}
		res = append(res, map[string]any{
			"name":    c.name,
			"element": json.RawMessage(element),
			"config":  json.RawMessage(out),
		})
	}
	return res
}

// --- DialogElement.IsValid --------------------------------------------------------------------

type dialogValidCase struct {
	Name     string          `json:"name"`
	Input    json.RawMessage `json:"input"`
	Messages []string        `json:"messages"`
	Error    string          `json:"error"`
}

// named returns an element with the two required fields filled in, so a case's corpus value is
// the only thing producing messages.
func namedElement(t string) model.DialogElement {
	return model.DialogElement{DisplayName: "Display", Name: "name", Type: t}
}

func opt(text, value string) *model.PostActionOptions {
	return &model.PostActionOptions{Text: text, Value: value}
}

func dialogElementIsValidAll() []dialogValidCase {
	long := func(n int) string {
		b := make([]byte, n)
		for i := range b {
			b[i] = 'x'
		}
		return string(b)
	}

	withText := namedElement("text")
	withText.SubType = "email"

	cases := []struct {
		name string
		e    model.DialogElement
	}{
		{"zero", model.DialogElement{}},
		{"valid_text", namedElement("text")},
		{"valid_text_subtype", withText},

		// --- shared checks ------------------------------------------------------------
		{"missing_display_name", model.DialogElement{Name: "n", Type: "text"}},
		{"missing_name", model.DialogElement{DisplayName: "d", Type: "text"}},
		{"display_name_at_limit", func() model.DialogElement {
			e := namedElement("text")
			e.DisplayName = long(model.DialogElementDisplayNameMaxLength)
			return e
		}()},
		{"display_name_over_limit", func() model.DialogElement {
			e := namedElement("text")
			e.DisplayName = long(model.DialogElementDisplayNameMaxLength + 1)
			return e
		}()},
		{"name_over_limit", func() model.DialogElement {
			e := namedElement("text")
			e.Name = long(model.DialogElementNameMaxLength + 1)
			return e
		}()},
		{"help_text_over_limit", func() model.DialogElement {
			e := namedElement("text")
			e.HelpText = long(model.DialogElementHelpTextMaxLength + 1)
			return e
		}()},
		// The caps are byte lengths, so multi-byte characters hit them sooner.
		{"display_name_multibyte", func() model.DialogElement {
			e := namedElement("text")
			e.DisplayName = "ééééééééééééé" // 13 runes, 26 bytes
			return e
		}()},
		{"negative_min_length", func() model.DialogElement {
			e := namedElement("text")
			e.MinLength = -1
			return e
		}()},
		{"min_greater_than_max", func() model.DialogElement {
			e := namedElement("text")
			e.MinLength, e.MaxLength = 5, 2
			return e
		}()},
		{"min_equals_max", func() model.DialogElement {
			e := namedElement("text")
			e.MinLength, e.MaxLength = 2, 2
			return e
		}()},
		// MaxLength defaults to 0, so any positive MinLength fails against it.
		{"min_length_without_max", func() model.DialogElement {
			e := namedElement("text")
			e.MinLength = 1
			return e
		}()},
		{"multiselect_on_text", func() model.DialogElement {
			e := namedElement("text")
			e.MultiSelect = true
			return e
		}()},
		{"allow_multiple_on_text", func() model.DialogElement {
			e := namedElement("text")
			e.AllowMultiple = true
			return e
		}()},

		// --- text / textarea ----------------------------------------------------------
		{"text_default_over_limit", func() model.DialogElement {
			e := namedElement("text")
			e.Default = long(model.DialogElementTextMaxLength + 1)
			return e
		}()},
		{"text_placeholder_over_limit", func() model.DialogElement {
			e := namedElement("text")
			e.Placeholder = long(model.DialogElementTextMaxLength + 1)
			return e
		}()},
		{"text_bad_subtype", func() model.DialogElement {
			e := namedElement("text")
			e.SubType = "nope"
			return e
		}()},
		{"textarea_valid", namedElement("textarea")},
		{"textarea_default_over_limit", func() model.DialogElement {
			e := namedElement("textarea")
			e.Default = long(model.DialogElementTextareaMaxLength + 1)
			return e
		}()},
		{"textarea_bad_subtype", func() model.DialogElement {
			e := namedElement("textarea")
			e.SubType = "nope"
			return e
		}()},

		// --- select -------------------------------------------------------------------
		{"select_with_options", func() model.DialogElement {
			e := namedElement("select")
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			e.Default = "1"
			return e
		}()},
		{"select_default_not_in_options", func() model.DialogElement {
			e := namedElement("select")
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			e.Default = "2"
			return e
		}()},
		{"select_empty_default_is_fine", func() model.DialogElement {
			e := namedElement("select")
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			return e
		}()},
		{"select_no_options_no_default", namedElement("select")},
		{"select_nil_option_element", func() model.DialogElement {
			e := namedElement("select")
			e.Options = []*model.PostActionOptions{nil, opt("a", "1")}
			e.Default = "1"
			return e
		}()},
		{"select_data_source_users", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource = "users"
			return e
		}()},
		{"select_data_source_bad", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource = "nope"
			return e
		}()},
		// A bad data source is not "", so the default-in-options branch is skipped entirely.
		{"select_bad_data_source_skips_default_check", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource, e.Default = "nope", "missing"
			return e
		}()},
		{"select_dynamic_without_url", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource = "dynamic"
			return e
		}()},
		{"select_dynamic_with_url", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource, e.DataSourceURL = "dynamic", "https://example.com/lookup"
			return e
		}()},
		{"select_dynamic_plugin_url", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource, e.DataSourceURL = "dynamic", "/plugins/x/lookup"
			return e
		}()},
		{"select_dynamic_bad_url", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource, e.DataSourceURL = "dynamic", "not a url"
			return e
		}()},
		{"select_dynamic_with_options", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource, e.DataSourceURL = "dynamic", "https://example.com/lookup"
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			return e
		}()},
		// A dynamic select never checks its default against anything.
		{"select_dynamic_ignores_default", func() model.DialogElement {
			e := namedElement("select")
			e.DataSource, e.DataSourceURL = "dynamic", "https://example.com/lookup"
			e.Default = "anything"
			return e
		}()},
		{"multiselect_all_present", func() model.DialogElement {
			e := namedElement("select")
			e.MultiSelect = true
			e.Options = []*model.PostActionOptions{opt("a", "1"), opt("b", "2")}
			e.Default = "1,2"
			return e
		}()},
		{"multiselect_spaces_stripped", func() model.DialogElement {
			e := namedElement("select")
			e.MultiSelect = true
			e.Options = []*model.PostActionOptions{opt("a", "1"), opt("b", "2")}
			e.Default = " 1 , 2 "
			return e
		}()},
		// Spaces are stripped from the whole string, not trimmed per value — so an option
		// value containing a space can never match.
		{"multiselect_option_value_with_space", func() model.DialogElement {
			e := namedElement("select")
			e.MultiSelect = true
			e.Options = []*model.PostActionOptions{opt("a", "with space")}
			e.Default = "with space"
			return e
		}()},
		{"multiselect_missing_value", func() model.DialogElement {
			e := namedElement("select")
			e.MultiSelect = true
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			e.Default = "1,3"
			return e
		}()},
		{"multiselect_trailing_comma", func() model.DialogElement {
			e := namedElement("select")
			e.MultiSelect = true
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			e.Default = "1,"
			return e
		}()},
		{"select_default_over_limit", func() model.DialogElement {
			e := namedElement("select")
			e.Default = long(model.DialogElementSelectMaxLength + 1)
			e.Options = []*model.PostActionOptions{opt("a", e.Default)}
			return e
		}()},

		// --- bool ---------------------------------------------------------------------
		{"bool_valid_true", func() model.DialogElement {
			e := namedElement("bool")
			e.Default = "true"
			return e
		}()},
		{"bool_valid_empty", namedElement("bool")},
		{"bool_bad_default", func() model.DialogElement {
			e := namedElement("bool")
			e.Default = "TRUE"
			return e
		}()},
		{"bool_placeholder_over_limit", func() model.DialogElement {
			e := namedElement("bool")
			e.Placeholder = long(model.DialogElementBoolMaxLength + 1)
			return e
		}()},
		// bool never checks Default's length.
		{"bool_long_default_not_checked", func() model.DialogElement {
			e := namedElement("bool")
			e.Default = long(5000)
			return e
		}()},

		// --- radio --------------------------------------------------------------------
		{"radio_default_in_options", func() model.DialogElement {
			e := namedElement("radio")
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			e.Default = "1"
			return e
		}()},
		{"radio_default_missing", func() model.DialogElement {
			e := namedElement("radio")
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			e.Default = "2"
			return e
		}()},
		// radio checks nothing else — no length caps at all.
		{"radio_long_placeholder_not_checked", func() model.DialogElement {
			e := namedElement("radio")
			e.Placeholder = long(5000)
			return e
		}()},

		// --- file ---------------------------------------------------------------------
		{"file_valid", namedElement("file")},
		{"file_one_id", func() model.DialogElement {
			e := namedElement("file")
			e.Default = idA
			return e
		}()},
		{"file_two_ids_without_allow_multiple", func() model.DialogElement {
			e := namedElement("file")
			e.Default = idA + "," + idB
			return e
		}()},
		{"file_two_ids_with_allow_multiple", func() model.DialogElement {
			e := namedElement("file")
			e.AllowMultiple = true
			e.Default = idA + "," + idB
			return e
		}()},
		{"file_bad_id", func() model.DialogElement {
			e := namedElement("file")
			e.Default = "nope"
			return e
		}()},
		{"file_blank_segments_skipped", func() model.DialogElement {
			e := namedElement("file")
			e.Default = " " + idA + " , , "
			return e
		}()},
		{"file_with_options", func() model.DialogElement {
			e := namedElement("file")
			e.Options = []*model.PostActionOptions{opt("a", "1")}
			return e
		}()},
		{"file_with_data_source", func() model.DialogElement {
			e := namedElement("file")
			e.DataSource = "users"
			return e
		}()},
		{"file_placeholder_over_limit", func() model.DialogElement {
			e := namedElement("file")
			e.Placeholder = long(model.DialogElementFileMaxLength + 1)
			return e
		}()},

		// --- action_button ------------------------------------------------------------
		{"action_button_missing_config", namedElement("action_button")},
		{"action_button_empty_url", func() model.DialogElement {
			e := namedElement("action_button")
			e.ActionButton = &model.DialogActionButton{}
			return e
		}()},
		{"action_button_bad_url", func() model.DialogElement {
			e := namedElement("action_button")
			e.ActionButton = &model.DialogActionButton{URL: "not a url"}
			return e
		}()},
		{"action_button_valid", func() model.DialogElement {
			e := namedElement("action_button")
			e.ActionButton = &model.DialogActionButton{
				URL: "/plugins/x/do", Context: map[string]string{"k": "v"},
			}
			return e
		}()},

		// --- unknown ------------------------------------------------------------------
		{"unknown_type", namedElement("nope")},
		{"empty_type", func() model.DialogElement {
			e := namedElement("")
			return e
		}()},
	}

	res := make([]dialogValidCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.e)
		if err != nil {
			panic(err)
		}
		e := c.e
		msgs, joined := errorMessages(e.IsValid())
		res = append(res, dialogValidCase{
			Name: c.name, Input: blob, Messages: msgs, Error: joined,
		})
	}
	return res
}

// --- the date/datetime corpus -----------------------------------------------------------------

// dialogDateFormatsAll runs one corpus through the three validators that differ, by way of the
// three element shapes that reach them. The messages are the whole output — a "valid datetime in
// a date field" is a message, not a pass.
func dialogDateFormatsAll() []map[string]any {
	inputs := []string{
		"",
		// relative words
		"today", "tomorrow", "yesterday", "Today", "TOMORROW", " today",
		// relative patterns: +/- , 1..3 digits, one of dwmHMS
		"+1d", "-1d", "+0d", "+12d", "+123d", "+1234d", "+1w", "+1m", "+1H", "+1M", "+1S",
		"+1h", "+1s", "+1D", "+1W", "+1x", "+d", "+1", "1d", "++5d", "+-5d", "+ 5d", "+5 d",
		"-99S", "+999M", "+1000M",
		// ISO dates
		"2023-01-02", "2023-1-2", "23-01-02", "2023-01-2", "2023-13-01", "2023-02-29",
		"2024-02-29", "2023-02-30", "2023-00-10", "2023-01-00", "2023-01-32",
		"0000-01-01", "9999-12-31", "10000-01-01", "-001-01-01",
		"2023-01-02 ", " 2023-01-02", "2023-01-02x",
		// ISO datetimes
		"2023-01-02T15:04:05Z", "2023-01-02T15:04:05z", "2023-01-02t15:04:05Z",
		"2023-01-02T15:04:05", "2023-01-02T15:04", "2023-01-02T15:04:05+05:30",
		"2023-01-02T15:04:05-07:00", "2023-01-02T15:04:05+0530", "2023-01-02T15:04:05Z+05:00",
		"2023-01-02T15:04:05.123Z", "2023-01-02T15:04:05.123456789Z", "2023-01-02T15:04:05.Z",
		"2023-01-02T15:04:05.123", "2023-01-02T15:04.5",
		"2023-01-02T24:00:00Z", "2023-01-02T23:59:60Z", "2023-01-02T15:60:00Z",
		"2023-01-02T5:04:05Z", "2023-01-02T15:04:05",
		// junk
		"nope", "2023", "2023-01", "T15:04:05Z", "null",
	}

	res := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		date := namedElement("date")
		date.Default = in
		dateMsgs, _ := errorMessages(date.IsValid())

		datetime := namedElement("datetime")
		datetime.Default = in
		datetimeMsgs, _ := errorMessages(datetime.IsValid())

		minmax := namedElement("datetime")
		minmax.DateTimeConfig = &model.DialogDateTimeConfig{MinDate: in}
		minmaxMsgs, _ := errorMessages(minmax.IsValid())

		res = append(res, map[string]any{
			"input":           in,
			"as_date_default": dateMsgs,
			"as_datetime":     datetimeMsgs,
			"as_min_date":     minmaxMsgs,
		})
	}
	return res
}

// dialogTimeIntervalAll is separate because the interval check has its own arithmetic.
func dialogTimeIntervalAll() []map[string]any {
	intervals := []int{0, 1, -1, 2, 3, 7, 15, 60, 90, 120, 480, 720, 1440, 1441, 10000}

	res := make([]map[string]any, 0, len(intervals))
	for _, iv := range intervals {
		e := namedElement("datetime")
		e.TimeInterval = iv
		msgs, _ := errorMessages(e.IsValid())
		res = append(res, map[string]any{"interval": iv, "messages": msgs})
	}
	return res
}

// --- Dialog.IsValid ---------------------------------------------------------------------------

func dialogIsValidAll() []dialogValidCase {
	longTitle := ""
	for range model.DialogTitleMaxLength + 1 {
		longTitle += "x"
	}

	cases := []struct {
		name string
		d    model.Dialog
	}{
		{"zero", model.Dialog{}},
		{"minimal_valid", model.Dialog{Title: "T"}},
		{"title_at_limit", model.Dialog{Title: longTitle[:model.DialogTitleMaxLength]}},
		{"title_over_limit", model.Dialog{Title: longTitle}},
		// The cap is bytes: 13 two-byte characters is 26 bytes.
		{"title_multibyte", model.Dialog{Title: "ééééééééééééé"}},
		{"icon_url_valid", model.Dialog{Title: "T", IconURL: "https://example.com/i.png"}},
		{"icon_url_invalid", model.Dialog{Title: "T", IconURL: "not a url"}},
		// The icon URL is plain IsValidHTTPURL — a plugin path is NOT accepted here, unlike
		// the element URLs, which use IsValidLookupURL.
		{"icon_url_plugin_path", model.Dialog{Title: "T", IconURL: "/plugins/x/i.png"}},
		{"empty_elements_slice", model.Dialog{Title: "T", Elements: []model.DialogElement{}}},
		{"one_valid_element", model.Dialog{Title: "T", Elements: []model.DialogElement{
			namedElement("text"),
		}}},
		{"one_invalid_element", model.Dialog{Title: "T", Elements: []model.DialogElement{
			{DisplayName: "d", Name: "n", Type: "nope"},
		}}},
		// An element with several failures nests a whole multierror inside one parent message.
		{"element_with_two_failures", model.Dialog{Title: "T", Elements: []model.DialogElement{
			{Type: "nope"},
		}}},
		{"duplicate_names", model.Dialog{Title: "T", Elements: []model.DialogElement{
			namedElement("text"), namedElement("text"),
		}}},
		{"three_duplicates", model.Dialog{Title: "T", Elements: []model.DialogElement{
			namedElement("text"), namedElement("text"), namedElement("text"),
		}}},
		{"duplicate_empty_names", model.Dialog{Title: "T", Elements: []model.DialogElement{
			{DisplayName: "d", Type: "text"}, {DisplayName: "d", Type: "text"},
		}}},
		{"everything_wrong", model.Dialog{
			Title: longTitle, IconURL: "nope",
			Elements: []model.DialogElement{{Type: "bad"}, {Type: "bad"}},
		}},
	}

	res := make([]dialogValidCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.d)
		if err != nil {
			panic(err)
		}
		d := c.d
		msgs, joined := errorMessages(d.IsValid())
		res = append(res, dialogValidCase{
			Name: c.name, Input: blob, Messages: msgs, Error: joined,
		})
	}
	return res
}

// --- OpenDialogRequest.IsValid ----------------------------------------------------------------

func openDialogRequestIsValidAll() []dialogValidCase {
	cases := []struct {
		name string
		r    model.OpenDialogRequest
	}{
		{"zero", model.OpenDialogRequest{}},
		{"valid", model.OpenDialogRequest{
			URL: "https://example.com/d", TriggerId: "trig", Dialog: model.Dialog{Title: "T"},
		}},
		{"missing_url", model.OpenDialogRequest{TriggerId: "t", Dialog: model.Dialog{Title: "T"}}},
		{"missing_trigger", model.OpenDialogRequest{URL: "u", Dialog: model.Dialog{Title: "T"}}},
		// The URL is only tested for emptiness here — anything non-empty passes.
		{"nonsense_url_accepted", model.OpenDialogRequest{
			URL: "not a url", TriggerId: "t", Dialog: model.Dialog{Title: "T"},
		}},
		// The dialog's failures are appended flat, because Dialog.IsValid returns a
		// *multierror.Error and multierror.Append splices one in rather than nesting it.
		{"invalid_dialog", model.OpenDialogRequest{
			URL: "u", TriggerId: "t", Dialog: model.Dialog{IconURL: "nope"},
		}},
	}

	res := make([]dialogValidCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.r)
		if err != nil {
			panic(err)
		}
		r := c.r
		msgs, joined := errorMessages(r.IsValid())
		res = append(res, dialogValidCase{
			Name: c.name, Input: blob, Messages: msgs, Error: joined,
		})
	}
	return res
}

// --- SubmitDialogResponse.IsValid -------------------------------------------------------------

func submitDialogResponseIsValidAll() []dialogValidCase {
	cases := []struct {
		name string
		r    model.SubmitDialogResponse
	}{
		{"zero", model.SubmitDialogResponse{}},
		{"error_short_circuits", model.SubmitDialogResponse{
			Error: "boom", Type: "garbage", Form: &model.Dialog{},
		}},
		{"errors_map_short_circuits", model.SubmitDialogResponse{
			Errors: map[string]string{"f": "bad"}, Type: "garbage",
		}},
		{"empty_errors_map_does_not", model.SubmitDialogResponse{
			Errors: map[string]string{}, Type: "garbage",
		}},
		{"type_ok", model.SubmitDialogResponse{Type: "ok"}},
		{"type_navigate", model.SubmitDialogResponse{Type: "navigate"}},
		{"type_ok_with_form", model.SubmitDialogResponse{Type: "ok", Form: &model.Dialog{Title: "T"}}},
		{"type_empty_with_form", model.SubmitDialogResponse{Form: &model.Dialog{Title: "T"}}},
		{"type_form_without_form", model.SubmitDialogResponse{Type: "form"}},
		{"type_form_with_valid_form", model.SubmitDialogResponse{
			Type: "form", Form: &model.Dialog{Title: "T"},
		}},
		{"type_form_with_invalid_form", model.SubmitDialogResponse{
			Type: "form", Form: &model.Dialog{},
		}},
		{"type_unknown", model.SubmitDialogResponse{Type: "nope"}},
		{"type_uppercase", model.SubmitDialogResponse{Type: "OK"}},
	}

	res := make([]dialogValidCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.r)
		if err != nil {
			panic(err)
		}
		r := c.r
		msgs, joined := errorMessages(r.IsValid())
		res = append(res, dialogValidCase{
			Name: c.name, Input: blob, Messages: msgs, Error: joined,
		})
	}
	return res
}

// --- wire -------------------------------------------------------------------------------------

type dialogWireCase struct {
	Name string `json:"name"`
	JSON string `json:"json"`
}

func dialogWireAll() []dialogWireCase {
	cases := []struct {
		name string
		v    any
	}{
		// Dialog: only source_url carries omitempty, so every other key is always present and
		// a nil Elements slice is `null`.
		{"dialog_zero", &model.Dialog{}},
		{"dialog_empty_elements", &model.Dialog{Elements: []model.DialogElement{}}},
		{"dialog_full", &model.Dialog{
			CallbackId: "cb", Title: "T", IntroductionText: "intro",
			IconURL: "https://example.com/i.png", SubmitLabel: "Go", NotifyOnCancel: true,
			State: "st", SourceURL: "https://example.com/s",
			Elements: []model.DialogElement{{DisplayName: "d", Name: "n", Type: "text"}},
		}},

		// DialogElement: eight keys are always present and eight carry omitempty.
		{"element_zero", &model.DialogElement{}},
		{"element_full", &model.DialogElement{
			DisplayName: "d", Name: "n", Type: "select", SubType: "s", Default: "1",
			Placeholder: "p", HelpText: "h", Optional: true, MinLength: 1, MaxLength: 9,
			DataSource: "dynamic", DataSourceURL: "https://example.com/l",
			Options:     []*model.PostActionOptions{{Text: "t", Value: "1"}},
			MultiSelect: true, AllowMultiple: true, Refresh: true,
			DateTimeConfig: &model.DialogDateTimeConfig{
				MinDate: "2020-01-01", MaxDate: "2020-12-31", TimeInterval: 30,
				LocationTimezone: "Asia/Tokyo", ManualTimeEntry: true, AllowManualTimeEntry: true,
			},
			MinDate: "2019-01-01", MaxDate: "2019-12-31", TimeInterval: 15,
			ActionButton: &model.DialogActionButton{
				URL: "/plugins/x/do", Context: map[string]string{"k": "v"},
			},
		}},
		{"element_empty_options_slice", &model.DialogElement{Options: []*model.PostActionOptions{}}},

		{"datetime_config_zero", &model.DialogDateTimeConfig{}},
		{"datetime_config_full", &model.DialogDateTimeConfig{
			MinDate: "2020-01-01", MaxDate: "2020-12-31", TimeInterval: 30,
			LocationTimezone: "Asia/Tokyo", ManualTimeEntry: true, AllowManualTimeEntry: true,
		}},

		{"action_button_zero", &model.DialogActionButton{}},
		{"action_button_empty_context", &model.DialogActionButton{
			URL: "u", Context: map[string]string{},
		}},
		{"action_button_full", &model.DialogActionButton{
			URL: "u", Context: map[string]string{"b": "2", "a": "1"},
		}},

		{"open_dialog_request_zero", &model.OpenDialogRequest{}},
		{"open_dialog_request_full", &model.OpenDialogRequest{
			TriggerId: "tr", URL: "https://example.com/d",
			Dialog: model.Dialog{Title: "T"},
		}},

		{"submit_request_zero", &model.SubmitDialogRequest{}},
		{"submit_request_full", &model.SubmitDialogRequest{
			Type: "dialog_submission", URL: "https://example.com/s", CallbackId: "cb",
			State: "st", UserId: idA, ChannelId: idB, TeamId: idC,
			Submission: map[string]any{"b": 2, "a": "1"},
			Cancelled:  true, FileIds: []string{idA},
		}},
		{"submit_request_empty_file_ids", &model.SubmitDialogRequest{FileIds: []string{}}},

		{"submit_response_zero", &model.SubmitDialogResponse{}},
		{"submit_response_full", &model.SubmitDialogResponse{
			Error: "e", Errors: map[string]string{"f": "bad"}, Type: "form",
			Form: &model.Dialog{Title: "T"},
		}},
		{"submit_response_empty_errors_map", &model.SubmitDialogResponse{
			Errors: map[string]string{},
		}},

		{"execute_dialog_action_request_zero", &model.ExecuteDialogActionRequest{}},
		{"execute_dialog_action_request_full", &model.ExecuteDialogActionRequest{
			URL: "u", Context: map[string]string{"k": "v"}, ChannelId: idA, TeamId: idB,
		}},

		{"dialog_select_option_zero", &model.DialogSelectOption{}},
		{"dialog_select_option_full", &model.DialogSelectOption{Text: "t", Value: "v"}},

		{"lookup_dialog_response_zero", &model.LookupDialogResponse{}},
		{"lookup_dialog_response_empty", &model.LookupDialogResponse{
			Items: []model.DialogSelectOption{},
		}},
		{"lookup_dialog_response_full", &model.LookupDialogResponse{
			Items: []model.DialogSelectOption{{Text: "t", Value: "v"}},
		}},
	}

	res := make([]dialogWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(c.v)
		if err != nil {
			panic(err)
		}
		res = append(res, dialogWireCase{Name: c.name, JSON: string(blob)})
	}
	return res
}
