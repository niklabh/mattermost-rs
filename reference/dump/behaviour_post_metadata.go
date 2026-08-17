package main

// Behavioural oracle for model/post_metadata.go, written to fixtures/behaviour_post_metadata.json.
//
// Three things need Go's own answer here:
//
//  1. **Every field of PostMetadata carries omitempty**, including the slices and maps. Go's
//     omitempty drops a nil slice *and* an empty one, so the two collapse on the wire and a
//     `Vec` with a length predicate is the faithful port rather than `Option<Vec>`. The `wire`
//     section drives nil against empty for all four collection fields.
//
//  2. **PostPriority's PostId and ChannelId use `json:",omitempty"` — an empty name.** Go falls
//     back to the *Go field name* for those, so the wire keys are `PostId` and `ChannelId`,
//     capitalised, sitting next to the snake_case ones. Same trap as TeamForExport.SchemeName.
//     PostPriority is declared in post.go, not post_metadata.go, but PostMetadata embeds it and
//     post.go embeds PostMetadata, so the two files are mutually dependent and it is recorded
//     here.
//
//  3. **Copy() is not a copy.** It is documented "does a deep copy"; it deep-copies only
//     Priority, shares every pointer in the slices and maps, and — the part worth pinning —
//     omits ExpireAt and Recipients from the struct literal it returns, so those two fields are
//     silently dropped. The `copy` section measures all of it rather than trusting the comment.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostMetadataBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":          postMetadataWireAll(),
		"priority_wire": postPriorityWireAll(),
		"copy":          postMetadataCopyAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_metadata.json"), append(blob, '\n'), 0o644)
}

type metadataWireCase struct {
	Name      string `json:"name"`
	JSON      string `json:"json"`
	Roundtrip string `json:"roundtrip"`
}

func postMetadataWireAll() []metadataWireCase {
	priority := "urgent"
	ack := true

	cases := []struct {
		name string
		pm   model.PostMetadata
	}{
		{"zero", model.PostMetadata{}},

		// nil against empty, for each collection. omitempty drops both.
		{"embeds_empty", model.PostMetadata{Embeds: []*model.PostEmbed{}}},
		{"embeds_one", model.PostMetadata{Embeds: []*model.PostEmbed{
			{Type: model.PostEmbedOpengraph, URL: "https://example.com"},
		}}},
		{"emojis_empty", model.PostMetadata{Emojis: []*model.Emoji{}}},
		{"files_empty", model.PostMetadata{Files: []*model.FileInfo{}}},
		{"reactions_empty", model.PostMetadata{Reactions: []*model.Reaction{}}},
		{"acknowledgements_empty", model.PostMetadata{Acknowledgements: []*model.PostAcknowledgement{}}},
		{"images_empty", model.PostMetadata{Images: map[string]*model.PostImage{}}},
		{"translations_empty", model.PostMetadata{Translations: map[string]*model.PostTranslation{}}},
		{"recipients_empty", model.PostMetadata{Recipients: []string{}}},

		// A nil element inside a non-empty slice marshals as null.
		{"embeds_nil_element", model.PostMetadata{Embeds: []*model.PostEmbed{nil}}},

		{"images_one", model.PostMetadata{Images: map[string]*model.PostImage{
			"https://example.com/a.png": {Width: 640, Height: 480, Format: "png", FrameCount: 0},
		}}},
		{"images_animated", model.PostMetadata{Images: map[string]*model.PostImage{
			"https://example.com/a.gif": {Width: 1, Height: 2, Format: "gif", FrameCount: 12},
		}}},

		// PostTranslation: Text and Object are alternatives, both omitempty; Type and State are
		// not, so they are always present.
		{"translation_text", model.PostMetadata{Translations: map[string]*model.PostTranslation{
			"fr": {Text: "bonjour", Type: "string", State: "done", SourceLang: "en"},
		}}},
		{"translation_object", model.PostMetadata{Translations: map[string]*model.PostTranslation{
			"fr": {Object: json.RawMessage(`{"a":1}`), Type: "object", State: "done"},
		}}},
		{"translation_zero", model.PostMetadata{Translations: map[string]*model.PostTranslation{
			"fr": {},
		}}},
		// json.RawMessage is a []byte: omitempty drops it when empty, but a RawMessage holding
		// the four bytes `null` is not empty and marshals as null.
		{"translation_object_null", model.PostMetadata{Translations: map[string]*model.PostTranslation{
			"fr": {Object: json.RawMessage(`null`), Type: "object"},
		}}},

		{"redacted_file_count", model.PostMetadata{RedactedFileCount: 3}},
		{"expire_at", model.PostMetadata{ExpireAt: 1700000000000}},
		{"recipients", model.PostMetadata{Recipients: []string{idA, idB}}},
		{"priority", model.PostMetadata{Priority: &model.PostPriority{
			Priority: &priority, RequestedAck: &ack,
		}}},
	}

	res := make([]metadataWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.pm)
		if err != nil {
			panic(err)
		}
		var back model.PostMetadata
		if err := json.Unmarshal(blob, &back); err != nil {
			panic(err)
		}
		again, err := json.Marshal(&back)
		if err != nil {
			panic(err)
		}
		res = append(res, metadataWireCase{Name: c.name, JSON: string(blob), Roundtrip: string(again)})
	}
	return res
}

// postPriorityWireAll pins the capitalised PostId/ChannelId keys, and the three pointer fields
// which have plain tags and therefore write null rather than disappearing.
func postPriorityWireAll() []metadataWireCase {
	urgent := "urgent"
	empty := ""
	yes := true
	no := false

	cases := []struct {
		name string
		pp   model.PostPriority
	}{
		{"zero", model.PostPriority{}},
		{"priority_set", model.PostPriority{Priority: &urgent}},
		{"priority_empty_string", model.PostPriority{Priority: &empty}},
		{"requested_ack_true", model.PostPriority{RequestedAck: &yes}},
		{"requested_ack_false", model.PostPriority{RequestedAck: &no}},
		{"persistent_notifications", model.PostPriority{PersistentNotifications: &yes}},
		// The two internal fields with an empty json name.
		{"post_id_set", model.PostPriority{PostId: idA}},
		{"channel_id_set", model.PostPriority{ChannelId: idB}},
		{"complete", model.PostPriority{
			Priority: &urgent, RequestedAck: &yes, PersistentNotifications: &no,
			PostId: idA, ChannelId: idB,
		}},
	}

	res := make([]metadataWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.pp)
		if err != nil {
			panic(err)
		}
		var back model.PostPriority
		if err := json.Unmarshal(blob, &back); err != nil {
			panic(err)
		}
		again, err := json.Marshal(&back)
		if err != nil {
			panic(err)
		}
		res = append(res, metadataWireCase{Name: c.name, JSON: string(blob), Roundtrip: string(again)})
	}
	return res
}

type metadataCopyCase struct {
	Name string `json:"name"`
	In   string `json:"in"`
	Out  string `json:"out"`
	// Copy() shares the element pointers, so mutating the copy's first embed is visible through
	// the original. Recorded rather than described.
	SharesEmbedPointer bool `json:"shares_embed_pointer"`
	SharesImagePointer bool `json:"shares_image_pointer"`
	// Priority is the one field genuinely deep-copied.
	SharesPriorityPointer bool `json:"shares_priority_pointer"`
	// ExpireAt and Recipients are absent from the returned struct literal.
	ExpireAtSurvived   bool `json:"expire_at_survived"`
	RecipientsSurvived bool `json:"recipients_survived"`
}

func postMetadataCopyAll() []metadataCopyCase {
	priority := "urgent"

	build := func() *model.PostMetadata {
		return &model.PostMetadata{
			Embeds:            []*model.PostEmbed{{Type: model.PostEmbedLink, URL: "https://example.com"}},
			Images:            map[string]*model.PostImage{"u": {Width: 1, Height: 2, Format: "png"}},
			Priority:          &model.PostPriority{Priority: &priority, PostId: idA},
			RedactedFileCount: 7,
			ExpireAt:          1700000000000,
			Recipients:        []string{idA, idB},
		}
	}

	var res []metadataCopyCase

	// Case 1: what survives the copy at all.
	original := build()
	before, err := json.Marshal(original)
	if err != nil {
		panic(err)
	}
	copied := original.Copy()
	after, err := json.Marshal(copied)
	if err != nil {
		panic(err)
	}
	res = append(res, metadataCopyCase{
		Name:                  "fields_dropped_by_copy",
		In:                    string(before),
		Out:                   string(after),
		SharesEmbedPointer:    len(copied.Embeds) > 0 && copied.Embeds[0] == original.Embeds[0],
		SharesImagePointer:    copied.Images["u"] == original.Images["u"],
		SharesPriorityPointer: copied.Priority == original.Priority,
		ExpireAtSurvived:      copied.ExpireAt == original.ExpireAt,
		RecipientsSurvived:    len(copied.Recipients) == len(original.Recipients),
	})

	// Case 2: an empty metadata, to show Copy() turns nil collections into empty non-nil ones.
	emptyOriginal := &model.PostMetadata{}
	emptyBefore, err := json.Marshal(emptyOriginal)
	if err != nil {
		panic(err)
	}
	emptyCopied := emptyOriginal.Copy()
	emptyAfter, err := json.Marshal(emptyCopied)
	if err != nil {
		panic(err)
	}
	res = append(res, metadataCopyCase{
		Name:                  "nil_collections_become_empty",
		In:                    string(emptyBefore),
		Out:                   string(emptyAfter),
		SharesEmbedPointer:    false,
		SharesImagePointer:    false,
		SharesPriorityPointer: emptyCopied.Priority == emptyOriginal.Priority,
		ExpireAtSurvived:      emptyCopied.ExpireAt == emptyOriginal.ExpireAt,
		RecipientsSurvived:    len(emptyCopied.Recipients) == len(emptyOriginal.Recipients),
	})

	return res
}
