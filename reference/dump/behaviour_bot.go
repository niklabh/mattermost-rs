package main

// Behavioural oracle for model/bot.go, written to fixtures/behaviour_bot.json.
//
// Sixteen functions over a nine-field struct, and two of them are wrong in ways a translator will
// silently "fix" unless the answers are recorded from Go itself.
//
// # The copy-paste bug in IsValidCreate
//
// The DisplayName length branch returns the **user_id** error id:
//
//	if utf8.RuneCountInString(b.DisplayName) > BotDisplayNameMaxRunes {
//	    return NewAppError("Bot.IsValid", "model.bot.is_valid.user_id.app_error", ...)
//	}
//
// There is no `model.bot.is_valid.display_name.app_error` anywhere in the tree. A port that
// writes the obvious id changes what a client sees for an over-long display name, and nothing in
// a happy-path test would catch it. The corpus records the id per input for exactly this reason.
//
// # BotList.Etag's third component is always zero
//
//	var t int64
//	var delta int64        // declared, never assigned
//	...
//	return Etag(id, t, delta, len(*l))
//
// `delta` is written nowhere in the function, so every bot-list etag carries a literal "0" in
// that position. It looks like a leftover from a version that computed something. Reproduced, and
// pinned here so a future reader who deletes the "unused" variable fails a test.
//
// # The empty-list case is not the zero value
//
// `id` starts at the string "0", not "", so an empty BotList etags as `<version>.0.0.0.0` rather
// than with an empty first component. Same shape of trap as audits.go's etag ([D-076]).
//
// Determinism: PreSave and PreUpdate call GetMillis, so those sections record **invariants and
// relationships** (equal / non-zero / cleared) rather than absolute instants — the pattern
// behaviour_custom_status.go established. Everything else is fixed values. See [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeBotBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":       botConstants(),
		"keys":            expectedKeys(reflect.TypeOf(model.Bot{})),
		"patch_keys":      expectedKeys(reflect.TypeOf(model.BotPatch{})),
		"wire":            botWireAll(),
		"is_valid":        botIsValidAll(),
		"is_valid_create": botIsValidCreateAll(),
		"pre_save":        botPreSaveAll(),
		"etag":            botEtagAll(),
		"list_etag":       botListEtagAll(),
		"patch":           botPatchAll(),
		"would_patch":     botWouldPatchAll(),
		"user_from_bot":   botUserFromBotAll(),
		"bot_from_user":   botBotFromUserAll(),
		"is_bot_dm":       botIsBotDMChannelAll(),
		"not_found_error": botNotFoundErrorCase(),
		"auditable":       botAuditableAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_bot.json"), append(blob, '\n'), 0o644)
}

func botConstants() map[string]any {
	return map[string]any{
		// Both of these are aliases of constants that live in other files. Recorded by value so
		// the Rust side pins the number rather than re-deriving the alias.
		"BotDisplayNameMaxRunes":   model.BotDisplayNameMaxRunes,
		"BotDescriptionMaxRunes":   model.BotDescriptionMaxRunes,
		"BotCreatorIdMaxRunes":     model.BotCreatorIdMaxRunes,
		"BotWarnMetricBotUsername": model.BotWarnMetricBotUsername,
		"BotSystemBotUsername":     model.BotSystemBotUsername,
		// The sources of the two aliases, so a drift in either is visible here.
		"UserFirstNameMaxRunes":    model.UserFirstNameMaxRunes,
		"KeyValuePluginIdMaxRunes": model.KeyValuePluginIdMaxRunes,
		"SystemUserRoleId":         model.SystemUserRoleId,
	}
}

// --- helpers ---------------------------------------------------------------------------------

// validBot is the baseline every IsValid case mutates from: everything set correctly.
func validBot() model.Bot {
	return model.Bot{
		UserId:         "y9i4er48tt8bukijy7i3u5y9ar",
		Username:       "botusername",
		DisplayName:    "Bot Display Name",
		Description:    "a bot that does things",
		OwnerId:        "aaaaaaaaaaaaaaaaaaaaaaaaaa",
		LastIconUpdate: 1700000000000,
		CreateAt:       1600000000000,
		UpdateAt:       1650000000000,
		DeleteAt:       0,
	}
}

func appErrOut(err *model.AppError) map[string]any {
	if err == nil {
		return map[string]any{"ok": true}
	}
	return map[string]any{
		"ok":     false,
		"id":     err.Id,
		"where":  err.Where,
		"status": err.StatusCode,
		// `params` is unexported and has no getter, so it cannot be recorded from outside the
		// package. Every one of these errors is constructed with `b.Trace()` as its params, and
		// `Trace()` is recorded separately under "auditable" — that is as close as an external
		// oracle can get. The params are `json:"-"` in any case, so they never reach a client.
	}
}

func runes(n int) string {
	out := make([]rune, n)
	for i := range out {
		out[i] = 'a'
	}
	return string(out)
}

// --- the wire format -------------------------------------------------------------------------

func botWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Bot
	}{
		// Four fields carry omitempty: display_name, description, last_icon_update — and the
		// zero value therefore emits only the five that do not.
		{"zero", model.Bot{}},
		{"full", validBot()},
		{"deleted", func() model.Bot { b := validBot(); b.DeleteAt = 1700000000001; return b }()},
		{"no_display_name", func() model.Bot { b := validBot(); b.DisplayName = ""; return b }()},
		{"no_description", func() model.Bot { b := validBot(); b.Description = ""; return b }()},
		{"no_icon", func() model.Bot { b := validBot(); b.LastIconUpdate = 0; return b }()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}
	return out
}

func botPatchWire() []map[string]any {
	s := func(v string) *string { return &v }
	corpus := []struct {
		name string
		in   model.BotPatch
	}{
		// BotPatch has NO omitempty on any field, so an all-nil patch is three explicit nulls
		// rather than `{}`. That is the difference between "leave alone" and "not mentioned",
		// and it is on the wire.
		{"all_nil", model.BotPatch{}},
		{"all_set", model.BotPatch{Username: s("u"), DisplayName: s("d"), Description: s("desc")}},
		{"empty_strings", model.BotPatch{Username: s(""), DisplayName: s(""), Description: s("")}},
		{"only_username", model.BotPatch{Username: s("u")}},
	}
	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}
	return out
}

// --- IsValid / IsValidCreate -----------------------------------------------------------------

func botIsValidAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Bot
	}{
		{"valid", validBot()},
		{"empty_user_id", func() model.Bot { b := validBot(); b.UserId = ""; return b }()},
		{"short_user_id", func() model.Bot { b := validBot(); b.UserId = "abc"; return b }()},
		{"zero_create_at", func() model.Bot { b := validBot(); b.CreateAt = 0; return b }()},
		{"zero_update_at", func() model.Bot { b := validBot(); b.UpdateAt = 0; return b }()},
		// IsValid delegates to IsValidCreate after its own three checks, so the create-side
		// failures surface through IsValid too — and the ORDER matters: a bot with both a zero
		// CreateAt and a bad username reports create_at, not username.
		{"bad_username_via_is_valid", func() model.Bot { b := validBot(); b.Username = "Bad Username!"; return b }()},
		{"zero_create_at_and_bad_username", func() model.Bot {
			b := validBot()
			b.CreateAt = 0
			b.Username = "Bad Username!"
			return b
		}()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		out = append(out, map[string]any{"name": c.name, "result": appErrOut(in.IsValid())})
	}
	return out
}

func botIsValidCreateAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Bot
	}{
		{"valid", validBot()},
		// IsValidCreate does NOT check UserId, CreateAt or UpdateAt — that is the whole point of
		// the "skips validations of fields that are auto-filled" comment. Recorded so a port does
		// not helpfully add them.
		{"empty_user_id_is_fine_on_create", func() model.Bot { b := validBot(); b.UserId = ""; return b }()},
		{"zero_timestamps_are_fine_on_create", func() model.Bot {
			b := validBot()
			b.CreateAt = 0
			b.UpdateAt = 0
			return b
		}()},
		{"empty_username", func() model.Bot { b := validBot(); b.Username = ""; return b }()},
		{"bad_username", func() model.Bot { b := validBot(); b.Username = "Has Spaces"; return b }()},
		// THE BUG: this returns model.bot.is_valid.user_id.app_error, not display_name.
		{"display_name_too_long", func() model.Bot {
			b := validBot()
			b.DisplayName = runes(model.BotDisplayNameMaxRunes + 1)
			return b
		}()},
		{"display_name_at_limit", func() model.Bot {
			b := validBot()
			b.DisplayName = runes(model.BotDisplayNameMaxRunes)
			return b
		}()},
		{"description_too_long", func() model.Bot {
			b := validBot()
			b.Description = runes(model.BotDescriptionMaxRunes + 1)
			return b
		}()},
		{"description_at_limit", func() model.Bot {
			b := validBot()
			b.Description = runes(model.BotDescriptionMaxRunes)
			return b
		}()},
		{"empty_owner_id", func() model.Bot { b := validBot(); b.OwnerId = ""; return b }()},
		{"owner_id_too_long", func() model.Bot {
			b := validBot()
			b.OwnerId = runes(model.BotCreatorIdMaxRunes + 1)
			return b
		}()},
		{"owner_id_at_limit", func() model.Bot {
			b := validBot()
			b.OwnerId = runes(model.BotCreatorIdMaxRunes)
			return b
		}()},
		// Multi-byte counting: RuneCountInString, not len(). A display name of N multi-byte
		// characters is N runes and 2N bytes, so it must pass at the limit.
		{"display_name_at_limit_multibyte", func() model.Bot {
			b := validBot()
			b.DisplayName = string([]rune(repeatRune('é', model.BotDisplayNameMaxRunes)))
			return b
		}()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		out = append(out, map[string]any{"name": c.name, "result": appErrOut(in.IsValidCreate())})
	}
	return out
}

func repeatRune(r rune, n int) []rune {
	out := make([]rune, n)
	for i := range out {
		out[i] = r
	}
	return out
}

// --- PreSave / PreUpdate ---------------------------------------------------------------------

func botPreSaveAll() []map[string]any {
	// Clock-dependent, so relationships rather than instants.
	b := validBot()
	b.Username = "  MixedCase  "
	b.DeleteAt = 12345
	before := b
	b.PreSave()

	preUpdate := validBot()
	preUpdateBefore := preUpdate
	preUpdate.PreUpdate()

	return []map[string]any{
		{
			"name":                       "pre_save",
			"create_at_equals_update_at": b.CreateAt == b.UpdateAt,
			"create_at_nonzero":          b.CreateAt != 0,
			"delete_at_cleared":          b.DeleteAt == 0,
			"delete_at_before":           before.DeleteAt,
			"username_normalized":        b.Username,
			"username_before":            before.Username,
			// PreSave does NOT touch these.
			"user_id_unchanged":      b.UserId == before.UserId,
			"owner_id_unchanged":     b.OwnerId == before.OwnerId,
			"description_unchanged":  b.Description == before.Description,
			"display_name_unchanged": b.DisplayName == before.DisplayName,
		},
		{
			"name":                "pre_update",
			"update_at_changed":   preUpdate.UpdateAt != preUpdateBefore.UpdateAt,
			"update_at_nonzero":   preUpdate.UpdateAt != 0,
			"create_at_unchanged": preUpdate.CreateAt == preUpdateBefore.CreateAt,
			"delete_at_unchanged": preUpdate.DeleteAt == preUpdateBefore.DeleteAt,
			"username_unchanged":  preUpdate.Username == preUpdateBefore.Username,
		},
	}
}

// --- etags -----------------------------------------------------------------------------------

func botEtagAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Bot
	}{
		{"typical", validBot()},
		{"zero", model.Bot{}},
		{"zero_update_at", func() model.Bot { b := validBot(); b.UpdateAt = 0; return b }()},
	}
	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		out = append(out, map[string]any{"name": c.name, "etag": in.Etag()})
	}
	return out
}

func botListEtagAll() []map[string]any {
	bot := func(id string, updateAt int64) *model.Bot {
		b := validBot()
		b.UserId = id
		b.UpdateAt = updateAt
		return &b
	}

	corpus := []struct {
		name string
		in   model.BotList
	}{
		// id starts at the STRING "0", so this is not an empty first component.
		{"empty", model.BotList{}},
		{"one", model.BotList{bot("y9i4er48tt8bukijy7i3u5y9ar", 100)}},
		// The max wins regardless of position, and it decides BOTH components.
		{"ascending", model.BotList{
			bot("aaaaaaaaaaaaaaaaaaaaaaaaaa", 100),
			bot("bbbbbbbbbbbbbbbbbbbbbbbbbb", 200),
		}},
		{"descending", model.BotList{
			bot("bbbbbbbbbbbbbbbbbbbbbbbbbb", 200),
			bot("aaaaaaaaaaaaaaaaaaaaaaaaaa", 100),
		}},
		// Strictly-greater comparison: a tie keeps the FIRST id seen.
		{"tie", model.BotList{
			bot("aaaaaaaaaaaaaaaaaaaaaaaaaa", 200),
			bot("bbbbbbbbbbbbbbbbbbbbbbbbbb", 200),
		}},
		// All zero UpdateAt: nothing beats the initial t=0, so id stays the literal "0".
		{"all_zero_update_at", model.BotList{
			bot("aaaaaaaaaaaaaaaaaaaaaaaaaa", 0),
			bot("bbbbbbbbbbbbbbbbbbbbbbbbbb", 0),
		}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		out = append(out, map[string]any{"name": c.name, "len": len(in), "etag": in.Etag()})
	}
	return out
}

// --- Patch / WouldPatch ----------------------------------------------------------------------

func botPatchCorpus() []struct {
	name  string
	patch *model.BotPatch
} {
	s := func(v string) *string { return &v }
	return []struct {
		name  string
		patch *model.BotPatch
	}{
		{"nil_patch", nil},
		{"empty_patch", &model.BotPatch{}},
		{"username_only", &model.BotPatch{Username: s("newname")}},
		{"display_name_only", &model.BotPatch{DisplayName: s("New Display")}},
		{"description_only", &model.BotPatch{Description: s("new description")}},
		{"all_three", &model.BotPatch{Username: s("n"), DisplayName: s("d"), Description: s("x")}},
		// A pointer to the value it already has: Patch applies it (no-op), WouldPatch says false.
		{"same_username", &model.BotPatch{Username: s("botusername")}},
		// A pointer to empty is NOT nil — it clears the field.
		{"clear_display_name", &model.BotPatch{DisplayName: s("")}},
	}
}

func botPatchAll() []map[string]any {
	out := make([]map[string]any, 0)
	for _, c := range botPatchCorpus() {
		if c.patch == nil {
			// Patch dereferences the patch unconditionally, so a nil patch panics. Recorded as
			// such rather than skipped.
			out = append(out, map[string]any{"name": c.name, "panics": true})
			continue
		}
		b := validBot()
		b.Patch(c.patch)
		blob, err := json.Marshal(&b)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "panics": false, "json": string(blob)})
	}
	return out
}

func botWouldPatchAll() []map[string]any {
	out := make([]map[string]any, 0)
	for _, c := range botPatchCorpus() {
		b := validBot()
		// WouldPatch guards nil explicitly, unlike Patch.
		out = append(out, map[string]any{"name": c.name, "would": b.WouldPatch(c.patch)})
	}
	return out
}

// --- conversions -----------------------------------------------------------------------------

func botUserFromBotAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Bot
	}{
		{"typical", validBot()},
		{"empty_username", func() model.Bot { b := validBot(); b.Username = ""; return b }()},
		// NormalizeEmail lower-cases, so a mixed-case username produces a lower-case email while
		// the Username field itself is copied verbatim. The two therefore disagree in case.
		{"mixed_case_username", func() model.Bot { b := validBot(); b.Username = "MixedCase"; return b }()},
	}
	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		u := model.UserFromBot(&in)
		out = append(out, map[string]any{
			"name":       c.name,
			"id":         u.Id,
			"username":   u.Username,
			"email":      u.Email,
			"first_name": u.FirstName,
			"roles":      u.Roles,
			// Everything else is the User zero value — recorded so a port does not fill more in.
			"last_name":    u.LastName,
			"nickname":     u.Nickname,
			"position":     u.Position,
			"create_at":    u.CreateAt,
			"auth_service": u.AuthService,
		})
	}
	return out
}

func botBotFromUserAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.User
	}{
		{"typical", model.User{
			Id:        "y9i4er48tt8bukijy7i3u5y9ar",
			Username:  "someuser",
			FirstName: "First",
			LastName:  "Last",
			Nickname:  "Nick",
		}},
		// BotFromUser uses GetDisplayName(ShowUsername), so DisplayName is the USERNAME — not
		// the first/last name, and not the nickname. Easy to port as the wrong one.
		{"with_full_name", model.User{
			Id:        "aaaaaaaaaaaaaaaaaaaaaaaaaa",
			Username:  "uname",
			FirstName: "Given",
			LastName:  "Family",
		}},
		{"empty", model.User{}},
	}
	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		b := model.BotFromUser(&in)
		out = append(out, map[string]any{
			"name":         c.name,
			"owner_id":     b.OwnerId,
			"user_id":      b.UserId,
			"username":     b.Username,
			"display_name": b.DisplayName,
			// Not set by the conversion.
			"description": b.Description,
			"create_at":   b.CreateAt,
			"update_at":   b.UpdateAt,
			"delete_at":   b.DeleteAt,
		})
	}
	return out
}

// --- IsBotDMChannel --------------------------------------------------------------------------

func botIsBotDMChannelAll() []map[string]any {
	const botID = "y9i4er48tt8bukijy7i3u5y9ar"
	const otherID = "aaaaaaaaaaaaaaaaaaaaaaaaaa"

	corpus := []struct {
		name    string
		channel model.Channel
		botID   string
	}{
		{"direct_prefix", model.Channel{Type: model.ChannelTypeDirect, Name: botID + "__" + otherID}, botID},
		{"direct_suffix", model.Channel{Type: model.ChannelTypeDirect, Name: otherID + "__" + botID}, botID},
		{"direct_not_involved", model.Channel{Type: model.ChannelTypeDirect, Name: otherID + "__" + otherID}, botID},
		{"open_channel", model.Channel{Type: model.ChannelTypeOpen, Name: botID + "__" + otherID}, botID},
		{"group_channel", model.Channel{Type: model.ChannelTypeGroup, Name: botID + "__" + otherID}, botID},
		// The separator is part of the test: a bare id with no "__" matches neither branch.
		{"no_separator", model.Channel{Type: model.ChannelTypeDirect, Name: botID}, botID},
		// A name that merely contains the id in the middle is not a match.
		{"id_in_middle", model.Channel{Type: model.ChannelTypeDirect, Name: otherID + "__" + botID + "__x"}, botID},
		{"empty_name", model.Channel{Type: model.ChannelTypeDirect, Name: ""}, botID},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		ch := c.channel
		out = append(out, map[string]any{
			"name":   c.name,
			"result": model.IsBotDMChannel(&ch, c.botID),
		})
	}
	return out
}

// --- misc ------------------------------------------------------------------------------------

func botNotFoundErrorCase() map[string]any {
	err := model.MakeBotNotFoundError("SomeWhere", "y9i4er48tt8bukijy7i3u5y9ar")
	return map[string]any{
		"id":     err.Id,
		"where":  err.Where,
		"status": err.StatusCode,
	}
}

func botAuditableAll() map[string]any {
	b := validBot()
	s := func(v string) *string { return &v }
	p := model.BotPatch{Username: s("u"), DisplayName: s("d"), Description: s("x")}

	botBlob, err := json.Marshal(b.Auditable())
	if err != nil {
		panic(err)
	}
	// BotPatch.Auditable puts *pointers* in the map, so they marshal as their pointed-to values
	// rather than as addresses — and a nil pointer becomes null.
	patchBlob, err := json.Marshal(p.Auditable())
	if err != nil {
		panic(err)
	}
	emptyPatch := model.BotPatch{}
	nilPatchBlob, err := json.Marshal(emptyPatch.Auditable())
	if err != nil {
		panic(err)
	}

	return map[string]any{
		"bot":           string(botBlob),
		"patch":         string(patchBlob),
		"patch_all_nil": string(nilPatchBlob),
		"trace":         mustJSON(b.Trace()),
		"patch_wire":    botPatchWire(),
	}
}

func mustJSON(v any) string {
	blob, err := json.Marshal(v)
	if err != nil {
		panic(err)
	}
	return string(blob)
}
