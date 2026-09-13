package main

// Behavioural oracle for `json.Unmarshal` into `model.MemberInvite`, written to
// fixtures/behaviour_member_invite.json.
//
// Driven because `inviteUsersToTeam` (api4/team.go:1751) turns the decode into a 400 with one id
// and the *result* of the decode into a 400 with two others, so every row here is a different
// answer on the wire:
//
//   - a decode error is `api.team.invite_members_to_team_and_channels.invalid_body.app_error`;
//   - `len(Emails) == 0` is `api.context.invalid_body_param.app_error` naming `user_email`;
//   - `len(Profiles) > 0` without `?graceful=` is `api.team.invite_members.profiles_graceful`.
//
// Two rules decide almost all of it, and neither is what serde does by default:
//
//   - **`MemberInvite.UnmarshalJSON` tries a bare `[]string` first** (member_invite.go:99), so
//     `["a@b.c"]` and `{"emails":["a@b.c"]}` are the same body and the array form resets every
//     other field.
//   - **`null` is never an error.** `encoding/json` sets a pointer, map, slice or interface to nil
//     and leaves anything else alone, so `null`, `{"emails":null}`, `{"emails":[null]}`,
//     `{"message":null}` and `{"profiles":[{"email":null}]}` all decode — the last two to the zero
//     string, and `[null]` to a one-element slice.
//
// `ProfileCount` is recorded separately from `Profiles` because a nil pointer in the slice is
// exactly what the `graceful` gate counts, and it is invisible in the marshalled form.
//
// Determinism: fixed inputs only.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

type memberInviteDecodeCase struct {
	Name string `json:"name"`
	Body string `json:"body"`
	// True when `json.Unmarshal` returned an error — the handler's first 400.
	Err bool `json:"err"`
	// The decoded fields, present only when Err is false.
	Emails       []string `json:"emails"`
	ChannelIDs   []string `json:"channel_ids"`
	Message      string   `json:"message"`
	ProfileCount int      `json:"profile_count"`
	// One entry per profile: false where the pointer is nil.
	ProfilesPresent []bool `json:"profiles_present"`
	// The first profile's fields, or "" everywhere when there is none or it is nil.
	FirstEmail     string `json:"first_email"`
	FirstUsername  string `json:"first_username"`
	FirstFirstName string `json:"first_first_name"`
	FirstLastName  string `json:"first_last_name"`
}

func memberInviteDecodeCorpus() []memberInviteDecodeCase {
	bodies := []struct{ name, body string }{
		// --- the bare-array form ---------------------------------------------------------------
		{"bare_array_one", `["a@example.com"]`},
		{"bare_array_two", `["a@example.com","b@example.com"]`},
		{"bare_array_empty", `[]`},
		{"bare_array_null_element", `[null]`},
		{"bare_array_of_numbers", `[1,2]`},
		{"bare_array_of_objects", `[{"emails":["a@example.com"]}]`},

		// --- the object form -------------------------------------------------------------------
		{"object_empty", `{}`},
		{"object_emails", `{"emails":["a@example.com"]}`},
		{"object_every_field", `{"emails":["a@example.com"],"channelIds":["abcdefghijklmnopqrstuvwxyz"],"message":"hi","profiles":[{"email":"a@example.com","username":"someone","first_name":"Some","last_name":"One"}]}`},
		{"object_unknown_key", `{"emails":["a@example.com"],"nope":1}`},
		{"object_channel_ids_snake_case_is_ignored", `{"emails":["a@example.com"],"channel_ids":["abcdefghijklmnopqrstuvwxyz"]}`},

		// --- null everywhere it can appear -------------------------------------------------------
		{"whole_body_null", `null`},
		{"emails_null", `{"emails":null}`},
		{"emails_null_element", `{"emails":[null]}`},
		{"emails_null_among_strings", `{"emails":["a@example.com",null]}`},
		{"message_null", `{"emails":["a@example.com"],"message":null}`},
		{"channel_ids_null", `{"emails":["a@example.com"],"channelIds":null}`},
		{"channel_ids_null_element", `{"emails":["a@example.com"],"channelIds":[null]}`},
		{"profiles_null", `{"emails":["a@example.com"],"profiles":null}`},
		{"profiles_null_element", `{"emails":["a@example.com"],"profiles":[null]}`},
		{"profiles_two_with_a_null", `{"emails":["a@example.com"],"profiles":[null,{"email":"a@example.com","username":"someone"}]}`},
		{"profile_field_null", `{"emails":["a@example.com"],"profiles":[{"email":null,"username":null,"first_name":null,"last_name":null}]}`},

		// --- genuine decode errors ---------------------------------------------------------------
		{"not_json", `garbage`},
		{"empty_body", ``},
		{"number", `5`},
		{"string", `"x"`},
		{"true", `true`},
		{"emails_is_a_number", `{"emails":5}`},
		{"emails_is_a_string", `{"emails":"a@example.com"}`},
		{"message_is_a_number", `{"emails":["a@example.com"],"message":5}`},
		{"profiles_is_an_object", `{"emails":["a@example.com"],"profiles":{}}`},
		{"trailing_garbage", `{"emails":["a@example.com"]} and then some`},
	}

	out := make([]memberInviteDecodeCase, 0, len(bodies))
	for _, b := range bodies {
		row := memberInviteDecodeCase{Name: b.name, Body: b.body}
		var invite model.MemberInvite
		if err := json.Unmarshal([]byte(b.body), &invite); err != nil {
			row.Err = true
			out = append(out, row)
			continue
		}
		row.Emails = invite.Emails
		row.ChannelIDs = invite.ChannelIds
		row.Message = invite.Message
		row.ProfileCount = len(invite.Profiles)
		row.ProfilesPresent = make([]bool, 0, len(invite.Profiles))
		for _, p := range invite.Profiles {
			row.ProfilesPresent = append(row.ProfilesPresent, p != nil)
		}
		if len(invite.Profiles) > 0 && invite.Profiles[0] != nil {
			row.FirstEmail = invite.Profiles[0].Email
			row.FirstUsername = invite.Profiles[0].Username
			row.FirstFirstName = invite.Profiles[0].FirstName
			row.FirstLastName = invite.Profiles[0].LastName
		}
		out = append(out, row)
	}
	return out
}

func writeMemberInviteBehaviourFixture(outDir string) error {
	out := map[string]any{
		"decode": memberInviteDecodeCorpus(),
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_member_invite.json"), append(blob, '\n'), 0o644)
}
