package main

// Behavioural oracle for model.User.IsValid, written to fixtures/behaviour_user_is_valid.json.
//
// Eighteen error branches. Three things about them are not visible from reading the function.
//
// # The caps are a mix of bytes and runes, and the mix is not obvious
//
//	len(u.Email)                      > UserEmailMaxLength      bytes
//	utf8.RuneCountInString(u.Nickname)> UserNicknameMaxRunes    runes
//	len(*u.AuthData)                  > UserAuthDataMaxLength   bytes
//	len(u.Roles)                      > UserRolesMaxLength      bytes
//	utf8.RuneCount(tzJSON)            > UserTimezoneMaxRunes    runes, of the MARSHALLED json
//
// The constant names say which — `MaxLength` for bytes, `MaxRunes` for runes — but only if you
// notice, and `Email` and `Roles` are the two that read like they should count characters.
//
// # A remote user may hold an invalid email
//
//	if len(u.Email) > max || u.Email == "" || (!IsValidEmail(u.Email) && !u.IsRemote()) {
//
// The emptiness and length checks apply to everyone; the *format* check is skipped for a remote
// user. So a synced user from another server can carry something that is not an email at all, and
// a port that hoists `IsValidEmail` out of that conjunction would reject it.
//
// # The timezone check measures Go's JSON, not the map
//
// It marshals `u.Timezone` and counts the **runes of the result**, so the braces, quotes, colons
// and commas all count — and Go's `encoding/json` HTML-escapes, so a `<` in a timezone name costs
// six runes rather than one ([D-022]). Driven at the boundary.
//
// # And one branch formats a POINTER
//
//	return InvalidUserError("auth_data", u.Id, u.AuthData)   // u.AuthData is *string
//
// `InvalidUserError` renders its value with `%v`, and `%v` on a `*string` prints the **address**.
// The two neighbouring auth-data branches dereference first, so only this one does it. The corpus
// records the detail string for every branch precisely so this shows up as data rather than as a
// surprise in production. See [D-107].
//
// Determinism: fixed values only, except that one address — which is why the fixture records
// whether the detail is stable rather than the detail itself for that branch.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeUserIsValidBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":     userIsValidConstants(),
		"cases":         userIsValidAll(),
		"auth_data_ptr": userAuthDataPointerProbe(),
		"timezone_json": userTimezoneJSONAll(),
		"remote_email":  userRemoteEmailAll(),
		"map_format":    userMapFormatAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_user_is_valid.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

func userIsValidConstants() map[string]any {
	return map[string]any{
		"UserEmailMaxLength":    model.UserEmailMaxLength,
		"UserNicknameMaxRunes":  model.UserNicknameMaxRunes,
		"UserPositionMaxRunes":  model.UserPositionMaxRunes,
		"UserFirstNameMaxRunes": model.UserFirstNameMaxRunes,
		"UserLastNameMaxRunes":  model.UserLastNameMaxRunes,
		"UserAuthDataMaxLength": model.UserAuthDataMaxLength,
		"UserTimezoneMaxRunes":  model.UserTimezoneMaxRunes,
		"UserRolesMaxLength":    model.UserRolesMaxLength,
		"UserLocaleMaxLength":   model.UserLocaleMaxLength,
	}
}

func uvStr(v string) *string { return &v }

func uvRunes(n int, r rune) string {
	out := make([]rune, n)
	for i := range out {
		out[i] = r
	}
	return string(out)
}

// validUser is the baseline every case mutates from.
func validUser() model.User {
	return model.User{
		Id:        "y9i4er48tt8bukijy7i3u5y9ar",
		CreateAt:  1600000000000,
		UpdateAt:  1650000000000,
		Username:  "someuser",
		Email:     "someone@example.com",
		Nickname:  "nick",
		Position:  "position",
		FirstName: "First",
		LastName:  "Last",
		Locale:    "en",
		Roles:     "system_user",
	}
}

func userIsValidAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.User
	}{
		{"valid", validUser()},

		// Identity and timestamps.
		{"bad_id", func() model.User { u := validUser(); u.Id = "nope"; return u }()},
		{"empty_id", func() model.User { u := validUser(); u.Id = ""; return u }()},
		{"zero_create_at", func() model.User { u := validUser(); u.CreateAt = 0; return u }()},
		{"zero_update_at", func() model.User { u := validUser(); u.UpdateAt = 0; return u }()},

		// Username, and the remote variant which allows more.
		{"bad_username", func() model.User { u := validUser(); u.Username = "Has Spaces"; return u }()},
		{"empty_username", func() model.User { u := validUser(); u.Username = ""; return u }()},

		// Email: length is bytes, emptiness is unconditional, format is skipped when remote.
		{"empty_email", func() model.User { u := validUser(); u.Email = ""; return u }()},
		{"bad_email", func() model.User { u := validUser(); u.Email = "not an email"; return u }()},
		{"email_at_cap", func() model.User {
			u := validUser()
			u.Email = uvRunes(model.UserEmailMaxLength-len("@example.com"), 'a') + "@example.com"
			return u
		}()},
		{"email_over_cap", func() model.User {
			u := validUser()
			u.Email = uvRunes(model.UserEmailMaxLength-len("@example.com")+1, 'a') + "@example.com"
			return u
		}()},
		// Multi-byte: the cap is BYTES, so this is over even though it is short in characters.
		{"email_multibyte_over_cap_in_bytes", func() model.User {
			u := validUser()
			u.Email = uvRunes(model.UserEmailMaxLength/2, 'é') + "@example.com"
			return u
		}()},

		// The rune-counted fields.
		{"nickname_at_cap", func() model.User {
			u := validUser()
			u.Nickname = uvRunes(model.UserNicknameMaxRunes, 'a')
			return u
		}()},
		{"nickname_over_cap", func() model.User {
			u := validUser()
			u.Nickname = uvRunes(model.UserNicknameMaxRunes+1, 'a')
			return u
		}()},
		// Multi-byte at the rune cap: passes, because runes not bytes.
		{"nickname_multibyte_at_cap", func() model.User {
			u := validUser()
			u.Nickname = uvRunes(model.UserNicknameMaxRunes, 'é')
			return u
		}()},
		{"position_over_cap", func() model.User {
			u := validUser()
			u.Position = uvRunes(model.UserPositionMaxRunes+1, 'a')
			return u
		}()},
		{"first_name_over_cap", func() model.User {
			u := validUser()
			u.FirstName = uvRunes(model.UserFirstNameMaxRunes+1, 'a')
			return u
		}()},
		{"last_name_over_cap", func() model.User {
			u := validUser()
			u.LastName = uvRunes(model.UserLastNameMaxRunes+1, 'a')
			return u
		}()},

		// The three auth-data branches, and their order.
		{"auth_data_over_cap", func() model.User {
			u := validUser()
			u.AuthData = uvStr(uvRunes(model.UserAuthDataMaxLength+1, 'a'))
			u.AuthService = "gitlab"
			return u
		}()},
		{"auth_data_without_service", func() model.User {
			u := validUser()
			u.AuthData = uvStr("some-auth-data")
			u.AuthService = ""
			return u
		}()},
		{"auth_data_with_password", func() model.User {
			u := validUser()
			u.AuthData = uvStr("some-auth-data")
			u.AuthService = "gitlab"
			u.Password = "hashed"
			return u
		}()},
		// An empty (non-nil) AuthData is not the same as nil: the service check is skipped.
		{"auth_data_empty_pointer", func() model.User {
			u := validUser()
			u.AuthData = uvStr("")
			return u
		}()},
		{"auth_data_valid", func() model.User {
			u := validUser()
			u.AuthData = uvStr("some-auth-data")
			u.AuthService = "gitlab"
			return u
		}()},
		// Password alone, with a nil AuthData, is fine.
		{"password_without_auth_data", func() model.User {
			u := validUser()
			u.Password = "hashed"
			return u
		}()},

		// Locale, now that D-001 is closed.
		{"bad_locale", func() model.User { u := validUser(); u.Locale = "xx"; return u }()},
		{"empty_locale_is_valid", func() model.User { u := validUser(); u.Locale = ""; return u }()},
		{"locale_over_length", func() model.User { u := validUser(); u.Locale = "en-USA"; return u }()},

		// Roles: BYTES.
		{"roles_at_cap", func() model.User {
			u := validUser()
			u.Roles = uvRunes(model.UserRolesMaxLength, 'a')
			return u
		}()},
		{"roles_over_cap", func() model.User {
			u := validUser()
			u.Roles = uvRunes(model.UserRolesMaxLength+1, 'a')
			return u
		}()},

		// Props gates the custom-status check entirely.
		{"nil_props_skips_custom_status", func() model.User {
			u := validUser()
			u.Props = nil
			return u
		}()},
		{"empty_props_is_not_nil", func() model.User {
			u := validUser()
			u.Props = model.StringMap{}
			return u
		}()},
		{"props_with_bad_custom_status", func() model.User {
			u := validUser()
			u.Props = model.StringMap{model.UserPropsKeyCustomStatus: "not json"}
			return u
		}()},

		// The timezone branch, which formats a map with %v — sorted keys, `map[k:v]` shape.
		{"timezone_over_cap", func() model.User {
			u := validUser()
			u.Timezone = model.StringMap{
				"automaticTimezone":    uvRunes(model.UserTimezoneMaxRunes, 'a'),
				"manualTimezone":       "b",
				"useAutomaticTimezone": "true",
			}
			return u
		}()},
		// A small map that stays under the cap, to pin the %v rendering without tripping it.
		{"timezone_small_is_valid", func() model.User {
			u := validUser()
			u.Timezone = model.StringMap{"b": "2", "a": "1"}
			return u
		}()},

		// Ordering: a user broken in two ways reports the FIRST check to fail.
		{"bad_id_and_bad_email", func() model.User {
			u := validUser()
			u.Id = "nope"
			u.Email = ""
			return u
		}()},
		{"zero_create_at_and_bad_username", func() model.User {
			u := validUser()
			u.CreateAt = 0
			u.Username = "Has Spaces"
			return u
		}()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		in := c.in
		err := in.IsValid()
		entry := map[string]any{"name": c.name}
		if err == nil {
			entry["ok"] = true
		} else {
			entry["ok"] = false
			entry["id"] = err.Id
			entry["where"] = err.Where
			entry["status"] = err.StatusCode
			entry["detailed_error"] = err.DetailedError
		}
		out = append(out, entry)
	}
	return out
}

// The pointer-formatting branch, probed on its own.
//
// `InvalidUserError("auth_data", u.Id, u.AuthData)` passes a `*string`, and `%v` on a pointer
// prints its address — so the detail differs between runs and between processes. Recorded as a
// *property* rather than a value, because the value is not reproducible.
func userAuthDataPointerProbe() map[string]any {
	makeOne := func() string {
		u := validUser()
		u.AuthData = uvStr(uvRunes(model.UserAuthDataMaxLength+1, 'a'))
		u.AuthService = "gitlab"
		return u.IsValid().DetailedError
	}

	first := makeOne()
	second := makeOne()

	return map[string]any{
		"id":                  "model.user.is_valid.auth_data.app_error",
		"detail_prefix":       strings.SplitN(first, " auth_data=", 2)[0],
		"value_is_an_address": strings.Contains(first, "0x"),
		// Two calls in one process, so if this is false the detail is not reproducible even here.
		"stable_across_calls": first == second,
	}
}

// The timezone branch measures the runes of the MARSHALLED map.
func userTimezoneJSONAll() []map[string]any {
	corpus := []struct {
		name string
		tz   model.StringMap
	}{
		{"nil", nil},
		{"empty", model.StringMap{}},
		{"typical", model.StringMap{
			"automaticTimezone":    "America/New_York",
			"manualTimezone":       "",
			"useAutomaticTimezone": "true",
		}},
		// A value containing `<`, which Go's encoding/json escapes to < — six runes, not one.
		{"html_escapable", model.StringMap{"a": "<"}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		blob, err := json.Marshal(c.tz)
		if err != nil {
			panic(err)
		}
		u := validUser()
		u.Timezone = c.tz
		entry := map[string]any{
			"name":       c.name,
			"json":       string(blob),
			"rune_count": len([]rune(string(blob))),
			"byte_count": len(blob),
		}
		if err := u.IsValid(); err != nil {
			entry["is_valid_id"] = err.Id
		} else {
			entry["is_valid_id"] = ""
		}
		out = append(out, entry)
	}
	return out
}

// The remote-user email exemption, driven both ways.
func userRemoteEmailAll() []map[string]any {
	remoteID := "aaaaaaaaaaaaaaaaaaaaaaaaaa"

	corpus := []struct {
		name     string
		email    string
		remoteID *string
	}{
		{"local_valid_email", "someone@example.com", nil},
		{"local_invalid_email", "not an email", nil},
		{"remote_valid_email", "someone@example.com", &remoteID},
		// The exemption: a remote user may hold something that is not an email.
		{"remote_invalid_email", "not an email", &remoteID},
		// ...but not an empty one, and not an over-long one.
		{"remote_empty_email", "", &remoteID},
		{"remote_over_cap_email", uvRunes(model.UserEmailMaxLength+1, 'a'), &remoteID},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		u := validUser()
		u.Email = c.email
		u.RemoteId = c.remoteID
		entry := map[string]any{"name": c.name, "is_remote": u.IsRemote()}
		if err := u.IsValid(); err != nil {
			entry["ok"] = false
			entry["id"] = err.Id
		} else {
			entry["ok"] = true
		}
		out = append(out, entry)
	}
	return out
}

// Go's `%v` on a map[string]string, which the timezone_limit branch interpolates.
//
// Since Go 1.12 map formatting sorts by key, so this is deterministic — but the shape is
// `map[k:v k2:v2]`, with no quotes and no commas, which nothing about `%v` announces.
func userMapFormatAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.StringMap
	}{
		{"nil", nil},
		{"empty", model.StringMap{}},
		{"one", model.StringMap{"a": "1"}},
		// Insertion order is not emission order: Go sorts.
		{"sorted", model.StringMap{"z": "26", "a": "1", "m": "13"}},
		{"empty_value", model.StringMap{"a": ""}},
		{"space_in_value", model.StringMap{"a": "one two"}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		out = append(out, map[string]any{
			"name":     c.name,
			"rendered": fmt.Sprintf("%v", c.in),
		})
	}
	return out
}
