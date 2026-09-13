package main

// Behavioural oracle for model/group.go and model/group_member.go, written to
// fixtures/behaviour_group.json.
//
// The seven group CRUD routes in api4/group.go all run one of these validators behind their
// licence gate, and every one of them is a chain of refusals whose *order* is the part a reader
// gets wrong. Driven from Go rather than read out of it.
//
//   - `IsValidName` — the reserved-name branch is `NewAppError("IsValidName", …)`, **without**
//     the `Group.` prefix its two neighbours carry. That is a typo upstream, not a pattern, and a
//     port that tidied it would drift.
//   - `IsValidForCreate` — name, then display name, then description, then source, then remote
//     id. The remote-id branch is one `||` with two halves: missing-and-required, or too long. So
//     a `custom` group may have *no* remote id and still be refused for an over-long one.
//   - `IsValidForUpdate` — id, create_at, update_at, then the whole of `IsValidForCreate`. Its id
//     branch is the only error in the file whose namespace is `app.` and not `model.`.
//   - `Patch` — four independent `!= nil` guards.
//   - `IsSyncable` / `requiresRemoteId` — the same predicate spelled twice in Go: `ldap`, or a
//     source *prefixed* `plugin_`. `custom` is neither.
//   - `GroupMember.IsValid` — two id checks, group before user.
//
// # The lengths are bytes, and the length check runs before the charset check
//
// `len(*group.Name)` is a byte count, and `validGroupnameChars` is `^[a-z0-9\.\-_]+$` — which no
// multi-byte character can satisfy. So a name of 33 two-byte characters is refused for its
// *length* (66 bytes) while the same character repeated ten times is refused for its *charset*.
// One input distinguishes both the byte-vs-rune reading and the order of the two checks, and it
// is in the corpus twice for exactly that reason.
//
// # `GroupSourceMaxLength` is declared and never read
//
// group.go:19 defines it; nothing in the file uses it. The source check is membership in
// {ldap, custom} or the `plugin_` prefix, with no cap at all — recorded here by a 200-character
// source that is refused for not being a known source rather than for its length.
//
// # What this oracle cannot see
//
// `AppError.params` is **unexported** in Go (utils.go:240), so the `GroupNameMaxLength`,
// `GroupDisplayNameMaxLength` and `GroupDescriptionMaxLength` interpolation params cannot be read
// from outside the model package. They are transcribed from the source in the Rust port and are
// *not* pinned by this corpus. They never reach the wire — `Message` is the untranslated id —
// so the exposure is limited to a future i18n bundle.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

const (
	grpID       = "wc4tnzbdb3bsfxu8srzjhqb6ur"
	grpRemoteID = "t9k5cjnq6bdatxg8hiu3mwqe1o"
	grpUserID   = "y3zh8mcpr7f3bemk9w4qsn5xdo"
)

func writeGroupBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_valid_name":       grpIsValidNameAll(),
		"is_valid_for_create": grpIsValidForCreateAll(),
		"is_valid_for_update": grpIsValidForUpdateAll(),
		"patch":               grpPatchAll(),
		"is_syncable":         grpIsSyncableAll(),
		"member_is_valid":     grpMemberIsValidAll(),
		"syncable_sources":    grpSyncableSources(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_group.json"), append(blob, '\n'), 0o644)
}

// grpValid is the row every validation case starts from: a custom group that passes all three
// validators. Custom rather than ldap so that `remote_id` is optional and each case can turn on
// exactly one thing.
func grpValid() model.Group {
	name := "engineering.team-1_a"
	return model.Group{
		Id:             grpID,
		Name:           &name,
		DisplayName:    "Engineering Team",
		Description:    "the people who build it",
		Source:         model.GroupSourceCustom,
		RemoteId:       nil,
		CreateAt:       1701305143000,
		UpdateAt:       1704208730000,
		DeleteAt:       0,
		AllowReference: true,
	}
}

// grpRecord runs one validator over one group and records Go's answer.
func grpRecord(name string, g *model.Group, f func(*model.Group) *model.AppError) map[string]any {
	row := map[string]any{"name": name, "in": mustMarshal(g)}
	probe(row, func() {
		appErr := f(g)
		row["ok"] = appErr == nil
		if appErr != nil {
			row["id"] = appErr.Id
			row["status_code"] = appErr.StatusCode
			row["where"] = appErr.Where
			row["detailed_error"] = appErr.DetailedError
		} else {
			row["id"] = nil
			row["status_code"] = nil
			row["where"] = nil
			row["detailed_error"] = nil
		}
	})
	return row
}

func strptr(s string) *string { return &s }

// --- IsValidName ---------------------------------------------------------------------------

func grpIsValidNameAll() []map[string]any {
	corpus := []struct {
		name string
		with func(*model.Group)
	}{
		{"valid_name", func(g *model.Group) {}},

		// A nil name is legal *only* while the group cannot be mentioned. This is the pair the
		// whole `*string` modelling exists for.
		{"nil_name_without_allow_reference", func(g *model.Group) {
			g.Name = nil
			g.AllowReference = false
		}},
		{"nil_name_with_allow_reference", func(g *model.Group) {
			g.Name = nil
			g.AllowReference = true
		}},

		// An *empty* name is not a nil name: it takes the length branch, not the nil branch, and
		// does so whether or not the group is referenceable.
		{"empty_name_with_allow_reference", func(g *model.Group) { g.Name = strptr("") }},
		{"empty_name_without_allow_reference", func(g *model.Group) {
			g.Name = strptr("")
			g.AllowReference = false
		}},

		// The byte cap, at both boundaries.
		{"name_64_ascii", func(g *model.Group) { g.Name = strptr(strings.Repeat("a", 64)) }},
		{"name_65_ascii", func(g *model.Group) { g.Name = strptr(strings.Repeat("a", 65)) }},

		// 33 two-byte characters is 66 bytes: refused for LENGTH, not charset, because the length
		// check runs first. A port counting runes accepts this and then reports the charset error.
		{"name_33_two_byte_chars", func(g *model.Group) { g.Name = strptr(strings.Repeat("é", 33)) }},
		// The same character ten times is 20 bytes: under the cap, so it reaches the charset
		// check and is refused there. The pair pins the order and the unit together.
		{"name_10_two_byte_chars", func(g *model.Group) { g.Name = strptr(strings.Repeat("é", 10)) }},

		// The three reserved mention handles. Note the `where` on these.
		{"reserved_all", func(g *model.Group) { g.Name = strptr("all") }},
		{"reserved_channel", func(g *model.Group) { g.Name = strptr("channel") }},
		{"reserved_here", func(g *model.Group) { g.Name = strptr("here") }},
		// Reserved names are matched exactly, not case-insensitively — but "All" then fails the
		// charset check instead, so the two branches are distinguishable only by their id.
		{"reserved_all_uppercase", func(g *model.Group) { g.Name = strptr("All") }},
		{"reserved_all_with_suffix", func(g *model.Group) { g.Name = strptr("all-hands") }},

		// The charset: lower-case, digits, dot, hyphen, underscore. Nothing else.
		{"name_uppercase", func(g *model.Group) { g.Name = strptr("Engineering") }},
		{"name_with_space", func(g *model.Group) { g.Name = strptr("eng team") }},
		{"name_with_at", func(g *model.Group) { g.Name = strptr("@eng") }},
		{"name_all_punctuation", func(g *model.Group) { g.Name = strptr(".-_") }},
		{"name_digits_only", func(g *model.Group) { g.Name = strptr("12345") }},
		{"name_with_slash", func(g *model.Group) { g.Name = strptr("eng/team") }},
		{"name_with_newline", func(g *model.Group) { g.Name = strptr("eng\nteam") }},
		{"name_with_plus", func(g *model.Group) { g.Name = strptr("eng+team") }},

		// A 65-byte name that is *also* reserved-shaped and charset-invalid: length wins.
		{"name_65_uppercase", func(g *model.Group) { g.Name = strptr(strings.Repeat("A", 65)) }},
	}

	var res []map[string]any
	for _, c := range corpus {
		g := grpValid()
		c.with(&g)
		res = append(res, grpRecord(c.name, &g, func(g *model.Group) *model.AppError {
			return g.IsValidName()
		}))
	}
	return res
}

// --- IsValidForCreate ----------------------------------------------------------------------

func grpIsValidForCreateAll() []map[string]any {
	corpus := []struct {
		name string
		with func(*model.Group)
	}{
		{"valid_custom", func(g *model.Group) {}},
		{"valid_ldap_with_remote_id", func(g *model.Group) {
			g.Source = model.GroupSourceLdap
			g.RemoteId = strptr(grpRemoteID)
		}},
		{"valid_plugin_with_remote_id", func(g *model.Group) {
			g.Source = model.GroupSource("plugin_jira")
			g.RemoteId = strptr(grpRemoteID)
		}},
		{"valid_custom_with_remote_id", func(g *model.Group) { g.RemoteId = strptr(grpRemoteID) }},
		{"valid_custom_nil_name_no_reference", func(g *model.Group) {
			g.Name = nil
			g.AllowReference = false
		}},

		// The name error propagates out of IsValidForCreate unchanged — same id, same `where`,
		// which is `Group.IsValidName` and not `Group.IsValidForCreate`.
		{"bad_name_charset", func(g *model.Group) { g.Name = strptr("Engineering") }},
		{"bad_name_reserved", func(g *model.Group) { g.Name = strptr("here") }},

		// Display name: 0 or over 128.
		{"empty_display_name", func(g *model.Group) { g.DisplayName = "" }},
		{"display_name_128", func(g *model.Group) { g.DisplayName = strings.Repeat("d", 128) }},
		{"display_name_129", func(g *model.Group) { g.DisplayName = strings.Repeat("d", 129) }},
		// Bytes again: 65 two-byte characters is 130 bytes and is refused, though it is 65 runes.
		{"display_name_65_two_byte", func(g *model.Group) { g.DisplayName = strings.Repeat("é", 65) }},

		// Description: only an upper bound, so empty is fine.
		{"empty_description", func(g *model.Group) { g.Description = "" }},
		{"description_1024", func(g *model.Group) { g.Description = strings.Repeat("x", 1024) }},
		{"description_1025", func(g *model.Group) { g.Description = strings.Repeat("x", 1025) }},

		// Source: two literals plus a prefix, and nothing else.
		{"empty_source", func(g *model.Group) { g.Source = "" }},
		{"unknown_source", func(g *model.Group) { g.Source = model.GroupSource("saml") }},
		{"source_uppercase_ldap", func(g *model.Group) { g.Source = model.GroupSource("LDAP") }},
		// `plugin` without the underscore is not the prefix.
		{"source_plugin_no_underscore", func(g *model.Group) { g.Source = model.GroupSource("plugin") }},
		// The bare prefix *is* a prefix of itself, so it is a valid source — and a syncable one,
		// so it then demands a remote id.
		{"source_bare_plugin_prefix", func(g *model.Group) { g.Source = model.GroupSource("plugin_") }},
		{"source_bare_plugin_prefix_with_remote_id", func(g *model.Group) {
			g.Source = model.GroupSource("plugin_")
			g.RemoteId = strptr(grpRemoteID)
		}},
		// GroupSourceMaxLength is declared and never enforced: this 200-character source is
		// refused for being unknown, not for being long.
		{"source_200_chars", func(g *model.Group) { g.Source = model.GroupSource(strings.Repeat("s", 200)) }},
		// ...and a 200-character *plugin* source is accepted, which is the same fact stated so
		// that a port adding the cap fails here.
		{"source_200_char_plugin", func(g *model.Group) {
			g.Source = model.GroupSource("plugin_" + strings.Repeat("s", 200))
			g.RemoteId = strptr(grpRemoteID)
		}},

		// Remote id: required for the syncable sources, capped for all of them.
		{"ldap_without_remote_id", func(g *model.Group) { g.Source = model.GroupSourceLdap }},
		{"ldap_with_empty_remote_id", func(g *model.Group) {
			g.Source = model.GroupSourceLdap
			g.RemoteId = strptr("")
		}},
		{"plugin_without_remote_id", func(g *model.Group) { g.Source = model.GroupSource("plugin_jira") }},
		{"custom_without_remote_id", func(g *model.Group) { g.RemoteId = nil }},
		{"remote_id_48", func(g *model.Group) { g.RemoteId = strptr(strings.Repeat("r", 48)) }},
		// The over-long half of the `||` applies to `custom` too, which has no *requirement*.
		{"custom_remote_id_49", func(g *model.Group) { g.RemoteId = strptr(strings.Repeat("r", 49)) }},
		{"ldap_remote_id_49", func(g *model.Group) {
			g.Source = model.GroupSourceLdap
			g.RemoteId = strptr(strings.Repeat("r", 49))
		}},

		// --- order pairs: each violates two rules, and Go reports the earlier one ---
		{"bad_name_and_empty_display_name", func(g *model.Group) {
			g.Name = strptr("Bad")
			g.DisplayName = ""
		}},
		{"empty_display_name_and_long_description", func(g *model.Group) {
			g.DisplayName = ""
			g.Description = strings.Repeat("x", 1025)
		}},
		{"long_description_and_bad_source", func(g *model.Group) {
			g.Description = strings.Repeat("x", 1025)
			g.Source = model.GroupSource("saml")
		}},
		{"bad_source_and_missing_remote_id", func(g *model.Group) {
			g.Source = model.GroupSource("saml")
			g.RemoteId = nil
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		g := grpValid()
		c.with(&g)
		res = append(res, grpRecord(c.name, &g, func(g *model.Group) *model.AppError {
			return g.IsValidForCreate()
		}))
	}
	return res
}

// --- IsValidForUpdate ----------------------------------------------------------------------

func grpIsValidForUpdateAll() []map[string]any {
	corpus := []struct {
		name string
		with func(*model.Group)
	}{
		{"valid_update", func(g *model.Group) {}},
		{"empty_id", func(g *model.Group) { g.Id = "" }},
		{"short_id", func(g *model.Group) { g.Id = "abc" }},
		{"id_27_chars", func(g *model.Group) { g.Id = grpID + "z" }},
		{"zero_create_at", func(g *model.Group) { g.CreateAt = 0 }},
		{"zero_update_at", func(g *model.Group) { g.UpdateAt = 0 }},
		// Negative is not zero, so it passes: the guard is `== 0`, not `<= 0`.
		{"negative_create_at", func(g *model.Group) { g.CreateAt = -1 }},
		{"negative_update_at", func(g *model.Group) { g.UpdateAt = -1 }},
		// A delete_at is irrelevant here — a soft-deleted group still validates for update,
		// which is what makes `restoreGroup` possible.
		{"soft_deleted", func(g *model.Group) { g.DeleteAt = 1706835475000 }},

		// The three gates run before IsValidForCreate, so a group that fails both reports the
		// update-specific error.
		{"bad_id_and_bad_name", func(g *model.Group) {
			g.Id = "abc"
			g.Name = strptr("Bad")
		}},
		{"zero_create_at_and_bad_name", func(g *model.Group) {
			g.CreateAt = 0
			g.Name = strptr("Bad")
		}},
		{"zero_update_at_and_empty_display_name", func(g *model.Group) {
			g.UpdateAt = 0
			g.DisplayName = ""
		}},
		// ...and id precedes create_at precedes update_at.
		{"empty_id_and_zero_create_at", func(g *model.Group) {
			g.Id = ""
			g.CreateAt = 0
		}},
		{"zero_create_at_and_zero_update_at", func(g *model.Group) {
			g.CreateAt = 0
			g.UpdateAt = 0
		}},

		// Falling through to IsValidForCreate: the error keeps that function's `where`.
		{"valid_gates_bad_name", func(g *model.Group) { g.Name = strptr("Bad") }},
		{"valid_gates_empty_display_name", func(g *model.Group) { g.DisplayName = "" }},
		{"valid_gates_ldap_without_remote_id", func(g *model.Group) { g.Source = model.GroupSourceLdap }},
	}

	var res []map[string]any
	for _, c := range corpus {
		g := grpValid()
		c.with(&g)
		res = append(res, grpRecord(c.name, &g, func(g *model.Group) *model.AppError {
			return g.IsValidForUpdate()
		}))
	}
	return res
}

// --- Patch ---------------------------------------------------------------------------------

func grpPatchAll() []map[string]any {
	boolptr := func(b bool) *bool { return &b }

	corpus := []struct {
		name  string
		patch model.GroupPatch
	}{
		{"empty_patch", model.GroupPatch{}},
		{"name_only", model.GroupPatch{Name: strptr("new.name")}},
		{"display_name_only", model.GroupPatch{DisplayName: strptr("New Display")}},
		{"description_only", model.GroupPatch{Description: strptr("new description")}},
		{"allow_reference_true", model.GroupPatch{AllowReference: boolptr(true)}},
		// The false case is the one a `if patch.AllowReference != nil && *patch.AllowReference`
		// misreading would drop.
		{"allow_reference_false", model.GroupPatch{AllowReference: boolptr(false)}},
		// An empty string is a *value*, not an absence: it overwrites.
		{"empty_name", model.GroupPatch{Name: strptr("")}},
		{"empty_display_name", model.GroupPatch{DisplayName: strptr("")}},
		{"empty_description", model.GroupPatch{Description: strptr("")}},
		{"all_four", model.GroupPatch{
			Name:           strptr("all.four"),
			DisplayName:    strptr("All Four"),
			Description:    strptr("every field"),
			AllowReference: boolptr(false),
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		g := grpValid()
		row := map[string]any{
			"name":  c.name,
			"in":    mustMarshal(&g),
			"patch": mustMarshal(&c.patch),
		}
		probe(row, func() {
			g.Patch(&c.patch)
			row["out"] = mustMarshal(&g)
			// Recorded field by field as well, because `out` is a string and a diff in it is
			// unreadable; and because a nil name is *omitted* from `out` entirely.
			row["name_is_nil"] = g.Name == nil
			row["name_value"] = g.GetName()
			row["display_name"] = g.DisplayName
			row["description"] = g.Description
			row["allow_reference"] = g.AllowReference
		})
		res = append(res, row)
	}
	return res
}

// --- IsSyncable ----------------------------------------------------------------------------

func grpIsSyncableAll() []map[string]any {
	sources := []string{
		"ldap", "custom", "plugin_jira", "plugin_", "plugin", "Plugin_x", "LDAP", "",
		"ldap_extra", "xplugin_", " plugin_", "saml",
	}

	var res []map[string]any
	for _, s := range sources {
		g := grpValid()
		g.Source = model.GroupSource(s)
		row := map[string]any{"source": s}
		probe(row, func() {
			row["is_syncable"] = g.IsSyncable()
		})
		res = append(res, row)
	}
	return res
}

// --- GroupMember.IsValid -------------------------------------------------------------------

func grpMemberIsValidAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.GroupMember
	}{
		{"valid", model.GroupMember{GroupId: grpID, UserId: grpUserID, CreateAt: 1701440641000}},
		// Neither timestamp is checked, so a zero-timestamped membership is valid.
		{"zero_timestamps", model.GroupMember{GroupId: grpID, UserId: grpUserID}},
		{"empty_group_id", model.GroupMember{UserId: grpUserID}},
		{"short_group_id", model.GroupMember{GroupId: "abc", UserId: grpUserID}},
		{"empty_user_id", model.GroupMember{GroupId: grpID}},
		{"short_user_id", model.GroupMember{GroupId: grpID, UserId: "abc"}},
		// Group id is checked first.
		{"both_empty", model.GroupMember{}},
		{"deleted_membership", model.GroupMember{
			GroupId: grpID, UserId: grpUserID,
			CreateAt: 1701440641000, DeleteAt: 1705838686000,
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		gm := c.in
		row := map[string]any{"name": c.name, "in": mustMarshal(&gm)}
		probe(row, func() {
			appErr := gm.IsValid()
			row["ok"] = appErr == nil
			if appErr != nil {
				row["id"] = appErr.Id
				row["status_code"] = appErr.StatusCode
				row["where"] = appErr.Where
				row["detailed_error"] = appErr.DetailedError
			} else {
				row["id"] = nil
				row["status_code"] = nil
				row["where"] = nil
				row["detailed_error"] = nil
			}
		})
		res = append(res, row)
	}
	return res
}

// --- the two source lists ------------------------------------------------------------------

func grpSyncableSources() map[string]any {
	toStrings := func(in []model.GroupSource) []string {
		out := make([]string, 0, len(in))
		for _, s := range in {
			out = append(out, string(s))
		}
		return out
	}
	return map[string]any{
		"sources":  toStrings(model.GetSyncableGroupSources()),
		"prefixes": toStrings(model.GetSyncableGroupSourcePrefixes()),
	}
}
