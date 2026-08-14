package main

// Behavioural oracle for model/channel_member.go, written to
// fixtures/behaviour_channel_member.json.
//
// Two things here cannot be reasoned about safely and must come from Go:
//
//  1. IsChannelMemberNotifyPropsValid has an "ok || !allowMissingFields" shape on its first two
//     checks and a plain "ok" on the other four, so whether a *missing* key is an error depends
//     on which key it is and on the flag. The matrix below enumerates it.
//
//  2. The final size check runs on `ToJSON(notifyProps)`, i.e. encoding/json's output for a
//     map[string]string. Go sorts the keys, HTML-escapes < > &, escapes U+2028/U+2029, and
//     writes  /  where serde_json writes \b / \f. Every one of those changes the
//     rune count the check compares against, so the exact bytes are recorded, not just a length.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"unicode/utf8"

	"github.com/mattermost/mattermost/server/public/model"
)

type notifyPropsCase struct {
	Name               string            `json:"name"`
	Props              map[string]string `json:"props"`
	AllowMissingFields bool              `json:"allow_missing_fields"`
	ErrorID            string            `json:"error_id"`
	Detailed           string            `json:"detailed"`
}

type toJSONCase struct {
	Name      string            `json:"name"`
	Props     map[string]string `json:"props"`
	Encoded   string            `json:"encoded"`
	Bytes     int               `json:"bytes"`
	RuneCount int               `json:"rune_count"`
}

type memberValidCase struct {
	Name     string          `json:"name"`
	Member   json.RawMessage `json:"member"`
	ErrorID  string          `json:"error_id"`
	Detailed string          `json:"detailed"`
}

func writeChannelMemberBehaviourFixture(outDir string) error {
	out := map[string]any{
		"notify_props_valid":            notifyPropsAll(),
		"to_json_string_map":            toJSONAll(),
		"channel_member_is_valid":       memberIsValidAll(),
		"is_channel_notify_level":       levelAll(model.IsChannelNotifyLevelValid),
		"is_channel_mark_unread_level":  levelAll(model.IsChannelMarkUnreadLevelValid),
		"is_send_email":                 levelAll(model.IsSendEmailValid),
		"is_ignore_channel_mentions":    levelAll(model.IsIgnoreChannelMentionsValid),
		"is_channel_auto_follow_thread": levelAll(model.IsChannelAutoFollowThreadsValid),
		"default_channel_notify_props":  model.GetDefaultChannelNotifyProps(),
		"set_channel_muted":             setMutedAll(),
		"sanitize_for_current_user":     sanitizeForCurrentUserAll(),
		"member_get_roles":              memberRolesAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_member.json"), append(blob, '\n'), 0o644)
}

// --- the five level validators -------------------------------------------------

var levelCorpus = []string{
	"", "default", "all", "mention", "none", "on", "off", "true", "false",
	"Default", "ALL", "None", "ON", "OFF", "True", "0", "1", " all", "all ",
	"auto", "unknown", "mentions",
}

func levelAll(fn func(string) bool) map[string]bool {
	res := make(map[string]bool, len(levelCorpus))
	for _, in := range levelCorpus {
		res[in] = fn(in)
	}
	return res
}

// --- IsChannelMemberNotifyPropsValid -------------------------------------------

func validProps() map[string]string {
	// Start from Go's own defaults so the "valid" baseline cannot drift from them.
	base := map[string]string{}
	for k, v := range model.GetDefaultChannelNotifyProps() {
		base[k] = v
	}
	return base
}

func withProp(mutate func(p map[string]string)) map[string]string {
	p := validProps()
	mutate(p)
	return p
}

func notifyPropsAll() []notifyPropsCase {
	type mut struct {
		name  string
		props map[string]string
	}
	muts := []mut{
		{"defaults", validProps()},
		{"empty", map[string]string{}},
		{"nil", nil},
		{"only_desktop", map[string]string{model.DesktopNotifyProp: model.ChannelNotifyAll}},
		{"only_mark_unread", map[string]string{model.MarkUnreadNotifyProp: model.ChannelMarkUnreadAll}},
		{"desktop_and_mark_unread", map[string]string{
			model.DesktopNotifyProp:    model.ChannelNotifyAll,
			model.MarkUnreadNotifyProp: model.ChannelMarkUnreadAll,
		}},

		{"desktop_empty", withProp(func(p map[string]string) { p[model.DesktopNotifyProp] = "" })},
		{"desktop_bad", withProp(func(p map[string]string) { p[model.DesktopNotifyProp] = "nope" })},
		{"desktop_none", withProp(func(p map[string]string) { p[model.DesktopNotifyProp] = model.ChannelNotifyNone })},
		{"desktop_21_chars", withProp(func(p map[string]string) { p[model.DesktopNotifyProp] = repeat("a", 21) })},

		{"mark_unread_bad", withProp(func(p map[string]string) { p[model.MarkUnreadNotifyProp] = "none" })},
		{"mark_unread_mention", withProp(func(p map[string]string) { p[model.MarkUnreadNotifyProp] = model.ChannelMarkUnreadMention })},
		{"mark_unread_default_is_invalid", withProp(func(p map[string]string) { p[model.MarkUnreadNotifyProp] = model.ChannelNotifyDefault })},

		{"push_bad", withProp(func(p map[string]string) { p[model.PushNotifyProp] = "nope" })},
		{"push_none", withProp(func(p map[string]string) { p[model.PushNotifyProp] = model.ChannelNotifyNone })},
		{"push_missing", withProp(func(p map[string]string) { delete(p, model.PushNotifyProp) })},

		{"email_true", withProp(func(p map[string]string) { p[model.EmailNotifyProp] = "true" })},
		{"email_false", withProp(func(p map[string]string) { p[model.EmailNotifyProp] = "false" })},
		{"email_all_is_invalid", withProp(func(p map[string]string) { p[model.EmailNotifyProp] = model.ChannelNotifyAll })},
		{"email_missing", withProp(func(p map[string]string) { delete(p, model.EmailNotifyProp) })},

		{"ignore_mentions_on", withProp(func(p map[string]string) { p[model.IgnoreChannelMentionsNotifyProp] = model.IgnoreChannelMentionsOn })},
		{"ignore_mentions_bad", withProp(func(p map[string]string) { p[model.IgnoreChannelMentionsNotifyProp] = "yes" })},
		{"ignore_mentions_41_chars", withProp(func(p map[string]string) {
			p[model.IgnoreChannelMentionsNotifyProp] = repeat("a", 41)
		})},

		{"auto_follow_on", withProp(func(p map[string]string) { p[model.ChannelAutoFollowThreads] = model.ChannelAutoFollowThreadsOn })},
		{"auto_follow_bad", withProp(func(p map[string]string) { p[model.ChannelAutoFollowThreads] = "yes" })},
		{"auto_follow_default_is_invalid", withProp(func(p map[string]string) {
			p[model.ChannelAutoFollowThreads] = model.ChannelNotifyDefault
		})},

		{"unknown_key_is_ignored", withProp(func(p map[string]string) { p["totally_unknown"] = "whatever" })},

		// Ordering probe: two bad props at once.
		{"desktop_and_push_bad", withProp(func(p map[string]string) {
			p[model.DesktopNotifyProp] = "nope"
			p[model.PushNotifyProp] = "nope"
		})},
		{"push_and_email_bad", withProp(func(p map[string]string) {
			p[model.PushNotifyProp] = "nope"
			p[model.EmailNotifyProp] = "nope"
		})},
	}

	var res []notifyPropsCase
	for _, m := range muts {
		for _, allow := range []bool{false, true} {
			var id, detailed string
			if appErr := model.IsChannelMemberNotifyPropsValid(m.props, allow); appErr != nil {
				id = appErr.Id
				detailed = appErr.DetailedError
			}
			res = append(res, notifyPropsCase{
				Name:               m.name,
				Props:              m.props,
				AllowMissingFields: allow,
				ErrorID:            id,
				Detailed:           detailed,
			})
		}
	}
	return res
}

// --- ToJSON(map[string]string) -------------------------------------------------

// toJSONAll pins encoding/json's output for a string map: key ordering and every escape rule
// that differs from serde_json's. The rune count is what the 800,000-rune check compares.
func toJSONAll() []toJSONCase {
	cases := []struct {
		name  string
		props map[string]string
	}{
		{"nil", nil},
		{"empty", map[string]string{}},
		{"one", map[string]string{"a": "b"}},
		{"defaults", validProps()},
		// Key ordering: Go sorts by byte value, so uppercase sorts before lowercase.
		{"ordering", map[string]string{"b": "1", "a": "2", "C": "3", "_": "4", "0": "5", "é": "6"}},
		// HTML escaping is ON for json.Marshal.
		{"html_escapes", map[string]string{"<k>": "a&b", "&": "<", ">": "&"}},
		// Control characters: Go writes \u0008 and \u000c where serde_json writes \b and \f.
		{"control_chars", map[string]string{"a": "\x00\x01\x07\x08\x0c\n\r\t\x1f"}},
		{"quote_and_backslash", map[string]string{"q\"k": "v\\a"}},
		// Line/paragraph separators are escaped by Go even though they are valid JSON.
		{"line_separators", map[string]string{"a": "x\u2028y\u2029z"}},
		{"multibyte", map[string]string{"\u65e5\u672c": "\u2603\u00e9", "emoji": "\U0001f642"}},
		{"del_and_high", map[string]string{"a": "\x7f\u00a0\ufeff"}},
	}

	res := make([]toJSONCase, 0, len(cases))
	for _, c := range cases {
		encoded := string(model.ToJSON(c.props))
		res = append(res, toJSONCase{
			Name:      c.name,
			Props:     c.props,
			Encoded:   encoded,
			Bytes:     len(encoded),
			RuneCount: utf8.RuneCountInString(encoded),
		})
	}
	return res
}

// --- ChannelMember.IsValid -----------------------------------------------------

func baseMember() *model.ChannelMember {
	return &model.ChannelMember{
		ChannelId:    idA,
		UserId:       idB,
		Roles:        "channel_user",
		NotifyProps:  model.GetDefaultChannelNotifyProps(),
		LastUpdateAt: 1705492114000,
	}
}

func memberIsValidAll() []memberValidCase {
	type mut struct {
		name string
		fn   func(m *model.ChannelMember)
	}
	muts := []mut{
		{"valid", func(m *model.ChannelMember) {}},
		{"channel_id_empty", func(m *model.ChannelMember) { m.ChannelId = "" }},
		{"channel_id_short", func(m *model.ChannelMember) { m.ChannelId = repeat("a", 25) }},
		{"user_id_empty", func(m *model.ChannelMember) { m.UserId = "" }},
		{"notify_props_nil", func(m *model.ChannelMember) { m.NotifyProps = nil }},
		{"notify_props_empty", func(m *model.ChannelMember) { m.NotifyProps = model.StringMap{} }},
		{"notify_props_missing_desktop", func(m *model.ChannelMember) {
			delete(m.NotifyProps, model.DesktopNotifyProp)
		}},
		{"notify_props_bad_push", func(m *model.ChannelMember) {
			m.NotifyProps[model.PushNotifyProp] = "nope"
		}},
		{"roles_256", func(m *model.ChannelMember) { m.Roles = repeat("a", 256) }},
		{"roles_257", func(m *model.ChannelMember) { m.Roles = repeat("a", 257) }},
		{"roles_empty", func(m *model.ChannelMember) { m.Roles = "" }},
		// Ordering probe: bad id and bad props together.
		{"bad_user_id_and_bad_props", func(m *model.ChannelMember) {
			m.UserId = ""
			m.NotifyProps = model.StringMap{}
		}},
		{"bad_props_and_long_roles", func(m *model.ChannelMember) {
			m.NotifyProps = model.StringMap{}
			m.Roles = repeat("a", 257)
		}},
	}

	res := make([]memberValidCase, 0, len(muts))
	for _, mt := range muts {
		m := baseMember()
		mt.fn(m)
		blob, err := json.Marshal(m)
		if err != nil {
			panic(err)
		}
		var id, detailed string
		if appErr := m.IsValid(); appErr != nil {
			id = appErr.Id
			detailed = appErr.DetailedError
		}
		res = append(res, memberValidCase{Name: mt.name, Member: blob, ErrorID: id, Detailed: detailed})
	}
	return res
}

// --- SetChannelMuted / IsChannelMuted ------------------------------------------

// setMutedAll pins the fact that SetChannelMuted **ignores its argument** — it toggles.
// Calling SetChannelMuted(true) on an unmuted channel mutes it; calling SetChannelMuted(false)
// on an unmuted channel also mutes it.
func setMutedAll() []map[string]any {
	var res []map[string]any
	for _, start := range []string{model.ChannelMarkUnreadAll, model.ChannelMarkUnreadMention, "", "garbage"} {
		for _, arg := range []bool{true, false} {
			m := &model.ChannelMember{NotifyProps: model.StringMap{model.MarkUnreadNotifyProp: start}}
			before := m.IsChannelMuted()
			m.SetChannelMuted(arg)
			res = append(res, map[string]any{
				"start":        start,
				"arg":          arg,
				"muted_before": before,
				"mark_unread":  m.NotifyProps[model.MarkUnreadNotifyProp],
				"muted_after":  m.IsChannelMuted(),
			})
		}
	}
	// The prop missing entirely: IsChannelMuted reads the zero value of the map.
	m := &model.ChannelMember{NotifyProps: model.StringMap{}}
	before := m.IsChannelMuted()
	m.SetChannelMuted(true)
	res = append(res, map[string]any{
		"start":        "<absent>",
		"arg":          true,
		"muted_before": before,
		"mark_unread":  m.NotifyProps[model.MarkUnreadNotifyProp],
		"muted_after":  m.IsChannelMuted(),
	})
	return res
}

func sanitizeForCurrentUserAll() []map[string]any {
	var res []map[string]any
	for _, current := range []string{idB, idA, "", "stranger"} {
		m := baseMember()
		m.LastViewedAt = 1700557221000
		m.LastUpdateAt = 1707722148000
		m.SanitizeForCurrentUser(current)
		res = append(res, map[string]any{
			"member_user_id": m.UserId,
			"current_user":   current,
			"last_viewed_at": m.LastViewedAt,
			"last_update_at": m.LastUpdateAt,
		})
	}
	return res
}

func memberRolesAll() map[string][]string {
	corpus := []string{
		"", " ", "channel_user", "channel_user channel_admin",
		"channel_user  channel_admin", "channel_user\tchannel_admin",
		" channel_user ", "channel_user\nchannel_admin", "a b",
	}
	res := make(map[string][]string, len(corpus))
	for _, in := range corpus {
		m := &model.ChannelMember{Roles: in}
		roles := m.GetRoles()
		if roles == nil {
			roles = []string{}
		}
		res[in] = roles
	}
	return res
}
