package main

// Behavioural oracle for model/reaction.go, written to fixtures/behaviour_reaction.json.
//
// Small file, three things worth Go's own answer:
//
//  1. **Reaction.IsValid compiles its own emoji-name regex** (reaction.go:31) instead of calling
//     IsValidAlphaNumHyphenUnderscorePlus, and writes the character class differently
//     (`^[a-zA-Z0-9\-\+_]+$` vs `^[a-zA-Z0-9+_-]+$`). Those look equivalent; `regex_equivalence`
//     below runs both over the same corpus so the Rust port can reuse the shared validator on
//     evidence rather than on inspection.
//
//  2. **Reacting is not the same as creating an emoji.** Reaction.IsValid checks the pattern and
//     the 64-byte limit but *not* IsSystemEmojiName — so `grinning` is a legal reaction and an
//     illegal custom emoji. Two functions, one constant, different rules.
//
//  3. **PreSave calls GetMillis twice.** `CreateAt` is only set when zero, then `UpdateAt` is
//     assigned from a *separate* call, so a freshly saved reaction can have UpdateAt one
//     millisecond ahead of CreateAt — unlike Emoji.PreSave, which copies one into the other.
//     The fixture records the invariants that survive a clock, not the timestamps.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"

	"github.com/mattermost/mattermost/server/public/model"
)

// Recompiled verbatim from reaction.go:31. Copy any upstream change character for character.
var reactionValidName = regexp.MustCompile(`^[a-zA-Z0-9\-\+_]+$`)

func writeReactionBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_valid":          reactionIsValidAll(),
		"regex_equivalence": reactionRegexEquivalence(),
		"pre_save":          reactionPreSaveAll(),
		"pre_update":        reactionPreUpdateAll(),
		"get_remote_id":     reactionGetRemoteIDAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_reaction.json"), append(blob, '\n'), 0o644)
}

// --- IsValid ------------------------------------------------------------------------

type reactionValidCase struct {
	Name     string          `json:"name"`
	Reaction json.RawMessage `json:"reaction"`
	ErrorID  string          `json:"error_id"`
	Detailed string          `json:"detailed"`
}

func reactionIsValidAll() []reactionValidCase {
	type mut struct {
		name string
		fn   func(r *model.Reaction)
	}
	muts := []mut{
		{"valid", func(r *model.Reaction) {}},

		{"user_id_empty", func(r *model.Reaction) { r.UserId = "" }},
		{"user_id_short", func(r *model.Reaction) { r.UserId = repeat("a", 25) }},
		{"post_id_empty", func(r *model.Reaction) { r.PostId = "" }},
		{"post_id_long", func(r *model.Reaction) { r.PostId = repeat("a", 27) }},

		{"emoji_name_empty", func(r *model.Reaction) { r.EmojiName = "" }},
		{"emoji_name_64", func(r *model.Reaction) { r.EmojiName = repeat("a", 64) }},
		{"emoji_name_65", func(r *model.Reaction) { r.EmojiName = repeat("a", 65) }},
		{"emoji_name_multibyte_32_runes", func(r *model.Reaction) { r.EmojiName = repeat("é", 32) }},
		{"emoji_name_multibyte_33_runes", func(r *model.Reaction) { r.EmojiName = repeat("é", 33) }},
		{"emoji_name_space", func(r *model.Reaction) { r.EmojiName = "has space" }},
		{"emoji_name_dot", func(r *model.Reaction) { r.EmojiName = "has.dot" }},
		{"emoji_name_plus", func(r *model.Reaction) { r.EmojiName = "+1" }},
		{"emoji_name_hyphen", func(r *model.Reaction) { r.EmojiName = "-1" }},
		{"emoji_name_upper", func(r *model.Reaction) { r.EmojiName = "GRINNING" }},
		// A system emoji name is a perfectly legal reaction, unlike a custom emoji.
		{"emoji_name_system", func(r *model.Reaction) { r.EmojiName = "grinning" }},

		{"create_at_zero", func(r *model.Reaction) { r.CreateAt = 0 }},
		{"update_at_zero", func(r *model.Reaction) { r.UpdateAt = 0 }},
		// Neither of these is checked at all.
		{"delete_at_set", func(r *model.Reaction) { r.DeleteAt = 1700000000000 }},
		{"channel_id_empty", func(r *model.Reaction) { r.ChannelId = "" }},
		{"channel_id_nonsense", func(r *model.Reaction) { r.ChannelId = "nope" }},
		{"remote_id_nil", func(r *model.Reaction) { r.RemoteId = nil }},
	}

	var res []reactionValidCase
	for _, m := range muts {
		remote := idC
		r := &model.Reaction{
			UserId:    idA,
			PostId:    idB,
			EmojiName: "custom_emoji",
			CreateAt:  1700000000000,
			UpdateAt:  1700000000000,
			DeleteAt:  0,
			RemoteId:  &remote,
			ChannelId: idC,
		}
		m.fn(r)

		blob, err := json.Marshal(r)
		if err != nil {
			panic(err)
		}
		c := reactionValidCase{Name: m.name, Reaction: blob}
		if appErr := r.IsValid(); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

type regexEquivalenceCase struct {
	In     string `json:"in"`
	Local  bool   `json:"local"`
	Shared bool   `json:"shared"`
}

// reactionRegexEquivalence runs reaction.go's private pattern and utils.go's exported one over
// the same inputs. If they never disagree the Rust port can reuse the shared validator; the
// point is to establish that by measurement rather than by reading two character classes.
func reactionRegexEquivalence() []regexEquivalenceCase {
	inputs := []string{
		"", "a", "A", "1", "+1", "-1", "_", "-", "+",
		"custom_emoji", "custom-emoji", "custom+emoji", "CustomEmoji123",
		"has space", "has.dot", "has:colon", "has/slash", "has\\backslash",
		"héllo", "\U0001F600", "☃", "a\nb", "a\tb", " a", "a ",
		// The `-` is escaped inside the class in one and trailing in the other; these probe
		// whether either reads it as a range.
		"a-z", ",", ".", "*", "]", "^", "$",
	}
	res := make([]regexEquivalenceCase, 0, len(inputs))
	for _, in := range inputs {
		res = append(res, regexEquivalenceCase{
			In:     in,
			Local:  reactionValidName.MatchString(in),
			Shared: model.IsValidAlphaNumHyphenUnderscorePlus(in),
		})
	}
	return res
}

// --- PreSave / PreUpdate / GetRemoteID -------------------------------------------------

type reactionTimeCase struct {
	Name string `json:"name"`
	// Inputs.
	InCreateAt  int64 `json:"in_create_at"`
	InUpdateAt  int64 `json:"in_update_at"`
	InDeleteAt  int64 `json:"in_delete_at"`
	InRemoteNil bool  `json:"in_remote_nil"`
	// Invariants that hold whatever the clock says.
	CreateAtPreserved bool   `json:"create_at_preserved"`
	CreateAtChanged   bool   `json:"create_at_changed"`
	UpdateAtChanged   bool   `json:"update_at_changed"`
	OutDeleteAt       int64  `json:"out_delete_at"`
	OutRemoteNil      bool   `json:"out_remote_nil"`
	OutRemote         string `json:"out_remote"`
}

func reactionTimeCorpus() []struct {
	name                      string
	createAt, updateAt, delAt int64
	remoteNil                 bool
} {
	return []struct {
		name                      string
		createAt, updateAt, delAt int64
		remoteNil                 bool
	}{
		{"zero_create_at_nil_remote", 0, 0, 0, true},
		{"zero_create_at_set_remote", 0, 0, 0, false},
		{"existing_create_at", 1700000000000, 1700000000000, 0, true},
		{"deleted_reaction", 1700000000000, 1700000000000, 1700000000001, true},
		{"existing_remote", 1700000000000, 0, 0, false},
	}
}

func reactionPreSaveAll() []reactionTimeCase {
	var res []reactionTimeCase
	for _, c := range reactionTimeCorpus() {
		r := newReactionForTimes(c.createAt, c.updateAt, c.delAt, c.remoteNil)
		r.PreSave()
		res = append(res, describeReaction("pre_save/"+c.name, c.createAt, c.updateAt, c.delAt, c.remoteNil, r))
	}
	return res
}

func reactionPreUpdateAll() []reactionTimeCase {
	var res []reactionTimeCase
	for _, c := range reactionTimeCorpus() {
		r := newReactionForTimes(c.createAt, c.updateAt, c.delAt, c.remoteNil)
		r.PreUpdate()
		res = append(res, describeReaction("pre_update/"+c.name, c.createAt, c.updateAt, c.delAt, c.remoteNil, r))
	}
	return res
}

func newReactionForTimes(createAt, updateAt, delAt int64, remoteNil bool) *model.Reaction {
	r := &model.Reaction{
		UserId:    idA,
		PostId:    idB,
		EmojiName: "custom_emoji",
		CreateAt:  createAt,
		UpdateAt:  updateAt,
		DeleteAt:  delAt,
		ChannelId: idC,
	}
	if !remoteNil {
		remote := "cluster-a"
		r.RemoteId = &remote
	}
	return r
}

func describeReaction(name string, inCreate, inUpdate, inDelete int64, inRemoteNil bool, r *model.Reaction) reactionTimeCase {
	c := reactionTimeCase{
		Name:              name,
		InCreateAt:        inCreate,
		InUpdateAt:        inUpdate,
		InDeleteAt:        inDelete,
		InRemoteNil:       inRemoteNil,
		CreateAtPreserved: inCreate != 0 && r.CreateAt == inCreate,
		CreateAtChanged:   r.CreateAt != inCreate,
		UpdateAtChanged:   r.UpdateAt != inUpdate,
		OutDeleteAt:       r.DeleteAt,
		OutRemoteNil:      r.RemoteId == nil,
	}
	if r.RemoteId != nil {
		c.OutRemote = *r.RemoteId
	}
	return c
}

func reactionGetRemoteIDAll() map[string]string {
	nilRemote := &model.Reaction{}
	empty := ""
	set := "cluster-a"

	return map[string]string{
		"nil":   nilRemote.GetRemoteID(),
		"empty": (&model.Reaction{RemoteId: &empty}).GetRemoteID(),
		"set":   (&model.Reaction{RemoteId: &set}).GetRemoteID(),
	}
}
