package main

// Behavioural oracle for the user-update family, written to fixtures/behaviour_user_update.json.
//
// Four sections, each covering a decision the api4 handlers make that a reader is likely to get
// wrong in a way no happy-path test would show.
//
// # 1. `map_from_json` / `string_interface_from_json`
//
// `updateUserRoles` reads its body with `model.MapFromJSON` and `updateUserActive` with
// `model.StringInterfaceFromJSON`. Both are `json.NewDecoder(r.Body).Decode(&m)` with the error
// **discarded**, and that is not the same as "an empty map on any problem":
//
//   - `encoding/json` records the first `UnmarshalTypeError` and **keeps decoding**, so a
//     `map[string]string` given `{"roles":"system_user","n":1}` comes back holding `roles` and
//     not `n`. A Rust port reaching for `from_slice::<BTreeMap<_,_>>().unwrap_or_default()` gets
//     the *empty* map for that body — and on this route an empty map means `roles: ""`, which is
//     written. The divergence is not a status code; it is one user keeping their roles versus
//     having them erased.
//   - `Decoder.Decode` reads **one** value and ignores whatever follows it.
//   - A non-object leaves the map nil, which both helpers replace with an empty one.
//
// The corpus is driven at each of those, plus duplicate keys (last wins) and `null` values.
//
// # 2. `UserPatch` decoding
//
// `patchUser` decodes into a struct through the same `Decoder.Decode`, so it inherits the
// trailing-bytes rule — and it **rejects a JSON array**, which serde's derived struct
// deserializer accepts. That divergence was measured on `createUser` and is recorded again here
// for `UserPatch`, whose fields are all `*string` and so cannot be confused with the `User`
// corpus.
//
// # 3. `User.Patch` and `User.ToPatch`
//
// The merge that makes `patchUser` different from `updateUser`. Eleven `if patch.X != nil`
// branches — so an explicit `null` in the body is indistinguishable from an absent key and
// neither clears the field, while `""` **does** clear it. `Props`, `NotifyProps` and `Timezone`
// are whole-map replacements rather than merges. `ToPatch` is the inverse used by
// `CheckProviderAttributes`, and it **omits `RemoteId`**, so a locked-field scan driven from a
// `model.User` can never see a remote-id change.
//
// # 4. `CheckLockedProfileFields`'s field scan and `tryingToChange`
//
// Both are unexported in `channels/app` and the enclosing function is a method on `*App`, so
// there is no way to call them from here. The two are therefore **copied verbatim** below —
// the arrangement `behaviour_json_fold.go` uses for `encoding/json`'s own `foldName`, and under
// the same standing rule: if the upstream function changes, copy the change character for
// character, because the Rust side asserts against these results.
//
// Determinism: fixed corpora, no rand, no time.Now.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

// --- copied verbatim from channels/app/user.go --------------------------------------------------

const (
	lockedProfileFieldUsername  = "username"
	lockedProfileFieldFirstName = "first name"
	lockedProfileFieldLastName  = "last name"
	lockedProfileFieldNickname  = "nickname"
	lockedProfileFieldPosition  = "position"
)

// app/user.go:1392.
func tryingToChange(userValue *string, patchValue *string) bool {
	return patchValue != nil && *patchValue != *userValue
}

// The body of app/user.go:1425 with the three gates the caller holds (permission, licence,
// AuthService) removed — everything from the `setting` read down.
func lockedProfileFieldScan(setting string, user *model.User, patch *model.UserPatch) string {
	if setting != model.TeamSettingsLockProfileFieldsNameAndUsername && setting != model.TeamSettingsLockProfileFieldsAll {
		return ""
	}

	if tryingToChange(&user.Username, patch.Username) {
		return lockedProfileFieldUsername
	}

	if user.FirstName != "" && tryingToChange(&user.FirstName, patch.FirstName) {
		return lockedProfileFieldFirstName
	}
	if user.LastName != "" && tryingToChange(&user.LastName, patch.LastName) {
		return lockedProfileFieldLastName
	}

	if setting == model.TeamSettingsLockProfileFieldsAll {
		if tryingToChange(&user.Nickname, patch.Nickname) {
			return lockedProfileFieldNickname
		}
		if tryingToChange(&user.Position, patch.Position) {
			return lockedProfileFieldPosition
		}
	}

	return ""
}

// --- corpora ------------------------------------------------------------------------------------

// Bodies these two routes can actually be handed. Each is a string so the fixture records the
// exact bytes the Rust side must feed its own decoder.
var userUpdateBodyCorpus = []string{
	`{"roles":"system_user"}`,
	`{"roles":"system_user system_admin"}`,
	`{"roles":""}`,
	`{}`,
	`null`,
	`[]`,
	`[{"roles":"system_admin"}]`,
	`"roles"`,
	`7`,
	// The partial decode: a non-string value beside a good one, in both orders.
	`{"roles":"system_user","n":1}`,
	`{"n":1,"roles":"system_user"}`,
	`{"roles":1}`,
	`{"roles":null}`,
	`{"roles":["system_user"]}`,
	`{"roles":{"a":"b"}}`,
	// Duplicate keys — last wins, in both directions.
	`{"roles":"system_user","roles":"system_admin"}`,
	`{"roles":"system_admin","roles":""}`,
	// Trailing bytes after the first value.
	`{"roles":"system_user"} trailing`,
	`{"roles":"system_user"}{"roles":"system_admin"}`,
	// Malformed.
	`{"roles":`,
	``,
	// The `active` bodies.
	`{"active":true}`,
	`{"active":false}`,
	`{"active":"true"}`,
	`{"active":1}`,
	`{"active":null}`,
	`{"active":true,"x":[1,2]}`,
	`{"x":1}`,
	`{"active":true} trailing`,
	// Case: `encoding/json` matches field names case-insensitively for structs, but a MAP key is
	// taken literally, so `Active` is a different key and the assertion fails.
	`{"Active":true}`,
}

// Bodies `patchUser` can be handed. All-`*string` fields, so the null-versus-absent distinction
// is visible in the decoded struct.
var userPatchBodyCorpus = []string{
	`{"username":"newname"}`,
	`{"username":null}`,
	`{}`,
	`null`,
	`[]`,
	`["username"]`,
	`3`,
	`{"first_name":"","last_name":""}`,
	`{"remote_id":"abcdefghijklmnopqrstuvwxyz"}`,
	`{"props":{"a":"b"},"notify_props":{"c":"d"},"timezone":{"e":"f"}}`,
	`{"props":null}`,
	`{"password":"hunter2","email":"a@b.c"}`,
	`{"USERNAME":"folded"}`,
	`{"username":"first","username":"second"}`,
	`{"username":"a"} trailing`,
	`{"username":`,
	`{"unknown_field":"x","username":"kept"}`,
}

// The user the patch corpus is applied to. Every field distinct and non-zero so a branch that
// copies the wrong one is visible.
func userUpdateSeedUser() *model.User {
	return &model.User{
		Id:          "ddddddddddddddddddddddddda",
		Username:    "seeduser",
		FirstName:   "SeedFirst",
		LastName:    "SeedLast",
		Nickname:    "SeedNick",
		Position:    "SeedPosition",
		Email:       "seed@example.com",
		Password:    "seedpassword",
		Roles:       "system_user",
		Locale:      "de",
		AuthService: "",
		RemoteId:    model.NewPointer("seedremote"),
		Props:       model.StringMap{"seedprop": "seedvalue"},
		NotifyProps: model.StringMap{"seednotify": "seednotifyvalue"},
		Timezone:    model.StringMap{"useAutomaticTimezone": "true"},
	}
}

// The locked-field scan corpus: one row per branch, plus the coincidences that make a mutation
// survive — a patch naming the value the user already has, and an empty stored name.
func lockedProfileScanCases() []map[string]any {
	base := func() *model.User {
		return &model.User{
			Username:  "lockeduser",
			FirstName: "Locked",
			LastName:  "User",
			Nickname:  "Lock",
			Position:  "Engineer",
		}
	}
	nameless := func() *model.User {
		u := base()
		u.FirstName = ""
		u.LastName = ""
		return u
	}

	type row struct {
		name    string
		setting string
		user    *model.User
		patch   *model.UserPatch
	}

	rows := []row{
		{"none locks nothing", model.TeamSettingsLockProfileFieldsNone, base(), &model.UserPatch{Username: model.NewPointer("other")}},
		{"unknown setting locks nothing", "banana", base(), &model.UserPatch{Username: model.NewPointer("other")}},
		{"name_and_username username", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{Username: model.NewPointer("other")}},
		{"name_and_username username unchanged", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{Username: model.NewPointer("lockeduser")}},
		{"name_and_username username absent", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{}},
		{"name_and_username first name", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{FirstName: model.NewPointer("Other")}},
		{"name_and_username last name", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{LastName: model.NewPointer("Other")}},
		{"empty first name may be filled", model.TeamSettingsLockProfileFieldsNameAndUsername, nameless(), &model.UserPatch{FirstName: model.NewPointer("Filled")}},
		{"empty last name may be filled", model.TeamSettingsLockProfileFieldsNameAndUsername, nameless(), &model.UserPatch{LastName: model.NewPointer("Filled")}},
		{"name_and_username leaves nickname alone", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{Nickname: model.NewPointer("Other")}},
		{"name_and_username leaves position alone", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{Position: model.NewPointer("Other")}},
		{"name_and_username leaves email alone", model.TeamSettingsLockProfileFieldsNameAndUsername, base(), &model.UserPatch{Email: model.NewPointer("other@example.com")}},
		{"all nickname", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Nickname: model.NewPointer("Other")}},
		{"all position", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Position: model.NewPointer("Other")}},
		{"all username wins over position", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Username: model.NewPointer("other"), Position: model.NewPointer("Other")}},
		{"all first name wins over nickname", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{FirstName: model.NewPointer("Other"), Nickname: model.NewPointer("Other")}},
		{"all last name wins over nickname", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{LastName: model.NewPointer("Other"), Nickname: model.NewPointer("Other")}},
		{"all leaves email alone", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Email: model.NewPointer("other@example.com")}},
		// Adjacent pairs. Every ordering in the scan is between two *neighbouring* returns, and a
		// patch that moves one field gives the same answer under either order — so only a patch
		// moving both can tell a swap from the original. One row per adjacent pair, plus the
		// pair that spans the `all` boundary.
		{"all username before first name", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Username: model.NewPointer("other"), FirstName: model.NewPointer("Other")}},
		{"all first name before last name", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{FirstName: model.NewPointer("Other"), LastName: model.NewPointer("Other")}},
		{"all last name before nickname", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{LastName: model.NewPointer("Other"), Nickname: model.NewPointer("Other")}},
		{"all nickname before position", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Nickname: model.NewPointer("Other"), Position: model.NewPointer("Other")}},
		// The same pairs with the *first* of each already empty on the row, so the
		// fill-in-once escape decides the answer rather than the order.
		{"all nameless first name before last name", model.TeamSettingsLockProfileFieldsAll, nameless(), &model.UserPatch{FirstName: model.NewPointer("Other"), LastName: model.NewPointer("Other")}},
		{"name_and_username nameless names before nothing", model.TeamSettingsLockProfileFieldsNameAndUsername, nameless(), &model.UserPatch{FirstName: model.NewPointer("Other"), LastName: model.NewPointer("Other"), Nickname: model.NewPointer("Other")}},
		{"all clearing the username", model.TeamSettingsLockProfileFieldsAll, base(), &model.UserPatch{Username: model.NewPointer("")}},
	}

	out := make([]map[string]any, 0, len(rows))
	for _, r := range rows {
		out = append(out, map[string]any{
			"name":    r.name,
			"setting": r.setting,
			"user":    r.user,
			"patch":   r.patch,
			"field":   lockedProfileFieldScan(r.setting, r.user, r.patch),
		})
	}
	return out
}

// `tryingToChange` on its own, including the pointer-to-empty-string case a reader folds into
// "absent".
func tryingToChangeCases() []map[string]any {
	cases := []struct {
		user  string
		patch *string
	}{
		{"a", nil},
		{"a", model.NewPointer("a")},
		{"a", model.NewPointer("b")},
		{"a", model.NewPointer("")},
		{"", nil},
		{"", model.NewPointer("")},
		{"", model.NewPointer("b")},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		user := c.user
		out = append(out, map[string]any{
			"user":    c.user,
			"patch":   c.patch,
			"changed": tryingToChange(&user, c.patch),
		})
	}
	return out
}

// `NewSystemRoleIDs` membership, as `updateUserRoles` tests it — per whitespace-separated field.
func newSystemRoleCases() []map[string]any {
	corpus := []string{
		"system_user",
		"system_admin",
		"system_user_manager",
		"system_read_only_admin",
		"system_manager",
		"system_shared_channel_manager",
		"system_user system_manager",
		"system_user  system_manager  ",
		"system_managers",
		"System_Manager",
		"",
		"   ",
	}
	out := make([]map[string]any, 0, len(corpus))
	for _, roles := range corpus {
		needs := false
		for roleName := range stringsFieldsSeq(roles) {
			for _, id := range model.NewSystemRoleIDs {
				if roleName == id {
					needs = true
				}
			}
		}
		out = append(out, map[string]any{
			"roles":         roles,
			"valid":         model.IsValidUserRoles(roles),
			"needs_licence": needs,
		})
	}
	return out
}

// `strings.FieldsSeq` as an iterator, spelled out so this file does not depend on the Go version
// that introduced it being the one in go.mod.
func stringsFieldsSeq(s string) func(func(string) bool) {
	return func(yield func(string) bool) {
		for _, f := range splitFields(s) {
			if !yield(f) {
				return
			}
		}
	}
}

func splitFields(s string) []string {
	out := []string{}
	cur := []rune{}
	for _, r := range s {
		if r == ' ' || r == '\t' || r == '\n' || r == '\r' || r == '\v' || r == '\f' {
			if len(cur) > 0 {
				out = append(out, string(cur))
				cur = cur[:0]
			}
			continue
		}
		cur = append(cur, r)
	}
	if len(cur) > 0 {
		out = append(out, string(cur))
	}
	return out
}

// `auto_responder_active`'s two transitions (app/auto_responder.go:88).
func autoResponderCases() []map[string]any {
	values := []model.StringMap{
		nil,
		{},
		{"auto_responder_active": "true"},
		{"auto_responder_active": "false"},
		{"auto_responder_active": "True"},
		{"auto_responder_active": "1"},
		{"auto_responder_active": ""},
		{"other": "true"},
	}
	flag := func(props model.StringMap) bool { return props["auto_responder_active"] == "true" }

	out := make([]map[string]any, 0, len(values)*len(values))
	for _, old := range values {
		for _, next := range values {
			active := flag(next)
			oldActive := flag(old)
			out = append(out, map[string]any{
				"old":       old,
				"new":       next,
				"turns_on":  !oldActive && active,
				"turns_off": oldActive && !active,
			})
		}
	}
	return out
}

func mapDecodeCases() []map[string]any {
	out := make([]map[string]any, 0, len(userUpdateBodyCorpus))
	for _, body := range userUpdateBodyCorpus {
		stringMap := model.MapFromJSON(strings.NewReader(body))
		anyMap := model.StringInterfaceFromJSON(strings.NewReader(body))

		row := map[string]any{
			"in":               body,
			"map_from_json":    stringMap,
			"roles":            stringMap["roles"],
			"valid_user_roles": model.IsValidUserRoles(stringMap["roles"]),
		}

		// What `updateUserActive` actually does with the `any` map: a *type assertion*, not a
		// parse, so only a JSON boolean satisfies it.
		active, ok := anyMap["active"].(bool)
		row["active"] = active
		row["active_ok"] = ok
		row["string_interface_keys"] = userUpdateSortedKeys(anyMap)

		out = append(out, row)
	}
	return out
}

func userUpdateSortedKeys(m map[string]any) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	// A small insertion sort keeps this file free of a sort import it needs nowhere else.
	for i := 1; i < len(keys); i++ {
		for j := i; j > 0 && keys[j] < keys[j-1]; j-- {
			keys[j], keys[j-1] = keys[j-1], keys[j]
		}
	}
	return keys
}

func patchDecodeCases() []map[string]any {
	out := make([]map[string]any, 0, len(userPatchBodyCorpus))
	for _, body := range userPatchBodyCorpus {
		var patch model.UserPatch
		err := json.NewDecoder(strings.NewReader(body)).Decode(&patch)

		row := map[string]any{
			"in":     body,
			"failed": err != nil,
		}
		if err == nil {
			row["patch"] = patch

			// And the same patch applied, which is the only place the null-versus-absent
			// distinction becomes a stored value.
			applied := userUpdateSeedUser()
			applied.Patch(&patch)
			row["applied"] = applied
		}
		out = append(out, row)
	}
	return out
}

func toPatchCase() map[string]any {
	user := userUpdateSeedUser()
	return map[string]any{
		"user":  user,
		"patch": user.ToPatch(),
	}
}

func sanitizeInputCases() []map[string]any {
	out := make([]map[string]any, 0, 2)
	for _, isAdmin := range []bool{false, true} {
		user := userUpdateSeedUser()
		user.AuthData = model.NewPointer("seedauthdata")
		user.AuthService = "gitlab"
		user.EmailVerified = true
		user.CreateAt = 11
		user.UpdateAt = 22
		user.DeleteAt = 33
		user.LastPasswordUpdate = 44
		user.LastPictureUpdate = 55
		user.FailedAttempts = 6
		user.MfaActive = true
		user.MfaSecret = "seedmfa"
		user.MfaUsedTimestamps = model.StringArray{"77"}
		user.LastActivityAt = 88
		user.Email = "  spaced@example.com  "
		user.SanitizeInput(isAdmin)
		out = append(out, map[string]any{"is_admin": isAdmin, "user": user})
	}
	return out
}

func writeUserUpdateBehaviourFixture(outDir string) error {
	out := map[string]any{
		"map_decode":       mapDecodeCases(),
		"patch_decode":     patchDecodeCases(),
		"to_patch":         toPatchCase(),
		"sanitize_input":   sanitizeInputCases(),
		"locked_profile":   lockedProfileScanCases(),
		"trying_to_change": tryingToChangeCases(),
		"new_system_roles": newSystemRoleCases(),
		"auto_responder":   autoResponderCases(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_user_update.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}
