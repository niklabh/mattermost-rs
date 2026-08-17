package main

// Behavioural oracle for model/post_embed.go and model/post_acknowledgement.go, written to
// fixtures/behaviour_post_leaves.json.
//
// Two small leaves that sit under post_metadata.go, which sits under post.go. Neither has much
// logic; what they have is wire-format traps:
//
//  1. **`PostEmbed.Data` is a bare `any` with `omitempty`.** Go's omitempty on an interface tests
//     `IsNil()`, not emptiness, so `Data: ""` is *emitted* as `""` while `Data: nil` is dropped.
//     A nil *pointer* stored in the interface is non-nil as an interface and marshals to `null`,
//     which is a third state a naive `Option<Value>` port collapses. The corpus drives all of it.
//
//  2. **`PostAcknowledgement.RemoteId` has `omitempty`; `Reaction.RemoteId` and
//     `FileInfo.RemoteId` do not.** Same Go type, same field name, three types, two different
//     wire behaviours. Recorded side by side so the difference is impossible to miss.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostLeavesBehaviourFixture(outDir string) error {
	out := map[string]any{
		"embed_constants":            postEmbedConstants(),
		"embed_wire":                 postEmbedWireAll(),
		"acknowledgement_wire":       postAckWireAll(),
		"acknowledgement_is_valid":   postAckIsValidAll(),
		"acknowledgement_pre_save":   postAckPreSaveAll(),
		"acknowledgement_remote_id":  postAckRemoteIDAll(),
		"remote_id_omitempty_across": remoteIDOmitemptyAcross(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_leaves.json"), append(blob, '\n'), 0o644)
}

func postEmbedConstants() map[string]string {
	return map[string]string{
		"image":              string(model.PostEmbedImage),
		"message_attachment": string(model.PostEmbedMessageAttachment),
		"opengraph":          string(model.PostEmbedOpengraph),
		"link":               string(model.PostEmbedLink),
		"permalink":          string(model.PostEmbedPermalink),
		"boards":             string(model.PostEmbedBoards),
	}
}

// --- PostEmbed ---------------------------------------------------------------------------

type embedWireCase struct {
	Name string `json:"name"`
	JSON string `json:"json"`
	// What Go produces after unmarshalling its own output and re-marshalling it. `data: null`
	// is *lossy in Go too* — an explicit null decodes to a nil interface and then disappears —
	// so this is what a Rust round-trip must reproduce, not `JSON`.
	Roundtrip string `json:"roundtrip"`
}

func postEmbedWireAll() []embedWireCase {
	var nilPtr *model.PostImage

	cases := []struct {
		name string
		pe   model.PostEmbed
	}{
		{"zero", model.PostEmbed{}},
		{"type_only", model.PostEmbed{Type: model.PostEmbedOpengraph}},
		{"url_set", model.PostEmbed{Type: model.PostEmbedImage, URL: "https://example.com/a.png"}},
		{"url_empty_is_omitted", model.PostEmbed{Type: model.PostEmbedLink, URL: ""}},
		// Data is `any` with omitempty: omitempty on an interface means IsNil, not empty.
		{"data_nil_is_omitted", model.PostEmbed{Data: nil}},
		{"data_empty_string_is_kept", model.PostEmbed{Data: ""}},
		{"data_zero_number_is_kept", model.PostEmbed{Data: 0}},
		{"data_false_is_kept", model.PostEmbed{Data: false}},
		{"data_empty_object_is_kept", model.PostEmbed{Data: map[string]any{}}},
		{"data_object", model.PostEmbed{Data: map[string]any{"site_name": "Example"}}},
		{"data_array", model.PostEmbed{Data: []any{1, "two"}}},
		// A nil pointer inside the interface: the interface is non-nil, so omitempty keeps it,
		// and it marshals to null. This is the state an Option<Value> port collapses.
		{"data_typed_nil_pointer", model.PostEmbed{Data: nilPtr}},
		// An unknown type string round-trips: PostEmbedType is a defined string, not an enum.
		{"unknown_type", model.PostEmbed{Type: model.PostEmbedType("something_new")}},
	}

	res := make([]embedWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.pe)
		if err != nil {
			panic(err)
		}
		var back model.PostEmbed
		if err := json.Unmarshal(blob, &back); err != nil {
			panic(err)
		}
		again, err := json.Marshal(&back)
		if err != nil {
			panic(err)
		}
		res = append(res, embedWireCase{Name: c.name, JSON: string(blob), Roundtrip: string(again)})
	}
	return res
}

// --- PostAcknowledgement ------------------------------------------------------------------

func postAckWireAll() []embedWireCase {
	remote := "cluster-a"
	empty := ""

	cases := []struct {
		name string
		ack  model.PostAcknowledgement
	}{
		{"zero", model.PostAcknowledgement{}},
		{"remote_id_nil_is_omitted", model.PostAcknowledgement{UserId: idA, RemoteId: nil}},
		{"remote_id_empty_is_kept", model.PostAcknowledgement{UserId: idA, RemoteId: &empty}},
		{"remote_id_set", model.PostAcknowledgement{UserId: idA, RemoteId: &remote}},
		{"complete", model.PostAcknowledgement{
			UserId: idA, PostId: idB, AcknowledgedAt: 1700000000000, ChannelId: idC, RemoteId: &remote,
		}},
	}

	res := make([]embedWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.ack)
		if err != nil {
			panic(err)
		}
		var back model.PostAcknowledgement
		if err := json.Unmarshal(blob, &back); err != nil {
			panic(err)
		}
		again, err := json.Marshal(&back)
		if err != nil {
			panic(err)
		}
		res = append(res, embedWireCase{Name: c.name, JSON: string(blob), Roundtrip: string(again)})
	}
	return res
}

// remoteIDOmitemptyAcross puts the three RemoteId fields side by side. All are *string, all are
// named remote_id — and PostAcknowledgement's is the only one with omitempty, so it is the only
// one that disappears when nil instead of writing null.
func remoteIDOmitemptyAcross() map[string]string {
	res := map[string]string{}

	ack, err := json.Marshal(&model.PostAcknowledgement{})
	if err != nil {
		panic(err)
	}
	res["post_acknowledgement"] = string(ack)

	reaction, err := json.Marshal(&model.Reaction{})
	if err != nil {
		panic(err)
	}
	res["reaction"] = string(reaction)

	fileInfo, err := json.Marshal(&model.FileInfo{})
	if err != nil {
		panic(err)
	}
	res["file_info"] = string(fileInfo)

	return res
}

type ackValidCase struct {
	Name     string          `json:"name"`
	Ack      json.RawMessage `json:"ack"`
	ErrorID  string          `json:"error_id"`
	Detailed string          `json:"detailed"`
}

func postAckIsValidAll() []ackValidCase {
	type mut struct {
		name string
		fn   func(a *model.PostAcknowledgement)
	}
	muts := []mut{
		{"valid", func(a *model.PostAcknowledgement) {}},
		{"user_id_empty", func(a *model.PostAcknowledgement) { a.UserId = "" }},
		{"user_id_short", func(a *model.PostAcknowledgement) { a.UserId = repeat("a", 25) }},
		{"post_id_empty", func(a *model.PostAcknowledgement) { a.PostId = "" }},
		{"post_id_nonsense", func(a *model.PostAcknowledgement) { a.PostId = "nope" }},
		{"channel_id_empty", func(a *model.PostAcknowledgement) { a.ChannelId = "" }},
		{"channel_id_nonsense", func(a *model.PostAcknowledgement) { a.ChannelId = "nope" }},
		// Neither of these is checked.
		{"acknowledged_at_zero", func(a *model.PostAcknowledgement) { a.AcknowledgedAt = 0 }},
		{"acknowledged_at_negative", func(a *model.PostAcknowledgement) { a.AcknowledgedAt = -1 }},
		{"remote_id_nil", func(a *model.PostAcknowledgement) { a.RemoteId = nil }},
	}

	var res []ackValidCase
	for _, m := range muts {
		remote := "cluster-a"
		a := &model.PostAcknowledgement{
			UserId:         idA,
			PostId:         idB,
			AcknowledgedAt: 1700000000000,
			ChannelId:      idC,
			RemoteId:       &remote,
		}
		m.fn(a)

		blob, err := json.Marshal(a)
		if err != nil {
			panic(err)
		}
		c := ackValidCase{Name: m.name, Ack: blob}
		if appErr := a.IsValid(); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

type ackPreSaveCase struct {
	Name       string `json:"name"`
	In         int64  `json:"in_acknowledged_at"`
	Preserved  bool   `json:"preserved"`
	Generated  bool   `json:"generated"`
	OutRemoteN bool   `json:"out_remote_nil"`
}

// postAckPreSaveAll shows how little PreSave does: it fills AcknowledgedAt when zero and touches
// nothing else. Unlike Reaction.PreSave it does *not* materialise RemoteId.
func postAckPreSaveAll() []ackPreSaveCase {
	cases := []struct {
		name string
		at   int64
	}{
		{"zero_is_filled", 0},
		{"existing_is_kept", 1700000000000},
		{"negative_is_kept", -1},
	}

	var res []ackPreSaveCase
	for _, c := range cases {
		a := &model.PostAcknowledgement{UserId: idA, AcknowledgedAt: c.at}
		a.PreSave()
		res = append(res, ackPreSaveCase{
			Name:       c.name,
			In:         c.at,
			Preserved:  c.at != 0 && a.AcknowledgedAt == c.at,
			Generated:  c.at == 0 && a.AcknowledgedAt != 0,
			OutRemoteN: a.RemoteId == nil,
		})
	}
	return res
}

func postAckRemoteIDAll() map[string]string {
	empty := ""
	set := "cluster-a"
	return map[string]string{
		"nil":   (&model.PostAcknowledgement{}).GetRemoteID(),
		"empty": (&model.PostAcknowledgement{RemoteId: &empty}).GetRemoteID(),
		"set":   (&model.PostAcknowledgement{RemoteId: &set}).GetRemoteID(),
	}
}
