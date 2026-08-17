package main

// Behavioural oracle for model/channel.go, written to fixtures/behaviour_channel.json.
//
// Same contract as behaviour.go: run a corpus through the real Go implementations and record
// what they returned. Channel is the first type whose IsValid() depends on the *combination*
// of fields (type + name + banner + discoverable + group_constrained), so the corpus is a list
// of whole channels rather than a list of strings. Each case embeds the channel as JSON, which
// the Rust side deserializes into its own Channel and runs through its own is_valid() — so the
// test exercises the wire format and the logic in one step.
//
// The two regexes are unexported in the model package and are recompiled here verbatim from
// channel.go:22 and channel.go:294. Copy any upstream change to them character for character.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"

	"github.com/mattermost/mattermost/server/public/model"
)

var (
	channelHexColorRegex = regexp.MustCompile(`^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$`)
	gmNameRegex          = regexp.MustCompile("^[a-f0-9]{40}$")
)

// Two ids that are valid per IsValidId, used to build the "name looks like a DM" cases.
const (
	idA = "6bdz674pgq767e4jx75w4pf57a"
	idB = "qr6kf7ztp7yifxt4wm5xn51bke"
	idC = "g1ku9ozj3bhub3hs89bqu1m3gy"
)

type channelValidCase struct {
	Name     string          `json:"name"`
	Channel  json.RawMessage `json:"channel"`
	ErrorID  string          `json:"error_id"`
	Detailed string          `json:"detailed"`
}

type channelPatchCase struct {
	Name   string          `json:"name"`
	Before json.RawMessage `json:"before"`
	Patch  json.RawMessage `json:"patch"`
	After  json.RawMessage `json:"after"`
}

type dmPairCase struct {
	Name        string `json:"name"`
	Type        string `json:"type"`
	ChannelName string `json:"channel_name"`
	User1       string `json:"user1"`
	User2       string `json:"user2"`
	OtherFor1   string `json:"other_for_user1"`
	OtherForX   string `json:"other_for_stranger"`
}

type groupDisplayNameCase struct {
	Usernames []string `json:"usernames"`
	Truncate  bool     `json:"truncate"`
	Out       string   `json:"out"`
}

func writeChannelBehaviourFixture(outDir string) error {
	out := map[string]any{
		"channel_hex_color":              hexColorAll(),
		"gm_name_regex":                  gmNameAll(),
		"is_valid_channel_identifier":    channelIdentifierAll(),
		"channel_is_valid":               channelIsValidAll(),
		"channel_is_valid_board":         channelIsValidBoardAll(),
		"channel_pre_save":               channelPreSaveAll(),
		"channel_patch":                  channelPatchAll(),
		"channel_sanitize":               channelSanitizeAll(),
		"channel_type_predicates":        channelTypePredicatesAll(),
		"get_dm_name_from_ids":           dmNameAll(),
		"get_both_users_for_dm":          bothUsersAll(),
		"get_group_name_from_user_ids":   groupNameAll(),
		"get_group_display_name":         groupDisplayNameAll(),
		"channel_banner_info_round_trip": bannerRoundTripAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel.json"), append(blob, '\n'), 0o644)
}

// --- regexes ------------------------------------------------------------------

var hexColorCorpus = []string{
	"", "#fff", "#FFF", "#ffffff", "#FFFFFF", "#AbC123", "#abc123",
	"fff", "ffffff", "#ff", "#ffff", "#fffff", "#fffffff",
	"#ggg", "#gggggg", "#12g", "  #fff", "#fff ", "#fff\n", "\n#fff",
	"#fff#fff", "##fff", "#000000", "#0", "#",
}

func hexColorAll() map[string]bool {
	res := make(map[string]bool, len(hexColorCorpus))
	for _, in := range hexColorCorpus {
		res[in] = channelHexColorRegex.MatchString(in)
	}
	return res
}

var gmNameCorpus = []string{
	"", "town-square",
	repeat("a", 40), repeat("f", 40), repeat("0", 40),
	repeat("a", 39), repeat("a", 41),
	repeat("A", 40), // uppercase hex is NOT matched
	repeat("g", 40),
	"3f786850e387550fdab836ed7e6dc881de23001b", // sha1("a")
	"3f786850e387550fdab836ed7e6dc881de23001",  // one short
	repeat("a", 20) + "-" + repeat("a", 19),
	"\n" + repeat("a", 40),
	repeat("a", 40) + "\n",
}

func gmNameAll() map[string]bool {
	res := make(map[string]bool, len(gmNameCorpus))
	for _, in := range gmNameCorpus {
		res[in] = gmNameRegex.MatchString(in)
	}
	return res
}

// channelNameCorpus covers the alternation boundaries in validSimpleAlphaNum plus the
// shapes IsValid() treats specially (DM-like and GM-like names).
var channelNameCorpus = []string{
	"", "a", "ab", "1", "a1", "town-square", "off-topic",
	"a_b", "a__b", "a___b", "-a", "a-", "_a", "a_", "__", "--",
	"A", "Town-Square", "a b", "a.b", "a+b", "é", "café",
	repeat("a", 64), repeat("a", 65),
	idA + "__" + idB,
	repeat("a", 40),
}

func channelIdentifierAll() map[string]bool {
	res := make(map[string]bool, len(channelNameCorpus))
	for _, in := range channelNameCorpus {
		res[in] = model.IsValidChannelIdentifier(in)
	}
	return res
}

// --- IsValid ------------------------------------------------------------------

func baseChannel() *model.Channel {
	return &model.Channel{
		Id:          idA,
		CreateAt:    1701310223000,
		UpdateAt:    1705492114000,
		TeamId:      idB,
		Type:        model.ChannelTypeOpen,
		DisplayName: "Town Square",
		Name:        "town-square",
		CreatorId:   idC,
	}
}

func ptr[T any](v T) *T { return &v }

func channelIsValidAll() []channelValidCase {
	type mut struct {
		name string
		fn   func(c *model.Channel)
	}
	muts := []mut{
		{"valid_open", func(c *model.Channel) {}},
		{"valid_private", func(c *model.Channel) { c.Type = model.ChannelTypePrivate }},
		{"valid_space", func(c *model.Channel) { c.Type = model.ChannelTypeSpace }},
		{"valid_open_board", func(c *model.Channel) { c.Type = model.ChannelTypeOpenBoard }},
		{"valid_private_board", func(c *model.Channel) { c.Type = model.ChannelTypePrivateBoard }},

		{"id_empty", func(c *model.Channel) { c.Id = "" }},
		{"id_short", func(c *model.Channel) { c.Id = repeat("a", 25) }},
		{"id_long", func(c *model.Channel) { c.Id = repeat("a", 27) }},
		{"create_at_zero", func(c *model.Channel) { c.CreateAt = 0 }},
		{"update_at_zero", func(c *model.Channel) { c.UpdateAt = 0 }},

		{"display_name_empty", func(c *model.Channel) { c.DisplayName = "" }},
		{"display_name_64", func(c *model.Channel) { c.DisplayName = repeat("a", 64) }},
		{"display_name_65", func(c *model.Channel) { c.DisplayName = repeat("a", 65) }},
		{"display_name_64_runes_multibyte", func(c *model.Channel) { c.DisplayName = repeat("é", 64) }},
		{"display_name_65_runes_multibyte", func(c *model.Channel) { c.DisplayName = repeat("é", 65) }},

		{"name_empty", func(c *model.Channel) { c.Name = "" }},
		{"name_uppercase", func(c *model.Channel) { c.Name = "Town-Square" }},
		{"name_single_char", func(c *model.Channel) { c.Name = "a" }},
		{"name_leading_hyphen", func(c *model.Channel) { c.Name = "-a" }},
		{"name_65_chars", func(c *model.Channel) { c.Name = repeat("a", 65) }},

		{"type_empty", func(c *model.Channel) { c.Type = "" }},
		{"type_unknown", func(c *model.Channel) { c.Type = "X" }},
		{"type_lowercase_o", func(c *model.Channel) { c.Type = "o" }},

		{"header_1024", func(c *model.Channel) { c.Header = repeat("a", 1024) }},
		{"header_1025", func(c *model.Channel) { c.Header = repeat("a", 1025) }},
		{"header_1025_runes_multibyte", func(c *model.Channel) { c.Header = repeat("☃", 1025) }},
		{"purpose_250", func(c *model.Channel) { c.Purpose = repeat("a", 250) }},
		{"purpose_251", func(c *model.Channel) { c.Purpose = repeat("a", 251) }},

		{"creator_id_empty", func(c *model.Channel) { c.CreatorId = "" }},
		{"creator_id_26", func(c *model.Channel) { c.CreatorId = repeat("a", 26) }},
		{"creator_id_27", func(c *model.Channel) { c.CreatorId = repeat("a", 27) }},
		{"creator_id_not_an_id_but_short", func(c *model.Channel) { c.CreatorId = "nope" }},

		// The DM/GM name-collision guard. Only applies to non-D, non-G types.
		{"open_with_gm_shaped_name", func(c *model.Channel) { c.Name = repeat("a", 40) }},
		{"open_with_dm_shaped_name", func(c *model.Channel) { c.Name = idA + "__" + idB }},
		{"open_with_dm_shaped_name_same_id", func(c *model.Channel) { c.Name = idA + "__" + idA }},
		{"open_with_three_part_name", func(c *model.Channel) { c.Name = idA + "__" + idB + "__" + idC }},
		{"open_with_dm_shape_invalid_ids", func(c *model.Channel) { c.Name = "abc__def" }},
		{"direct_with_dm_shaped_name", func(c *model.Channel) {
			c.Type = model.ChannelTypeDirect
			c.Name = idA + "__" + idB
		}},
		{"group_with_gm_shaped_name", func(c *model.Channel) {
			c.Type = model.ChannelTypeGroup
			c.Name = repeat("a", 40)
		}},
		{"space_with_gm_shaped_name", func(c *model.Channel) {
			c.Type = model.ChannelTypeSpace
			c.Name = repeat("a", 40)
		}},

		// Banner info. Only checked when Enabled is non-nil and true.
		{"banner_nil", func(c *model.Channel) { c.BannerInfo = nil }},
		{"banner_enabled_nil", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Text: ptr("hi"), BackgroundColor: ptr("#fff")}
		}},
		{"banner_disabled_with_junk", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(false), Text: ptr(""), BackgroundColor: ptr("nope")}
		}},
		{"banner_enabled_ok", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi"), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_enabled_on_direct", func(c *model.Channel) {
			c.Type = model.ChannelTypeDirect
			c.Name = idA + "__" + idB
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi"), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_enabled_on_space", func(c *model.Channel) {
			c.Type = model.ChannelTypeSpace
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi"), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_text_nil", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_text_empty", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr(""), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_text_1024", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr(repeat("a", 1024)), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_text_1025", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr(repeat("a", 1025)), BackgroundColor: ptr("#ff0000")}
		}},
		// The text limit is len() — bytes, not runes. 400 snowmen is 1200 bytes.
		{"banner_text_400_multibyte", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr(repeat("☃", 400)), BackgroundColor: ptr("#ff0000")}
		}},
		{"banner_color_nil", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi")}
		}},
		{"banner_color_empty", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi"), BackgroundColor: ptr("")}
		}},
		{"banner_color_invalid", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi"), BackgroundColor: ptr("red")}
		}},
		{"banner_color_3_digit", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("hi"), BackgroundColor: ptr("#FFF")}
		}},

		{"discoverable_open", func(c *model.Channel) { c.Discoverable = true }},
		{"discoverable_private", func(c *model.Channel) {
			c.Discoverable = true
			c.Type = model.ChannelTypePrivate
		}},
		{"discoverable_space", func(c *model.Channel) {
			c.Discoverable = true
			c.Type = model.ChannelTypeSpace
		}},

		{"group_constrained_open", func(c *model.Channel) { c.GroupConstrained = ptr(true) }},
		{"group_constrained_private", func(c *model.Channel) {
			c.GroupConstrained = ptr(true)
			c.Type = model.ChannelTypePrivate
		}},
		{"group_constrained_space", func(c *model.Channel) {
			c.GroupConstrained = ptr(true)
			c.Type = model.ChannelTypeSpace
		}},
		{"group_constrained_board", func(c *model.Channel) {
			c.GroupConstrained = ptr(true)
			c.Type = model.ChannelTypeOpenBoard
		}},
		{"group_constrained_false_board", func(c *model.Channel) {
			c.GroupConstrained = ptr(false)
			c.Type = model.ChannelTypeOpenBoard
		}},
		{"group_constrained_direct", func(c *model.Channel) {
			c.GroupConstrained = ptr(true)
			c.Type = model.ChannelTypeDirect
			c.Name = idA + "__" + idB
		}},

		// Ordering probe: two failures at once. Records which one Go reports first.
		{"create_at_zero_and_bad_type", func(c *model.Channel) {
			c.CreateAt = 0
			c.Type = "X"
		}},
		{"bad_name_and_bad_banner", func(c *model.Channel) {
			c.Name = "-a"
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), BackgroundColor: ptr("red")}
		}},
		{"discoverable_and_group_constrained_open", func(c *model.Channel) {
			c.Discoverable = true
			c.GroupConstrained = ptr(true)
		}},
	}

	res := make([]channelValidCase, 0, len(muts))
	for _, m := range muts {
		c := baseChannel()
		m.fn(c)
		blob, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		var id, detailed string
		if appErr := c.IsValid(); appErr != nil {
			id = appErr.Id
			detailed = appErr.DetailedError
		}
		res = append(res, channelValidCase{Name: m.name, Channel: blob, ErrorID: id, Detailed: detailed})
	}
	return res
}

func channelIsValidBoardAll() []channelValidCase {
	type mut struct {
		name string
		fn   func(c *model.Channel)
	}
	muts := []mut{
		{"open_board_ok", func(c *model.Channel) { c.Type = model.ChannelTypeOpenBoard }},
		{"private_board_ok", func(c *model.Channel) { c.Type = model.ChannelTypePrivateBoard }},
		{"not_a_board", func(c *model.Channel) {}},
		{"space_is_not_a_board", func(c *model.Channel) { c.Type = model.ChannelTypeSpace }},
		{"board_no_team", func(c *model.Channel) {
			c.Type = model.ChannelTypeOpenBoard
			c.TeamId = ""
		}},
		{"board_no_display_name", func(c *model.Channel) {
			c.Type = model.ChannelTypeOpenBoard
			c.DisplayName = ""
		}},
		{"board_no_team_and_no_display_name", func(c *model.Channel) {
			c.Type = model.ChannelTypeOpenBoard
			c.TeamId = ""
			c.DisplayName = ""
		}},
		// IsValidBoard does not check id/create_at, unlike IsValid.
		{"board_empty_id", func(c *model.Channel) {
			c.Type = model.ChannelTypeOpenBoard
			c.Id = ""
			c.CreateAt = 0
		}},
	}

	res := make([]channelValidCase, 0, len(muts))
	for _, m := range muts {
		c := baseChannel()
		m.fn(c)
		blob, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		var id, detailed string
		if appErr := c.IsValidBoard(); appErr != nil {
			id = appErr.Id
			detailed = appErr.DetailedError
		}
		res = append(res, channelValidCase{Name: m.name, Channel: blob, ErrorID: id, Detailed: detailed})
	}
	return res
}

// --- PreSave / PreUpdate ------------------------------------------------------

// channelPreSaveAll only uses cases with a non-zero CreateAt and a non-empty Id, so the
// result is deterministic — GetMillis() and NewId() cannot be pinned in a fixture. The Rust
// side asserts the generated-value branches by shape.
func channelPreSaveAll() []channelPatchCase {
	type mut struct {
		name string
		fn   func(c *model.Channel)
	}
	muts := []mut{
		{"keeps_create_at_and_mirrors_it_to_update_at", func(c *model.Channel) {
			c.UpdateAt = 999
			c.ExtraUpdateAt = 777
		}},
		{"sanitizes_name_and_display_name", func(c *model.Channel) {
			c.Name = "town‮square"
			c.DisplayName = "Town Square"
		}},
		{"does_not_sanitize_header_or_purpose", func(c *model.Channel) {
			c.Header = "head‮er"
			c.Purpose = "purp‮ose"
		}},
	}

	res := make([]channelPatchCase, 0, len(muts))
	for _, m := range muts {
		c := baseChannel()
		m.fn(c)
		before, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		c.PreSave()
		after, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		res = append(res, channelPatchCase{Name: "pre_save/" + m.name, Before: before, After: after})
	}
	return res
}

// --- Patch --------------------------------------------------------------------

func channelPatchAll() []channelPatchCase {
	type mut struct {
		name  string
		setup func(c *model.Channel)
		patch *model.ChannelPatch
	}
	muts := []mut{
		{"empty_patch", nil, &model.ChannelPatch{}},
		{"display_name_is_trimmed", nil, &model.ChannelPatch{DisplayName: ptr("  Spaced  ")}},
		{"name_is_not_trimmed", nil, &model.ChannelPatch{Name: ptr("  spaced  ")}},
		{"header_and_purpose", nil, &model.ChannelPatch{Header: ptr(" h "), Purpose: ptr(" p ")}},
		{"group_constrained_true", nil, &model.ChannelPatch{GroupConstrained: ptr(true)}},
		{"group_constrained_false", func(c *model.Channel) { c.GroupConstrained = ptr(true) },
			&model.ChannelPatch{GroupConstrained: ptr(false)}},
		{"autotranslation", nil, &model.ChannelPatch{AutoTranslation: ptr(true)}},
		{"discoverable", nil, &model.ChannelPatch{Discoverable: ptr(true)}},
		{"default_category_name_is_trimmed", nil, &model.ChannelPatch{DefaultCategoryName: ptr("  cat  ")}},
		// ManagedCategoryName is declared on the patch but never applied by Patch().
		{"managed_category_name_is_ignored", nil, &model.ChannelPatch{ManagedCategoryName: ptr("managed")}},
		{"banner_created_from_nil", nil, &model.ChannelPatch{
			BannerInfo: &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("t"), BackgroundColor: ptr("#fff")},
		}},
		{"banner_partial_merge", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("old"), BackgroundColor: ptr("#000")}
		}, &model.ChannelPatch{BannerInfo: &model.ChannelBannerInfo{Text: ptr("new")}}},
		{"banner_empty_patch_leaves_existing", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("old"), BackgroundColor: ptr("#000")}
		}, &model.ChannelPatch{BannerInfo: &model.ChannelBannerInfo{}}},
		{"banner_empty_patch_on_nil_creates_empty", nil, &model.ChannelPatch{BannerInfo: &model.ChannelBannerInfo{}}},
		{"nil_banner_patch_leaves_existing", func(c *model.Channel) {
			c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("old"), BackgroundColor: ptr("#000")}
		}, &model.ChannelPatch{DisplayName: ptr("x")}},
	}

	res := make([]channelPatchCase, 0, len(muts))
	for _, m := range muts {
		c := baseChannel()
		if m.setup != nil {
			m.setup(c)
		}
		before, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		patchBlob, err := json.Marshal(m.patch)
		if err != nil {
			panic(err)
		}
		c.Patch(m.patch)
		after, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		res = append(res, channelPatchCase{Name: m.name, Before: before, Patch: patchBlob, After: after})
	}
	return res
}

// channelSanitizeAll pins which fields survive Sanitize. Everything else must be zeroed.
func channelSanitizeAll() []channelPatchCase {
	c := baseChannel()
	c.Header = "header"
	c.Purpose = "purpose"
	c.Props = map[string]any{"secret": "value"}
	c.SchemeId = ptr(idC)
	c.GroupConstrained = ptr(true)
	c.Shared = ptr(true)
	c.PolicyID = ptr(idB)
	c.PolicyEnforced = true
	c.PolicyActions = map[string]bool{"membership": true}
	c.BannerInfo = &model.ChannelBannerInfo{Enabled: ptr(true), Text: ptr("t"), BackgroundColor: ptr("#fff")}
	c.Discoverable = true
	c.DefaultCategoryName = "cat"
	c.ManagedCategoryName = "managed"

	before, err := json.Marshal(c)
	if err != nil {
		panic(err)
	}
	sanitized := c.Sanitize()
	after, err := json.Marshal(&sanitized)
	if err != nil {
		panic(err)
	}
	return []channelPatchCase{{Name: "sanitize_full", Before: before, After: after}}
}

// --- type predicates ----------------------------------------------------------

func channelTypePredicatesAll() map[string]map[string]bool {
	types := []model.ChannelType{"O", "P", "D", "G", "S", "BO", "BP", "", "X", "o", "bo"}
	res := make(map[string]map[string]bool, len(types))
	for _, t := range types {
		c := &model.Channel{Type: t}
		res[string(t)] = map[string]bool{
			"is_group_or_direct":  c.IsGroupOrDirect(),
			"supports_group_sync": c.SupportsGroupSync(),
			"is_open":             c.IsOpen(),
			"is_board":            c.IsBoard(),
			"is_space":            c.IsSpace(),
			"is_message_channel":  c.IsMessageChannel(),
			"is_open_board":       c.IsOpenBoard(),
			"is_private_board":    c.IsPrivateBoard(),
		}
	}
	return res
}

// --- DM / GM naming -----------------------------------------------------------

func dmNameAll() map[string]string {
	pairs := [][2]string{
		{idA, idB}, {idB, idA}, {idA, idA}, {"", idA}, {idA, ""}, {"", ""},
		{"a", "b"}, {"b", "a"}, {"A", "a"}, {"a", "A"}, {"aa", "b"}, {"b", "aa"},
	}
	res := make(map[string]string, len(pairs))
	for _, p := range pairs {
		res[p[0]+"|"+p[1]] = model.GetDMNameFromIds(p[0], p[1])
	}
	return res
}

func bothUsersAll() []dmPairCase {
	cases := []struct {
		name string
		typ  model.ChannelType
		cn   string
	}{
		{"direct_two_ids", model.ChannelTypeDirect, idA + "__" + idB},
		{"direct_self", model.ChannelTypeDirect, idA + "__" + idA},
		{"direct_one_part", model.ChannelTypeDirect, idA},
		{"direct_three_parts", model.ChannelTypeDirect, idA + "__" + idB + "__" + idC},
		{"direct_empty_name", model.ChannelTypeDirect, ""},
		{"direct_trailing_sep", model.ChannelTypeDirect, idA + "__"},
		{"direct_leading_sep", model.ChannelTypeDirect, "__" + idB},
		{"direct_non_id_parts", model.ChannelTypeDirect, "a__b"},
		{"open_with_dm_name", model.ChannelTypeOpen, idA + "__" + idB},
		{"group_with_dm_name", model.ChannelTypeGroup, idA + "__" + idB},
	}
	res := make([]dmPairCase, 0, len(cases))
	for _, tc := range cases {
		c := &model.Channel{Type: tc.typ, Name: tc.cn}
		u1, u2 := c.GetBothUsersForDM()
		res = append(res, dmPairCase{
			Name:        tc.name,
			Type:        string(tc.typ),
			ChannelName: tc.cn,
			User1:       u1,
			User2:       u2,
			OtherFor1:   c.GetOtherUserIdForDM(idA),
			OtherForX:   c.GetOtherUserIdForDM("stranger"),
		})
	}
	return res
}

func groupNameAll() map[string]string {
	corpus := [][]string{
		{},
		{idA},
		{idA, idB},
		{idB, idA},
		{idA, idB, idC},
		{idC, idB, idA},
		{idA, idA},
		{""},
		{"", ""},
		{"a", "b", "c"},
		{"é"},
	}
	res := make(map[string]string, len(corpus))
	for _, ids := range corpus {
		// GetGroupNameFromUserIds sorts its argument in place. Pass a copy so the key
		// records the caller's original order.
		key := ""
		for i, id := range ids {
			if i > 0 {
				key += "|"
			}
			key += id
		}
		dup := append([]string(nil), ids...)
		res[key] = model.GetGroupNameFromUserIds(dup)
	}
	return res
}

func groupDisplayNameAll() []groupDisplayNameCase {
	corpus := [][]string{
		{},
		{"alice"},
		{"charlie", "alice", "bob"},
		{"alice", "alice"},
		{"Alice", "alice"},
		{""},
		// 10 x "aaaaaaa, " is 90 bytes -> truncation at 64 lands mid-name.
		{"aaaaaaa", "bbbbbbb", "ccccccc", "ddddddd", "eeeeeee", "fffffff", "ggggggg", "hhhhhhh", "iiiiiii", "jjjjjjj"},
		// Multibyte: Go truncates at an exact byte offset and can split a rune.
		{repeat("é", 40)},
	}
	res := make([]groupDisplayNameCase, 0, len(corpus)*2)
	for _, names := range corpus {
		for _, truncate := range []bool{false, true} {
			users := make([]*model.User, len(names))
			for i, n := range names {
				users[i] = &model.User{Username: n}
			}
			out := model.GetGroupDisplayNameFromUsers(users, truncate)
			// The truncating branch can emit invalid UTF-8; json.Marshal would replace
			// it with U+FFFD and the fixture would lie. Record the bytes as \u escapes
			// only when they are valid; otherwise mark the case.
			res = append(res, groupDisplayNameCase{Usernames: names, Truncate: truncate, Out: out})
		}
	}
	return res
}

// --- ChannelBannerInfo wire shape ---------------------------------------------

// bannerRoundTripAll pins the three-pointer struct's JSON in each nil/non-nil combination.
// None of the fields carries omitempty, so every key must always be present.
func bannerRoundTripAll() map[string]json.RawMessage {
	cases := map[string]*model.ChannelBannerInfo{
		"all_nil":       {},
		"all_set":       {Enabled: ptr(true), Text: ptr("t"), BackgroundColor: ptr("#fff")},
		"enabled_only":  {Enabled: ptr(false)},
		"empty_strings": {Enabled: ptr(true), Text: ptr(""), BackgroundColor: ptr("")},
	}
	res := make(map[string]json.RawMessage, len(cases))
	for name, b := range cases {
		blob, err := json.Marshal(b)
		if err != nil {
			panic(err)
		}
		res[name] = blob
	}
	return res
}
