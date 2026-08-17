package main

// Behavioural oracle for model/user_autocomplete.go, written to
// fixtures/behaviour_user_autocomplete.json.
//
// Three structs, five fields, every one of them `[]*User`. No methods. The types are identical;
// what differs is the **tags**, and it differs *within* the file:
//
//	UserAutocompleteInChannel.InChannel    []*User `json:"in_channel"`               <- no omitempty
//	UserAutocompleteInChannel.OutOfChannel []*User `json:"out_of_channel"`           <- no omitempty
//	UserAutocompleteInTeam.InTeam          []*User `json:"in_team"`                  <- no omitempty
//	UserAutocomplete.Users                 []*User `json:"users"`                    <- no omitempty
//	UserAutocomplete.OutOfChannel          []*User `json:"out_of_channel,omitempty"` <- omitempty
//	UserAutocomplete.Agents                []*User `json:"agents,omitempty"`         <- omitempty
//
// So `out_of_channel` appears in two of the three structs with **different** rules, and the file
// is the clearest case yet of why the tag has to be read per field rather than per type:
//
//   - Without `omitempty`, nil is `null`, empty is `[]`, and the key is always present. Three
//     states, all distinguishable.
//   - With `omitempty`, Go drops a nil slice **and** an empty one — they are indistinguishable on
//     the wire, so the faithful Rust type is a plain `Vec` with a length predicate, not an
//     `Option<Vec>`. An `Option` there would invent a distinction Go cannot express.
//
// Getting that backwards is invisible locally: the type still round-trips through its own
// serializer. It shows up as a missing key at a client. Hence the corpus drives nil and empty
// through every field of every struct and records the key's presence separately from its value.
//
// `[]*User` also means a `null` element is a legal document in Go and a failed decode for us —
// five more instances of [D-033], recorded so the entry's table can cite them rather than a new
// entry being opened.
//
// Determinism: fixed values only, and the users are built by hand rather than by NewId — see
// [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeUserAutocompleteBehaviourFixture(outDir string) error {
	out := map[string]any{
		"in_channel_keys":   expectedKeys(reflect.TypeOf(model.UserAutocompleteInChannel{})),
		"in_team_keys":      expectedKeys(reflect.TypeOf(model.UserAutocompleteInTeam{})),
		"autocomplete_keys": expectedKeys(reflect.TypeOf(model.UserAutocomplete{})),
		"in_channel_wire":   userAutocompleteInChannelWireAll(),
		"in_team_wire":      userAutocompleteInTeamWireAll(),
		"autocomplete_wire": userAutocompleteWireAll(),
		"nil_elements":      userAutocompleteNilElementAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_user_autocomplete.json"), append(blob, '\n'), 0o644)
}

// --- the corpus ---------------------------------------------------------------------------------

// uacUser is a minimal but fully-populated user. Built by hand rather than through NewId so the
// fixture stays byte-identical across runs ([D-032]), and marshalled from a Go value so every key
// is present — a hand-written partial user could not be decoded by the Rust port at all, because
// `User` is one of [D-043]'s unfixed containers.
func uacUser(id, username string) *model.User {
	return &model.User{
		Id:       id,
		CreateAt: 100,
		UpdateAt: 200,
		Username: username,
		Email:    username + "@example.com",
		Roles:    "system_user",
		Locale:   "en",
	}
}

func uacUsers() []*model.User {
	return []*model.User{
		uacUser("6bdz674pgq767e4jx75w4pf57a", "alice"),
		uacUser("qr6kf7ztp7yifxt4wm5xn51bke", "bob"),
	}
}

// keyPresence re-parses Go's own output and reports which of the named keys survived. That is the
// fact `omitempty` turns on, and it is not readable from a value comparison.
func keyPresence(blob string, names ...string) map[string]any {
	var decoded map[string]json.RawMessage
	if err := json.Unmarshal([]byte(blob), &decoded); err != nil {
		panic(err)
	}
	out := map[string]any{}
	for _, n := range names {
		raw, ok := decoded[n]
		out[n+"_present"] = ok
		out[n+"_is_null"] = ok && string(raw) == "null"
	}
	return out
}

// --- UserAutocompleteInChannel ------------------------------------------------------------------

// Neither field has omitempty, so nil and empty are two documents and the keys never disappear.
func userAutocompleteInChannelWireAll() []map[string]any {
	corpus := []struct {
		name                 string
		inChannel, outOfChan []*model.User
	}{
		{"both_nil", nil, nil},
		{"both_empty", []*model.User{}, []*model.User{}},
		{"in_nil_out_empty", nil, []*model.User{}},
		{"in_empty_out_nil", []*model.User{}, nil},
		{"in_populated", uacUsers(), nil},
		{"both_populated", uacUsers(), uacUsers()[:1]},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			in := model.UserAutocompleteInChannel{InChannel: c.inChannel, OutOfChannel: c.outOfChan}
			blob := mustMarshal(in)
			row["json"] = blob
			row["in_channel_nil"] = c.inChannel == nil
			row["out_of_channel_nil"] = c.outOfChan == nil
			for k, v := range keyPresence(blob, "in_channel", "out_of_channel") {
				row[k] = v
			}
		})
		res = append(res, row)
	}
	return res
}

func userAutocompleteInTeamWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   []*model.User
	}{
		{"nil", nil},
		{"empty", []*model.User{}},
		{"populated", uacUsers()},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			blob := mustMarshal(model.UserAutocompleteInTeam{InTeam: c.in})
			row["json"] = blob
			row["in_team_nil"] = c.in == nil
			for k, v := range keyPresence(blob, "in_team") {
				row[k] = v
			}
		})
		res = append(res, row)
	}
	return res
}

// --- UserAutocomplete ------------------------------------------------------------------------------

// The interesting one: `users` has no omitempty and the other two do, so a single value produces
// a document where one nil slice is `null` and two nil slices are simply gone.
func userAutocompleteWireAll() []map[string]any {
	corpus := []struct {
		name                   string
		users, outOfChan, bots []*model.User
	}{
		{"all_nil", nil, nil, nil},
		{"all_empty", []*model.User{}, []*model.User{}, []*model.User{}},
		// The pair that proves omitempty collapses nil and empty: these two rows must produce
		// **identical** documents for the last two keys.
		{"optional_nil", uacUsers(), nil, nil},
		{"optional_empty", uacUsers(), []*model.User{}, []*model.User{}},
		{"users_nil_others_set", nil, uacUsers(), uacUsers()},
		{"out_of_channel_only", nil, uacUsers()[:1], nil},
		{"agents_only", nil, nil, uacUsers()[:1]},
		{"all_populated", uacUsers(), uacUsers()[:1], uacUsers()[1:]},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			in := model.UserAutocomplete{Users: c.users, OutOfChannel: c.outOfChan, Agents: c.bots}
			blob := mustMarshal(in)
			row["json"] = blob
			row["users_nil"] = c.users == nil
			row["out_of_channel_nil"] = c.outOfChan == nil
			row["agents_nil"] = c.bots == nil
			for k, v := range keyPresence(blob, "users", "out_of_channel", "agents") {
				row[k] = v
			}
		})
		res = append(res, row)
	}
	return res
}

// --- nil elements ------------------------------------------------------------------------------------

// userAutocompleteNilElementAll records [D-033] for all five fields. Go stores the nil pointer and
// re-emits it as `null`; our `Vec<User>` cannot hold one, so the whole document fails to decode.
// One row per field so the entry's table can cite each rather than generalising from one.
func userAutocompleteNilElementAll() []map[string]any {
	docs := []struct{ name, field, doc string }{
		{"in_channel", "in_channel", `{"in_channel":[null],"out_of_channel":null}`},
		{"in_channel_out_of_channel", "out_of_channel", `{"in_channel":null,"out_of_channel":[null]}`},
		{"in_team", "in_team", `{"in_team":[null]}`},
		{"autocomplete_users", "users", `{"users":[null]}`},
		{"autocomplete_out_of_channel", "out_of_channel", `{"users":null,"out_of_channel":[null]}`},
		{"autocomplete_agents", "agents", `{"users":null,"agents":[null]}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "field": d.field, "in": d.doc}
		probe(row, func() {
			// Decode into whichever type owns the field, then re-marshal.
			var blob string
			var elementNil bool
			switch {
			case d.name == "in_team":
				var out model.UserAutocompleteInTeam
				if err := json.Unmarshal([]byte(d.doc), &out); err != nil {
					panic(err)
				}
				blob = mustMarshal(out)
				elementNil = len(out.InTeam) == 1 && out.InTeam[0] == nil
			case d.name == "in_channel" || d.name == "in_channel_out_of_channel":
				var out model.UserAutocompleteInChannel
				if err := json.Unmarshal([]byte(d.doc), &out); err != nil {
					panic(err)
				}
				blob = mustMarshal(out)
				list := out.InChannel
				if d.field == "out_of_channel" {
					list = out.OutOfChannel
				}
				elementNil = len(list) == 1 && list[0] == nil
			default:
				var out model.UserAutocomplete
				if err := json.Unmarshal([]byte(d.doc), &out); err != nil {
					panic(err)
				}
				blob = mustMarshal(out)
				list := out.Users
				switch d.field {
				case "out_of_channel":
					list = out.OutOfChannel
				case "agents":
					list = out.Agents
				}
				elementNil = len(list) == 1 && list[0] == nil
			}
			row["json_after"] = blob
			row["element_nil"] = elementNil
		})
		res = append(res, row)
	}
	return res
}
