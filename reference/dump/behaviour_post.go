package main

// Behavioural oracle for model/post.go — chunk 1, written to fixtures/behaviour_post.json.
//
// post.go is 1,640 lines and is being ported across several sessions. This oracle covers the
// wire type, the constants, `IsValid`, the pre-hooks, the props accessors and the predicate
// family. The interactive-payload validator (`propsIsValid`), `AllStrings`, `Attachments`,
// `RewriteImageURLs` and the reporting API are not covered here; they land with their chunks.
//
// The traps this corpus exists to catch, none of which survive a casual reading:
//
//  1. **`HasForceNotification` treats any non-empty *string* as true**, so props
//     `{"force_notification": "false"}` forces a notification. `HasSilentNotification` accepts
//     only a real bool. The two are asymmetric and both are recorded over the same corpus.
//
//  2. **`IsFromOAuthBot` compares an `any` against `""`.** An *absent* `override_username` is a
//     nil interface, and `nil != ""` is true in Go — so the second half of the conjunction is
//     satisfied by a prop that was never set.
//
//  3. **`DelProp` sizes its copy `len(o.Props)-1`.** On a nil or empty Props that is `make(map,
//     -1)`, which is a run-time panic. Probed under `recover`.
//
//  4. **`IsValid` measures the props by `StringInterfaceToJSON`**, i.e. Go's `encoding/json`
//     with HTML escaping, so one `<` costs six runes against the 800,000 cap. The marshallers
//     are pinned separately over a corpus.
//
//  5. **`ShallowCopy` deep-copies exactly one field** (`IsFollowing`) and aliases every other
//     reference — Props, FileIds, Participants, Metadata. Recorded as aliasing flags.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":                  postConstants(),
		"wire":                       postWireAll(),
		"is_valid":                   postIsValidAll(),
		"pre_save":                   postPreSaveAll(),
		"pre_commit":                 postPreCommitAll(),
		"props_accessors":            postPropsAccessorsAll(),
		"del_prop_nil_map":           postDelPropNilMap(),
		"sanitize_props":             postSanitizePropsAll(),
		"preserve_identity_props":    postPreserveIdentityPropsAll(),
		"sanitize_input":             postSanitizeInputAll(),
		"reserved_props":             postReservedPropsAll(),
		"notification_props":         postNotificationPropsAll(),
		"type_predicates":            postTypePredicatesAll(),
		"patch":                      postPatchAll(),
		"patch_is_empty":             postPatchIsEmptyAll(),
		"etag":                       postEtagAll(),
		"find_at_channel_mention":    findAtChannelMentionAll(),
		"disable_mention_highlights": postDisableMentionHighlightsAll(),
		"is_from_oauth_bot":          postIsFromOAuthBotAll(),
		"priority_accessors":         postPriorityAccessorsAll(),
		"misc_accessors":             postMiscAccessorsAll(),
		"clone":                      postCloneAll(),
		"array_to_json":              arrayToJSONAll(),
		"string_interface_to_json":   stringInterfaceToJSONAll(),
		"should_index":               fileForIndexingShouldIndexAll(),
		"sync_cursor_is_empty":       syncCursorIsEmptyAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post.json"), append(blob, '\n'), 0o644)
}

// --- constants ---------------------------------------------------------------------------

func postConstants() map[string]any {
	return map[string]any{
		"post_system_message_prefix":            model.PostSystemMessagePrefix,
		"post_type_default":                     model.PostTypeDefault,
		"post_type_message_attachment":          model.PostTypeMessageAttachment,
		"post_type_system_generic":              model.PostTypeSystemGeneric,
		"post_type_join_leave":                  model.PostTypeJoinLeave,
		"post_type_join_channel":                model.PostTypeJoinChannel,
		"post_type_guest_join_channel":          model.PostTypeGuestJoinChannel,
		"post_type_leave_channel":               model.PostTypeLeaveChannel,
		"post_type_join_team":                   model.PostTypeJoinTeam,
		"post_type_leave_team":                  model.PostTypeLeaveTeam,
		"post_type_auto_responder":              model.PostTypeAutoResponder,
		"post_type_autotranslation_change":      model.PostTypeAutotranslationChange,
		"post_type_add_remove":                  model.PostTypeAddRemove,
		"post_type_add_to_channel":              model.PostTypeAddToChannel,
		"post_type_add_guest_to_channel":        model.PostTypeAddGuestToChannel,
		"post_type_remove_from_channel":         model.PostTypeRemoveFromChannel,
		"post_type_move_channel":                model.PostTypeMoveChannel,
		"post_type_add_to_team":                 model.PostTypeAddToTeam,
		"post_type_remove_from_team":            model.PostTypeRemoveFromTeam,
		"post_type_access_control_team_removal": model.PostTypeAccessControlTeamRemoval,
		"post_type_access_control_team_add":     model.PostTypeAccessControlTeamAddition,
		"post_type_header_change":               model.PostTypeHeaderChange,
		"post_type_displayname_change":          model.PostTypeDisplaynameChange,
		"post_type_convert_channel":             model.PostTypeConvertChannel,
		"post_type_purpose_change":              model.PostTypePurposeChange,
		"post_type_channel_deleted":             model.PostTypeChannelDeleted,
		"post_type_channel_restored":            model.PostTypeChannelRestored,
		"post_type_ephemeral":                   model.PostTypeEphemeral,
		"post_type_change_channel_privacy":      model.PostTypeChangeChannelPrivacy,
		"post_type_wrangler":                    model.PostTypeWrangler,
		"post_type_gm_converted_to_channel":     model.PostTypeGMConvertedToChannel,
		"post_type_add_bot_teams_channels":      model.PostTypeAddBotTeamsChannels,
		"post_type_me":                          model.PostTypeMe,
		"post_custom_type_prefix":               model.PostCustomTypePrefix,
		"post_type_reminder":                    model.PostTypeReminder,
		"post_type_burn_on_read":                model.PostTypeBurnOnRead,
		"post_type_card":                        model.PostTypeCard,
		"post_type_shared_channel_state":        model.PostTypeSharedChannelState,

		"post_fileids_max_runes":    model.PostFileidsMaxRunes,
		"post_filenames_max_runes":  model.PostFilenamesMaxRunes,
		"post_hashtags_max_runes":   model.PostHashtagsMaxRunes,
		"post_message_max_runes_v1": model.PostMessageMaxRunesV1,
		"post_message_max_bytes_v2": model.PostMessageMaxBytesV2,
		"post_message_max_runes_v2": model.PostMessageMaxRunesV2,

		"max_reporting_per_page":         model.MaxReportingPerPage,
		"reporting_time_field_create_at": model.ReportingTimeFieldCreateAt,
		"reporting_time_field_update_at": model.ReportingTimeFieldUpdateAt,
		"reporting_sort_direction_asc":   model.ReportingSortDirectionAsc,
		"reporting_sort_direction_desc":  model.ReportingSortDirectionDesc,
		"post_props_max_runes":           model.PostPropsMaxRunes,
		"post_props_max_user_runes":      model.PostPropsMaxUserRunes,

		"props_add_channel_member":            model.PropsAddChannelMember,
		"post_props_added_user_id":            model.PostPropsAddedUserId,
		"post_props_delete_by":                model.PostPropsDeleteBy,
		"post_props_override_icon_url":        model.PostPropsOverrideIconURL,
		"post_props_override_icon_emoji":      model.PostPropsOverrideIconEmoji,
		"post_props_override_username":        model.PostPropsOverrideUsername,
		"post_props_from_webhook":             model.PostPropsFromWebhook,
		"post_props_from_bot":                 model.PostPropsFromBot,
		"post_props_from_oauth_app":           model.PostPropsFromOAuthApp,
		"post_props_webhook_display_name":     model.PostPropsWebhookDisplayName,
		"post_props_from_plugin":              model.PostPropsFromPlugin,
		"post_props_mention_highlight_disabl": model.PostPropsMentionHighlightDisabled,
		"post_props_group_highlight_disabled": model.PostPropsGroupHighlightDisabled,
		"post_props_previewed_post":           model.PostPropsPreviewedPost,
		"post_props_force_notification":       model.PostPropsForceNotification,
		"post_props_silent_notification":      model.PostPropsSilentNotification,
		"post_props_channel_mentions":         model.PostPropsChannelMentions,
		"post_props_current_team_id":          model.PostPropsCurrentTeamId,
		"post_props_unsafe_links":             model.PostPropsUnsafeLinks,
		"post_props_ai_generated_by_user_id":  model.PostPropsAIGeneratedByUserID,
		"post_props_ai_generated_by_username": model.PostPropsAIGeneratedByUsername,
		"post_props_expire_at":                model.PostPropsExpireAt,
		"post_props_read_duration_seconds":    model.PostPropsReadDurationSeconds,
		"post_props_shared_channel_state":     model.PostPropsSharedChannelState,
		"post_props_shared_channel_workspace": model.PostPropsSharedChannelWorkspaceName,

		"post_props_attachments":       model.PostPropsAttachments,
		"post_props_mm_blocks":         model.PostPropsMmBlocks,
		"post_props_block_kit_blocks":  model.PostPropsBlockKitBlocks,
		"post_props_adaptive_cards":    model.PostPropsAdaptiveCards,
		"post_props_mm_blocks_actions": model.PostPropsMmBlocksActions,

		"post_priority_urgent":          model.PostPriorityUrgent,
		"default_expiry_seconds":        model.DefaultExpirySeconds,
		"default_read_duration_seconds": model.DefaultReadDurationSeconds,
		"post_context_key_is_scheduled": string(model.PostContextKeyIsScheduledPost),
		"shared_channel_state_shared":   model.SharedChannelStatePostValueShared,
		"shared_channel_state_unshared": model.SharedChannelStatePostValueUnshared,
		"post_identity_props_on_update_n": len([]string{
			model.PostPropsSilentNotification,
			model.PostPropsFromBot,
			model.PostPropsFromWebhook,
			model.PostPropsFromOAuthApp,
			model.PostPropsFromPlugin,
		}),
	}
}

// --- wire --------------------------------------------------------------------------------

type postWireCase struct {
	Name string `json:"name"`
	JSON string `json:"json"`
	// Go's own unmarshal→marshal of JSON. Recorded because several fields are lossy in Go too.
	Roundtrip string `json:"roundtrip"`
}

func postWireAll() []postWireCase {
	empty := ""
	remote := "cluster-a"
	following := true
	notFollowing := false

	cases := []struct {
		name string
		p    model.Post
	}{
		{"zero", model.Post{}},
		// props has no omitempty, so a nil map is `null` and an empty map is `{}`.
		{"props_nil", model.Post{Id: idA}},
		{"props_empty", model.Post{Id: idA, Props: model.StringInterface{}}},
		{"props_set", model.Post{Id: idA, Props: model.StringInterface{"b": 1, "a": "x"}}},
		// file_ids has no omitempty either.
		{"file_ids_nil", model.Post{Id: idA}},
		{"file_ids_empty", model.Post{Id: idA, FileIds: model.StringArray{}}},
		{"file_ids_set", model.Post{Id: idA, FileIds: model.StringArray{idB, idC}}},
		// Filenames carries json:"-" and must never appear.
		{"filenames_set_is_off_the_wire", model.Post{Id: idA, Filenames: model.StringArray{"a.txt"}}},
		// participants has no omitempty.
		{"participants_nil", model.Post{Id: idA}},
		{"participants_empty", model.Post{Id: idA, Participants: []*model.User{}}},
		// message_source, has_reactions, remote_id, is_following, metadata all have omitempty.
		{"message_source_empty", model.Post{Id: idA, MessageSource: ""}},
		{"message_source_set", model.Post{Id: idA, MessageSource: "src"}},
		{"has_reactions_false", model.Post{Id: idA, HasReactions: false}},
		{"has_reactions_true", model.Post{Id: idA, HasReactions: true}},
		{"remote_id_nil", model.Post{Id: idA}},
		{"remote_id_empty_string", model.Post{Id: idA, RemoteId: &empty}},
		{"remote_id_set", model.Post{Id: idA, RemoteId: &remote}},
		{"is_following_nil", model.Post{Id: idA}},
		{"is_following_false", model.Post{Id: idA, IsFollowing: &notFollowing}},
		{"is_following_true", model.Post{Id: idA, IsFollowing: &following}},
		{"metadata_nil", model.Post{Id: idA}},
		{"metadata_empty", model.Post{Id: idA, Metadata: &model.PostMetadata{}}},
		{"metadata_priority", model.Post{Id: idA, Metadata: &model.PostMetadata{
			Priority: &model.PostPriority{PostId: idA},
		}}},
		{"type_custom", model.Post{Id: idA, Type: "custom_up_notification"}},
		// Go sorts map keys and HTML-escapes; both are visible here.
		{"props_html_escaped", model.Post{Id: idA, Props: model.StringInterface{
			"z": "<b>", "a": "a&b", "m": " ",
		}}},
	}

	res := make([]postWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.p)
		if err != nil {
			panic(err)
		}
		var back model.Post
		if err := json.Unmarshal(blob, &back); err != nil {
			panic(err)
		}
		rt, err := json.Marshal(&back)
		if err != nil {
			panic(err)
		}
		res = append(res, postWireCase{Name: c.name, JSON: string(blob), Roundtrip: string(rt)})
	}
	return res
}

// --- IsValid -----------------------------------------------------------------------------

type postValidCase struct {
	Name        string          `json:"name"`
	Post        json.RawMessage `json:"post"`
	MaxPostSize int             `json:"max_post_size"`
	// Filenames carries json:"-", so it cannot be recovered from the marshalled form.
	Filenames []string `json:"filenames"`
	ErrorID   string   `json:"error_id"`
	Detailed  string   `json:"detailed"`
}

func postIsValidAll() []postValidCase {
	type mut struct {
		name string
		max  int
		fn   func(p *model.Post)
	}
	const dflt = model.PostMessageMaxRunesV1

	muts := []mut{
		{"valid", dflt, func(p *model.Post) {}},

		{"id_empty", dflt, func(p *model.Post) { p.Id = "" }},
		{"id_short", dflt, func(p *model.Post) { p.Id = repeat("a", 25) }},
		{"id_long", dflt, func(p *model.Post) { p.Id = repeat("a", 27) }},

		{"create_at_zero", dflt, func(p *model.Post) { p.CreateAt = 0 }},
		{"create_at_negative", dflt, func(p *model.Post) { p.CreateAt = -1 }},
		{"update_at_zero", dflt, func(p *model.Post) { p.UpdateAt = 0 }},
		{"update_at_negative", dflt, func(p *model.Post) { p.UpdateAt = -1 }},
		// edit_at and delete_at are never checked.
		{"edit_at_negative", dflt, func(p *model.Post) { p.EditAt = -1 }},
		{"delete_at_negative", dflt, func(p *model.Post) { p.DeleteAt = -1 }},

		{"user_id_empty", dflt, func(p *model.Post) { p.UserId = "" }},
		{"user_id_nonsense", dflt, func(p *model.Post) { p.UserId = "nope" }},
		{"channel_id_empty", dflt, func(p *model.Post) { p.ChannelId = "" }},
		{"channel_id_nonsense", dflt, func(p *model.Post) { p.ChannelId = "nope" }},

		// RootId is optional but must be a real id when set.
		{"root_id_empty", dflt, func(p *model.Post) { p.RootId = "" }},
		{"root_id_nonsense", dflt, func(p *model.Post) { p.RootId = "nope" }},
		// OriginalId is only *length*-checked — 26 bytes of anything passes.
		{"original_id_empty", dflt, func(p *model.Post) { p.OriginalId = "" }},
		{"original_id_26_junk", dflt, func(p *model.Post) { p.OriginalId = repeat("!", 26) }},
		{"original_id_25", dflt, func(p *model.Post) { p.OriginalId = repeat("a", 25) }},
		{"original_id_13_two_byte", dflt, func(p *model.Post) { p.OriginalId = repeat("é", 13) }},

		// The message limit is runes, and the limit is a parameter.
		{"message_at_limit", 10, func(p *model.Post) { p.Message = repeat("a", 10) }},
		{"message_over_limit", 10, func(p *model.Post) { p.Message = repeat("a", 11) }},
		{"message_runes_not_bytes", 10, func(p *model.Post) { p.Message = repeat("é", 10) }},
		{"message_runes_over", 10, func(p *model.Post) { p.Message = repeat("é", 11) }},
		{"message_max_zero", 0, func(p *model.Post) { p.Message = "a" }},

		{"hashtags_at_limit", dflt, func(p *model.Post) { p.Hashtags = repeat("a", model.PostHashtagsMaxRunes) }},
		{"hashtags_over_limit", dflt, func(p *model.Post) { p.Hashtags = repeat("a", model.PostHashtagsMaxRunes+1) }},
		{"hashtags_runes_not_bytes", dflt, func(p *model.Post) { p.Hashtags = repeat("é", model.PostHashtagsMaxRunes) }},

		{"type_empty", dflt, func(p *model.Post) { p.Type = "" }},
		{"type_join_leave", dflt, func(p *model.Post) { p.Type = model.PostTypeJoinLeave }},
		{"type_shared_channel_state", dflt, func(p *model.Post) { p.Type = model.PostTypeSharedChannelState }},
		{"type_card", dflt, func(p *model.Post) { p.Type = model.PostTypeCard }},
		{"type_burn_on_read", dflt, func(p *model.Post) { p.Type = model.PostTypeBurnOnRead }},
		{"type_me", dflt, func(p *model.Post) { p.Type = model.PostTypeMe }},
		// Ephemeral is NOT in the accepted switch, despite being a declared post type.
		{"type_ephemeral", dflt, func(p *model.Post) { p.Type = model.PostTypeEphemeral }},
		{"type_custom_prefix", dflt, func(p *model.Post) { p.Type = "custom_anything" }},
		{"type_custom_prefix_bare", dflt, func(p *model.Post) { p.Type = model.PostCustomTypePrefix }},
		{"type_custom_wrong_case", dflt, func(p *model.Post) { p.Type = "Custom_x" }},
		{"type_unknown", dflt, func(p *model.Post) { p.Type = "system_nope" }},

		// Filenames is measured through ArrayToJSON — off the wire, still validated.
		{"filenames_nil", dflt, func(p *model.Post) { p.Filenames = nil }},
		{"filenames_at_limit", dflt, func(p *model.Post) {
			p.Filenames = model.StringArray{repeat("a", model.PostFilenamesMaxRunes-4)}
		}},
		{"filenames_over_limit", dflt, func(p *model.Post) {
			p.Filenames = model.StringArray{repeat("a", model.PostFilenamesMaxRunes-3)}
		}},
		{"file_ids_nil", dflt, func(p *model.Post) { p.FileIds = nil }},
		{"file_ids_at_limit", dflt, func(p *model.Post) {
			p.FileIds = model.StringArray{repeat("a", model.PostFileidsMaxRunes-4)}
		}},
		{"file_ids_over_limit", dflt, func(p *model.Post) {
			p.FileIds = model.StringArray{repeat("a", model.PostFileidsMaxRunes-3)}
		}},
		// The FileIds contents are never validated as ids.
		{"file_ids_nonsense", dflt, func(p *model.Post) { p.FileIds = model.StringArray{"nope"} }},

		{"props_nil", dflt, func(p *model.Post) { p.Props = nil }},
		{"props_empty", dflt, func(p *model.Post) { p.Props = model.StringInterface{} }},
		// The props cap is measured over Go's JSON, so escaping inflates the count.
		{"props_at_limit", dflt, func(p *model.Post) {
			// {"a":"<pad>"} is 8 runes of framing plus the value.
			p.Props = model.StringInterface{"a": repeat("x", model.PostPropsMaxRunes-8)}
		}},
		{"props_over_limit", dflt, func(p *model.Post) {
			p.Props = model.StringInterface{"a": repeat("x", model.PostPropsMaxRunes-7)}
		}},
	}

	res := make([]postValidCase, 0, len(muts))
	for _, m := range muts {
		remote := "cluster-a"
		p := &model.Post{
			Id:            idA,
			CreateAt:      1700000000000,
			UpdateAt:      1700000001000,
			EditAt:        1700000002000,
			UserId:        idB,
			ChannelId:     idC,
			RootId:        idB,
			OriginalId:    idC,
			Message:       "hello",
			Type:          model.PostTypeDefault,
			Hashtags:      "#tag",
			FileIds:       model.StringArray{idA},
			PendingPostId: idB,
			RemoteId:      &remote,
			Props:         model.StringInterface{"a": "b"},
		}
		m.fn(p)

		blob, err := json.Marshal(p)
		if err != nil {
			panic(err)
		}
		c := postValidCase{
			Name: m.name, Post: blob, MaxPostSize: m.max, Filenames: []string(p.Filenames),
		}
		if appErr := p.IsValid(m.max); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

// --- PreSave / PreCommit -----------------------------------------------------------------

// PreSave calls NewId and GetMillis, neither of which may be recorded (see D-032). What is
// recorded is the *invariants*: whether an id was minted, whether create_at was taken from the
// clock, and the fields whose values are fully determined by the input.
type postPreSaveCase struct {
	Name                   string `json:"name"`
	InID                   string `json:"in_id"`
	InCreateAt             int64  `json:"in_create_at"`
	InUpdateAt             int64  `json:"in_update_at"`
	InOriginalID           string `json:"in_original_id"`
	IDWasGenerated         bool   `json:"id_was_generated"`
	OutIDLen               int    `json:"out_id_len"`
	CreateAtFromClock      bool   `json:"create_at_from_clock"`
	OutCreateAt            int64  `json:"out_create_at"` // 0 when taken from the clock
	UpdateAtEqualsCreateAt bool   `json:"update_at_equals_create_at"`
	OutOriginalID          string `json:"out_original_id"`
	PropsNonNil            bool   `json:"props_non_nil"`
	FilenamesNonNil        bool   `json:"filenames_non_nil"`
	FileIDsNonNil          bool   `json:"file_ids_non_nil"`
}

func postPreSaveAll() []postPreSaveCase {
	cases := []struct {
		name string
		p    model.Post
	}{
		{"all_zero", model.Post{}},
		{"id_set", model.Post{Id: idA}},
		{"create_at_set", model.Post{Id: idA, CreateAt: 1700000000000}},
		{"create_at_negative_is_kept", model.Post{Id: idA, CreateAt: -5}},
		{"update_at_ahead_is_overwritten", model.Post{Id: idA, CreateAt: 100, UpdateAt: 999}},
		{"original_id_is_always_cleared", model.Post{Id: idA, CreateAt: 100, OriginalId: idB}},
	}

	res := make([]postPreSaveCase, 0, len(cases))
	for _, c := range cases {
		p := c.p
		inID, inCreateAt := p.Id, p.CreateAt
		p.PreSave()

		out := postPreSaveCase{
			Name:                   c.name,
			InID:                   inID,
			InCreateAt:             inCreateAt,
			InUpdateAt:             c.p.UpdateAt,
			InOriginalID:           c.p.OriginalId,
			IDWasGenerated:         inID == "",
			OutIDLen:               len(p.Id),
			CreateAtFromClock:      inCreateAt == 0,
			UpdateAtEqualsCreateAt: p.UpdateAt == p.CreateAt,
			OutOriginalID:          p.OriginalId,
			PropsNonNil:            p.GetProps() != nil,
			FilenamesNonNil:        p.Filenames != nil,
			FileIDsNonNil:          p.FileIds != nil,
		}
		if inCreateAt != 0 {
			out.OutCreateAt = p.CreateAt
		}
		res = append(res, out)
	}
	return res
}

type postPreCommitCase struct {
	Name         string          `json:"name"`
	InFileIDs    []string        `json:"in_file_ids"`
	OutProps     json.RawMessage `json:"out_props"`
	OutFilenames []string        `json:"out_filenames"`
	OutFileIDs   []string        `json:"out_file_ids"`
}

func postPreCommitAll() []postPreCommitCase {
	cases := []struct {
		name string
		p    model.Post
	}{
		{"all_nil", model.Post{}},
		{"props_set_is_kept", model.Post{Props: model.StringInterface{"a": "b"}}},
		{"file_ids_deduped", model.Post{FileIds: model.StringArray{idA, idB, idA, idC, idB}}},
		{"file_ids_single", model.Post{FileIds: model.StringArray{idA}}},
		{"file_ids_all_same", model.Post{FileIds: model.StringArray{idA, idA, idA}}},
		{"file_ids_empty_strings", model.Post{FileIds: model.StringArray{"", "", idA}}},
		{"filenames_set_is_kept", model.Post{Filenames: model.StringArray{"a.txt"}}},
	}

	res := make([]postPreCommitCase, 0, len(cases))
	for _, c := range cases {
		p := c.p
		in := append([]string(nil), c.p.FileIds...)
		p.PreCommit()
		props, err := json.Marshal(p.GetProps())
		if err != nil {
			panic(err)
		}
		res = append(res, postPreCommitCase{
			Name:         c.name,
			InFileIDs:    in,
			OutProps:     props,
			OutFilenames: []string(p.Filenames),
			OutFileIDs:   []string(p.FileIds),
		})
	}
	return res
}

// --- props accessors ----------------------------------------------------------------------

type postPropsAccessorCase struct {
	Name     string          `json:"name"`
	InProps  json.RawMessage `json:"in_props"`
	Op       string          `json:"op"`
	Key      string          `json:"key"`
	Value    json.RawMessage `json:"value"`
	OutProps json.RawMessage `json:"out_props"`
	// GetProp returns an untyped nil for a missing key, which is not the same as a stored null.
	GetResult json.RawMessage `json:"get_result"`
	GetWasNil bool            `json:"get_was_nil"`
	AliasedIn bool            `json:"aliased_in"` // did the mutation write through to the input map?
}

func postPropsAccessorsAll() []postPropsAccessorCase {
	mustJSON := func(v any) json.RawMessage {
		b, err := json.Marshal(v)
		if err != nil {
			panic(err)
		}
		return b
	}

	cases := []struct {
		name  string
		props model.StringInterface
		op    string
		key   string
		value any
	}{
		{"add_to_empty", model.StringInterface{}, "add", "a", "b"},
		{"add_to_nil", nil, "add", "a", "b"},
		{"add_overwrites", model.StringInterface{"a": "old"}, "add", "a", "new"},
		{"add_bool", model.StringInterface{}, "add", "a", true},
		{"add_null", model.StringInterface{}, "add", "a", nil},
		{"del_present", model.StringInterface{"a": "b", "c": "d"}, "del", "a", nil},
		{"del_absent", model.StringInterface{"a": "b", "c": "d"}, "del", "z", nil},
		{"get_present", model.StringInterface{"a": "b"}, "get", "a", nil},
		{"get_absent", model.StringInterface{"a": "b"}, "get", "z", nil},
		{"get_stored_null", model.StringInterface{"a": nil}, "get", "a", nil},
		{"get_from_nil_map", nil, "get", "a", nil},
	}

	res := make([]postPropsAccessorCase, 0, len(cases))
	for _, c := range cases {
		p := &model.Post{Props: c.props}
		out := postPropsAccessorCase{
			Name: c.name, InProps: mustJSON(c.props), Op: c.op, Key: c.key, Value: mustJSON(c.value),
		}
		switch c.op {
		case "add":
			p.AddProp(c.key, c.value)
		case "del":
			p.DelProp(c.key)
		case "get":
			v := p.GetProp(c.key)
			out.GetResult = mustJSON(v)
			out.GetWasNil = v == nil
		}
		out.OutProps = mustJSON(p.GetProps())
		// AddProp/DelProp build a fresh map and swap it in, so the caller's map is never
		// written through. Compare the input map against the snapshot taken before the call:
		// equal means the mutation went only to the copy.
		if c.props != nil && c.op != "get" {
			out.AliasedIn = string(mustJSON(c.props)) != string(out.InProps)
		}
		res = append(res, out)
	}
	return res
}

// DelProp sizes its copy `make(map[string]any, len(o.Props)-1)`. On an empty or nil map that is
// a negative size hint, which Go panics on. Probed rather than reasoned about.
func postDelPropNilMap() map[string]any {
	probe := func(props model.StringInterface) (panicked bool, msg string) {
		defer func() {
			if r := recover(); r != nil {
				panicked = true
				if err, ok := r.(error); ok {
					msg = err.Error()
				} else {
					msg = "non-error panic"
				}
			}
		}()
		p := &model.Post{Props: props}
		p.DelProp("a")
		return false, ""
	}

	nilPanicked, nilMsg := probe(nil)
	emptyPanicked, emptyMsg := probe(model.StringInterface{})
	onePanicked, oneMsg := probe(model.StringInterface{"a": "b"})

	return map[string]any{
		"nil_map_panicked":   nilPanicked,
		"nil_map_message":    nilMsg,
		"empty_map_panicked": emptyPanicked,
		"empty_map_message":  emptyMsg,
		"one_entry_panicked": onePanicked,
		"one_entry_message":  oneMsg,
	}
}

// --- SanitizeProps -------------------------------------------------------------------------

type postSanitizeCase struct {
	Name                    string          `json:"name"`
	InProps                 json.RawMessage `json:"in_props"`
	RemoteID                *string         `json:"remote_id"`
	OutProps                json.RawMessage `json:"out_props"`
	OutParticipantPasswords []string        `json:"out_participant_passwords"`
}

func postSanitizePropsAll() []postSanitizeCase {
	empty := ""
	remote := "cluster-a"

	cases := []struct {
		name     string
		props    model.StringInterface
		remoteID *string
	}{
		{"empty_props", model.StringInterface{"x": "y"}, nil},
		{"add_channel_member_stripped", model.StringInterface{model.PropsAddChannelMember: "v", "x": "y"}, nil},
		{"notification_markers_stripped_when_local", model.StringInterface{
			model.PostPropsForceNotification:  true,
			model.PostPropsSilentNotification: true,
			"x":                               "y",
		}, nil},
		{"notification_markers_stripped_when_remote_id_empty", model.StringInterface{
			model.PostPropsForceNotification:  true,
			model.PostPropsSilentNotification: true,
		}, &empty},
		// A federated post keeps them: the origin cluster already enforced authority.
		{"notification_markers_kept_when_federated", model.StringInterface{
			model.PostPropsForceNotification:  true,
			model.PostPropsSilentNotification: true,
			model.PropsAddChannelMember:       "v",
		}, &remote},
		// from_* identity markers are not stripped.
		{"from_markers_kept", model.StringInterface{
			model.PostPropsFromWebhook:  "true",
			model.PostPropsFromBot:      "true",
			model.PostPropsFromPlugin:   "true",
			model.PostPropsFromOAuthApp: "true",
		}, nil},
	}

	res := make([]postSanitizeCase, 0, len(cases))
	for _, c := range cases {
		props := model.StringInterface{}
		for k, v := range c.props {
			props[k] = v
		}
		p := &model.Post{
			Props:    props,
			RemoteId: c.remoteID,
			Participants: []*model.User{
				{Id: idA, Password: "hunter2", MfaSecret: "s3cret"},
			},
		}
		p.SanitizeProps()

		blob, err := json.Marshal(p.GetProps())
		if err != nil {
			panic(err)
		}
		inBlob, err := json.Marshal(c.props)
		if err != nil {
			panic(err)
		}
		pw := make([]string, 0, len(p.Participants))
		for _, u := range p.Participants {
			pw = append(pw, u.Password)
		}
		res = append(res, postSanitizeCase{
			Name: c.name, InProps: inBlob, RemoteID: c.remoteID,
			OutProps: blob, OutParticipantPasswords: pw,
		})
	}
	return res
}

type postPreserveCase struct {
	Name     string          `json:"name"`
	OldProps json.RawMessage `json:"old_props"`
	NewProps json.RawMessage `json:"new_props"`
	OutProps json.RawMessage `json:"out_props"`
}

func postPreserveIdentityPropsAll() []postPreserveCase {
	cases := []struct {
		name     string
		old, new model.StringInterface
	}{
		{"nothing_to_preserve", model.StringInterface{"x": "y"}, model.StringInterface{"a": "b"}},
		{"all_five_preserved", model.StringInterface{
			model.PostPropsSilentNotification: true,
			model.PostPropsFromBot:            "true",
			model.PostPropsFromWebhook:        "true",
			model.PostPropsFromOAuthApp:       "true",
			model.PostPropsFromPlugin:         "true",
		}, model.StringInterface{"a": "b"}},
		// force_notification is deliberately NOT in the preserved set.
		{"force_notification_not_preserved", model.StringInterface{
			model.PostPropsForceNotification: true,
		}, model.StringInterface{"a": "b"}},
		{"old_overwrites_new", model.StringInterface{
			model.PostPropsFromBot: "old",
		}, model.StringInterface{model.PostPropsFromBot: "new"}},
		// A stored explicit null is nil in Go, so GetProp returns nil and it is NOT preserved.
		{"stored_null_is_not_preserved", model.StringInterface{
			model.PostPropsFromBot: nil,
		}, model.StringInterface{"a": "b"}},
		{"old_props_nil", nil, model.StringInterface{"a": "b"}},
	}

	res := make([]postPreserveCase, 0, len(cases))
	for _, c := range cases {
		newProps := model.StringInterface{}
		for k, v := range c.new {
			newProps[k] = v
		}
		p := &model.Post{Props: newProps}
		old := &model.Post{Props: c.old}
		p.PreserveIdentityPropsFrom(old)

		oldBlob, _ := json.Marshal(c.old)
		newBlob, _ := json.Marshal(c.new)
		outBlob, err := json.Marshal(p.GetProps())
		if err != nil {
			panic(err)
		}
		res = append(res, postPreserveCase{
			Name: c.name, OldProps: oldBlob, NewProps: newBlob, OutProps: outBlob,
		})
	}
	return res
}

type postSanitizeInputCase struct {
	Name           string  `json:"name"`
	OutDeleteAt    int64   `json:"out_delete_at"`
	OutRemoteID    *string `json:"out_remote_id"`
	OutEmbedsNil   bool    `json:"out_embeds_nil"`
	OutMetadataNil bool    `json:"out_metadata_nil"`
}

func postSanitizeInputAll() []postSanitizeInputCase {
	remote := "cluster-a"
	cases := []struct {
		name string
		p    model.Post
	}{
		{"zero", model.Post{}},
		{"delete_at_and_remote_id_cleared", model.Post{DeleteAt: 12345, RemoteId: &remote}},
		{"metadata_nil_stays_nil", model.Post{DeleteAt: 1}},
		{"embeds_cleared", model.Post{Metadata: &model.PostMetadata{
			Embeds: []*model.PostEmbed{{Type: model.PostEmbedLink}},
		}}},
	}

	res := make([]postSanitizeInputCase, 0, len(cases))
	for _, c := range cases {
		p := c.p
		p.SanitizeInput()
		out := postSanitizeInputCase{
			Name: c.name, OutDeleteAt: p.DeleteAt, OutRemoteID: p.RemoteId,
			OutMetadataNil: p.Metadata == nil,
		}
		out.OutEmbedsNil = p.Metadata == nil || p.Metadata.Embeds == nil
		res = append(res, out)
	}
	return res
}

// --- reserved props -------------------------------------------------------------------------

type postReservedCase struct {
	Name  string          `json:"name"`
	Props json.RawMessage `json:"props"`
	// Order is the declaration order of reservedProps, not the map's.
	Found         []string `json:"found"`
	PatchFound    []string `json:"patch_found"`
	PatchNilFound []string `json:"patch_nil_found"`
}

func postReservedPropsAll() []postReservedCase {
	cases := []struct {
		name  string
		props model.StringInterface
	}{
		{"nil", nil},
		{"empty", model.StringInterface{}},
		{"none_reserved", model.StringInterface{"a": "b"}},
		{"one", model.StringInterface{model.PostPropsFromWebhook: "true"}},
		// Insertion order must not leak: the result follows reservedProps' declaration order.
		{"all_nine_reversed_insertion", model.StringInterface{
			model.PostPropsMmBlocksActions:    "z",
			model.PostPropsOverrideIconEmoji:  "z",
			model.PostPropsOverrideIconURL:    "z",
			model.PostPropsWebhookDisplayName: "z",
			model.PostPropsOverrideUsername:   "z",
			model.PostPropsForceNotification:  "z",
			model.PostPropsSilentNotification: "z",
			model.PostPropsFromPlugin:         "z",
			model.PostPropsFromWebhook:        "z",
		}},
		// A key present with a null value still counts — membership, not truthiness.
		{"null_value_still_found", model.StringInterface{model.PostPropsFromPlugin: nil}},
		// from_bot and from_oauth_app are NOT reserved.
		{"from_bot_is_not_reserved", model.StringInterface{
			model.PostPropsFromBot: "true", model.PostPropsFromOAuthApp: "true",
		}},
	}

	res := make([]postReservedCase, 0, len(cases))
	for _, c := range cases {
		blob, _ := json.Marshal(c.props)
		p := &model.Post{Props: c.props}
		props := c.props
		patch := &model.PostPatch{Props: &props}
		var nilPatch *model.PostPatch
		res = append(res, postReservedCase{
			Name:          c.name,
			Props:         blob,
			Found:         p.ContainsIntegrationsReservedProps(),
			PatchFound:    patch.ContainsIntegrationsReservedProps(),
			PatchNilFound: nilPatch.ContainsIntegrationsReservedProps(),
		})
	}
	return res
}

// --- notification predicates ------------------------------------------------------------------

type postNotificationCase struct {
	Name  string          `json:"name"`
	Props json.RawMessage `json:"props"`

	HasForce       bool `json:"has_force"`
	HasSilent      bool `json:"has_silent"`
	Suppressed     bool `json:"suppressed"`
	ExcludesCount  bool `json:"excludes_count"`
	HasUnsafeLinks bool `json:"has_unsafe_links"`
}

func postNotificationPropsAll() []postNotificationCase {
	both := func(v any) model.StringInterface {
		return model.StringInterface{
			model.PostPropsForceNotification:  v,
			model.PostPropsSilentNotification: v,
			model.PostPropsUnsafeLinks:        v,
		}
	}

	cases := []struct {
		name  string
		props model.StringInterface
	}{
		{"absent", model.StringInterface{}},
		{"nil_map", nil},
		{"stored_null", both(nil)},
		{"bool_true", both(true)},
		{"bool_false", both(false)},
		{"string_true", both("true")},
		// The trap: HasForceNotification accepts any non-empty string, "false" included.
		{"string_false", both("false")},
		{"string_empty", both("")},
		{"string_junk", both("junk")},
		{"number_one", both(float64(1))},
		{"number_zero", both(float64(0))},
		{"array", both([]any{})},
		{"object", both(map[string]any{})},
		// Force beats silent.
		{"force_bool_true_silent_bool_true", model.StringInterface{
			model.PostPropsForceNotification: true, model.PostPropsSilentNotification: true,
		}},
		{"force_string_false_silent_bool_true", model.StringInterface{
			model.PostPropsForceNotification: "false", model.PostPropsSilentNotification: true,
		}},
		{"force_bool_false_silent_bool_true", model.StringInterface{
			model.PostPropsForceNotification: false, model.PostPropsSilentNotification: true,
		}},
		{"silent_only", model.StringInterface{model.PostPropsSilentNotification: true}},
	}

	res := make([]postNotificationCase, 0, len(cases))
	for _, c := range cases {
		blob, _ := json.Marshal(c.props)
		p := &model.Post{Props: c.props}
		res = append(res, postNotificationCase{
			Name:           c.name,
			Props:          blob,
			HasForce:       p.HasForceNotification(),
			HasSilent:      p.HasSilentNotification(),
			Suppressed:     p.IsNotificationSuppressed(),
			ExcludesCount:  p.ExcludesFromChannelMessageCount(),
			HasUnsafeLinks: p.HasUnsafeLinks(),
		})
	}
	return res
}

// --- type predicates ---------------------------------------------------------------------------

type postTypePredicateCase struct {
	Type            string `json:"type"`
	IsSystemMessage bool   `json:"is_system_message"`
	IsJoinLeave     bool   `json:"is_join_leave"`
	IsACLMembership bool   `json:"is_acl_membership_notification"`
	ExcludesCount   bool   `json:"excludes_count"`
}

func postTypePredicatesAll() []postTypePredicateCase {
	types := []string{
		"", "system_", "system", "syste", "custom_x", "me",
		model.PostTypeSystemGeneric,
		model.PostTypeJoinLeave,
		model.PostTypeAddRemove,
		model.PostTypeJoinChannel,
		model.PostTypeLeaveChannel,
		model.PostTypeJoinTeam,
		model.PostTypeLeaveTeam,
		model.PostTypeAddToChannel,
		model.PostTypeRemoveFromChannel,
		model.PostTypeAddToTeam,
		model.PostTypeRemoveFromTeam,
		model.PostTypeGuestJoinChannel,
		model.PostTypeAddGuestToChannel,
		model.PostTypeAccessControlTeamRemoval,
		model.PostTypeAccessControlTeamAddition,
		model.PostTypeEphemeral,
		model.PostTypeReminder,
		model.PostTypeBurnOnRead,
		model.PostTypeCard,
		"System_Join_Channel",
	}

	res := make([]postTypePredicateCase, 0, len(types))
	for _, t := range types {
		p := &model.Post{Type: t}
		res = append(res, postTypePredicateCase{
			Type:            t,
			IsSystemMessage: p.IsSystemMessage(),
			IsJoinLeave:     p.IsJoinLeaveMessage(),
			IsACLMembership: p.IsAccessControlTeamMembershipNotification(),
			ExcludesCount:   p.ExcludesFromChannelMessageCount(),
		})
	}
	return res
}

// --- Patch --------------------------------------------------------------------------------------

type postPatchCase struct {
	Name  string          `json:"name"`
	Patch json.RawMessage `json:"patch"`
	Out   json.RawMessage `json:"out"`
}

func postPatchAll() []postPatchCase {
	pinned := true
	msg := "patched"
	emptyMsg := ""
	props := model.StringInterface{"new": "props"}
	emptyProps := model.StringInterface{}
	files := model.StringArray{idC}
	emptyFiles := model.StringArray{}
	reactions := false

	cases := []struct {
		name  string
		patch model.PostPatch
	}{
		{"empty_patch_changes_nothing", model.PostPatch{}},
		{"is_pinned", model.PostPatch{IsPinned: &pinned}},
		// Patch does not trim the message, unlike Channel::Patch's display_name.
		{"message", model.PostPatch{Message: &msg}},
		{"message_empty", model.PostPatch{Message: &emptyMsg}},
		// Props are replaced wholesale, not merged.
		{"props_replace", model.PostPatch{Props: &props}},
		{"props_empty_clears", model.PostPatch{Props: &emptyProps}},
		{"file_ids_replace", model.PostPatch{FileIds: &files}},
		{"file_ids_empty_clears", model.PostPatch{FileIds: &emptyFiles}},
		{"has_reactions_false", model.PostPatch{HasReactions: &reactions}},
		{"all_at_once", model.PostPatch{
			IsPinned: &pinned, Message: &msg, Props: &props,
			FileIds: &files, HasReactions: &reactions,
		}},
	}

	res := make([]postPatchCase, 0, len(cases))
	for _, c := range cases {
		p := &model.Post{
			Id: idA, Message: "  original  ", IsPinned: false, HasReactions: true,
			Props: model.StringInterface{"old": "props"}, FileIds: model.StringArray{idA, idB},
		}
		patch := c.patch
		p.Patch(&patch)

		pb, _ := json.Marshal(c.patch)
		ob, err := json.Marshal(p)
		if err != nil {
			panic(err)
		}
		res = append(res, postPatchCase{Name: c.name, Patch: pb, Out: ob})
	}
	return res
}

func postPatchIsEmptyAll() []map[string]any {
	pinned := false
	msg := ""
	props := model.StringInterface{}
	files := model.StringArray{}
	reactions := false

	cases := []struct {
		name  string
		patch model.PostPatch
	}{
		{"all_nil", model.PostPatch{}},
		// A pointer to a zero value is still "set".
		{"is_pinned_false", model.PostPatch{IsPinned: &pinned}},
		{"message_empty", model.PostPatch{Message: &msg}},
		{"props_empty", model.PostPatch{Props: &props}},
		{"file_ids_empty", model.PostPatch{FileIds: &files}},
		{"has_reactions_false", model.PostPatch{HasReactions: &reactions}},
	}

	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		patch := c.patch
		blob, _ := json.Marshal(c.patch)
		res = append(res, map[string]any{
			"name":     c.name,
			"patch":    json.RawMessage(blob),
			"is_empty": patch.IsEmpty(),
		})
	}
	return res
}

// --- Etag ---------------------------------------------------------------------------------------

func postEtagAll() []map[string]any {
	cases := []struct {
		name string
		p    model.Post
	}{
		{"zero", model.Post{}},
		{"normal", model.Post{Id: idA, UpdateAt: 1700000000000}},
		{"negative_update_at", model.Post{Id: idA, UpdateAt: -1}},
		{"id_with_dot", model.Post{Id: "a.b", UpdateAt: 7}},
	}
	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		p := c.p
		res = append(res, map[string]any{
			"name": c.name, "id": c.p.Id, "update_at": c.p.UpdateAt, "etag": p.Etag(),
		})
	}
	return res
}

// --- mention helpers ------------------------------------------------------------------------------

func findAtChannelMentionAll() []map[string]any {
	inputs := []string{
		"",
		"hello",
		"@channel",
		"hey @channel!",
		"@all",
		"@here",
		"@CHANNEL",
		"@Channel",
		"@ChAnNeL",
		// \B before @ means the @ must not follow a word boundary start — i.e. it needs a
		// non-word character (or nothing) before it... measured, not reasoned.
		"a@channel",
		"a @channel",
		"-@channel",
		"@channels",
		"@channel.",
		"@channel-",
		"@channel_",
		"@chan",
		"email@channel.com",
		"first @here then @all",
		"first @all then @here",
		"@herex",
		"@here@all",
		"```@channel```",
		"\n@channel",
		"@ channel",
	}
	res := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		p := &model.Post{Message: in}
		mention := p.DisableMentionHighlights()
		res = append(res, map[string]any{
			"input":   in,
			"mention": mention,
			"found":   mention != "",
		})
	}
	return res
}

type postDisableMentionCase struct {
	Name       string          `json:"name"`
	Message    string          `json:"message"`
	InProps    json.RawMessage `json:"in_props"`
	Mention    string          `json:"mention"`
	OutProps   json.RawMessage `json:"out_props"`
	PatchProps json.RawMessage `json:"patch_props"`
}

func postDisableMentionHighlightsAll() []postDisableMentionCase {
	cases := []struct {
		name    string
		message string
		props   model.StringInterface
	}{
		{"no_mention_leaves_props_alone", "hello", model.StringInterface{"a": "b"}},
		{"mention_sets_flag", "hey @channel", model.StringInterface{"a": "b"}},
		{"mention_on_nil_props", "hey @all", nil},
		{"mention_uppercased_is_lowered", "hey @HERE", model.StringInterface{}},
	}

	res := make([]postDisableMentionCase, 0, len(cases))
	for _, c := range cases {
		p := &model.Post{Message: c.message, Props: c.props}
		inBlob, _ := json.Marshal(c.props)
		mention := p.DisableMentionHighlights()
		outBlob, _ := json.Marshal(p.GetProps())

		msg := c.message
		patch := &model.PostPatch{Message: &msg}
		patch.DisableMentionHighlights()
		var patchBlob json.RawMessage
		if patch.Props == nil {
			patchBlob = json.RawMessage("null")
		} else {
			patchBlob, _ = json.Marshal(*patch.Props)
		}

		res = append(res, postDisableMentionCase{
			Name: c.name, Message: c.message, InProps: inBlob,
			Mention: mention, OutProps: outBlob, PatchProps: patchBlob,
		})
	}
	return res
}

// --- IsFromOAuthBot -------------------------------------------------------------------------------

func postIsFromOAuthBotAll() []map[string]any {
	cases := []struct {
		name  string
		props model.StringInterface
	}{
		{"nil_props", nil},
		{"empty_props", model.StringInterface{}},
		// The trap: an absent override_username is a nil `any`, and nil != "" is true.
		{"webhook_true_username_absent", model.StringInterface{model.PostPropsFromWebhook: "true"}},
		{"webhook_true_username_empty", model.StringInterface{
			model.PostPropsFromWebhook: "true", model.PostPropsOverrideUsername: "",
		}},
		{"webhook_true_username_set", model.StringInterface{
			model.PostPropsFromWebhook: "true", model.PostPropsOverrideUsername: "bot",
		}},
		// A real bool does not equal the string "true".
		{"webhook_bool_true", model.StringInterface{
			model.PostPropsFromWebhook: true, model.PostPropsOverrideUsername: "bot",
		}},
		{"webhook_false", model.StringInterface{
			model.PostPropsFromWebhook: "false", model.PostPropsOverrideUsername: "bot",
		}},
		{"webhook_absent_username_set", model.StringInterface{model.PostPropsOverrideUsername: "bot"}},
		// A stored null username is a nil `any` too.
		{"webhook_true_username_null", model.StringInterface{
			model.PostPropsFromWebhook: "true", model.PostPropsOverrideUsername: nil,
		}},
	}

	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		blob, _ := json.Marshal(c.props)
		p := &model.Post{Props: c.props}
		res = append(res, map[string]any{
			"name": c.name, "props": json.RawMessage(blob), "is_from_oauth_bot": p.IsFromOAuthBot(),
		})
	}
	return res
}

// --- priority accessors ----------------------------------------------------------------------------

type postPriorityAccessorCase struct {
	Name                    string          `json:"name"`
	Post                    json.RawMessage `json:"post"`
	PriorityNil             bool            `json:"priority_nil"`
	Priority                json.RawMessage `json:"priority"`
	PersistentNotifications *bool           `json:"persistent_notifications"`
	RequestedAck            *bool           `json:"requested_ack"`
	IsUrgent                bool            `json:"is_urgent"`
	PreviewedPostProp       string          `json:"previewed_post_prop"`
}

func postPriorityAccessorsAll() []postPriorityAccessorCase {
	urgent := model.PostPriorityUrgent
	important := "important"
	yes := true
	no := false

	cases := []struct {
		name string
		p    model.Post
	}{
		{"metadata_nil", model.Post{Id: idA}},
		{"metadata_without_priority", model.Post{Id: idA, Metadata: &model.PostMetadata{}}},
		{"priority_all_nil", model.Post{Id: idA, Metadata: &model.PostMetadata{
			Priority: &model.PostPriority{},
		}}},
		{"priority_urgent", model.Post{Id: idA, Metadata: &model.PostMetadata{
			Priority: &model.PostPriority{Priority: &urgent},
		}}},
		{"priority_important_is_not_urgent", model.Post{Id: idA, Metadata: &model.PostMetadata{
			Priority: &model.PostPriority{Priority: &important},
		}}},
		{"ack_and_persistent", model.Post{Id: idA, Metadata: &model.PostMetadata{
			Priority: &model.PostPriority{
				Priority: &urgent, RequestedAck: &yes, PersistentNotifications: &no,
			},
		}}},
		{"previewed_post_prop", model.Post{Id: idA, Props: model.StringInterface{
			model.PostPropsPreviewedPost: idB,
		}}},
		// A non-string previewed_post prop falls through the type assertion.
		{"previewed_post_prop_wrong_type", model.Post{Id: idA, Props: model.StringInterface{
			model.PostPropsPreviewedPost: 42,
		}}},
	}

	res := make([]postPriorityAccessorCase, 0, len(cases))
	for _, c := range cases {
		p := c.p
		blob, err := json.Marshal(&p)
		if err != nil {
			panic(err)
		}
		out := postPriorityAccessorCase{
			Name:                    c.name,
			Post:                    blob,
			PriorityNil:             p.GetPriority() == nil,
			PersistentNotifications: p.GetPersistentNotification(),
			RequestedAck:            p.GetRequestedAck(),
			IsUrgent:                p.IsUrgent(),
			PreviewedPostProp:       p.GetPreviewedPostProp(),
		}
		pb, err := json.Marshal(p.GetPriority())
		if err != nil {
			panic(err)
		}
		out.Priority = pb
		res = append(res, out)
	}
	return res
}

// --- misc ----------------------------------------------------------------------------------------

func postMiscAccessorsAll() []map[string]any {
	empty := ""
	remote := "cluster-a"
	cases := []struct {
		name string
		p    model.Post
	}{
		{"zero", model.Post{}},
		{"remote_id_nil", model.Post{Id: idA}},
		{"remote_id_empty", model.Post{Id: idA, RemoteId: &empty}},
		{"remote_id_set", model.Post{Id: idA, RemoteId: &remote}},
	}
	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		p := c.p
		clean := (&model.Post{
			Id: idA, CreateAt: 1, UpdateAt: 2, EditAt: 3, DeleteAt: 4, Message: "keep",
		}).CleanPost()
		cleanBlob, _ := json.Marshal(clean)
		res = append(res, map[string]any{
			"name":              c.name,
			"is_remote":         p.IsRemote(),
			"remote_id":         p.GetRemoteID(),
			"to_nil_if_invalid": p.ToNilIfInvalid() == nil,
			"clean_post":        json.RawMessage(cleanBlob),
		})
	}
	return res
}

// ShallowCopy deep-copies exactly one field. Everything else that is a reference is aliased.
func postCloneAll() map[string]any {
	following := true
	remote := "cluster-a"
	orig := &model.Post{
		Id:           idA,
		Message:      "hello",
		Props:        model.StringInterface{"a": "b"},
		FileIds:      model.StringArray{idB},
		Filenames:    model.StringArray{"a.txt"},
		Participants: []*model.User{{Id: idC}},
		Metadata:     &model.PostMetadata{RedactedFileCount: 3},
		IsFollowing:  &following,
		RemoteId:     &remote,
	}
	clone := orig.Clone()

	// Mutate the clone's shared references and observe the original.
	clone.Props["a"] = "mutated"
	clone.FileIds[0] = "mutated"
	clone.Metadata.RedactedFileCount = 99
	*clone.IsFollowing = false

	origBlob, _ := json.Marshal(orig)
	cloneBlob, _ := json.Marshal(clone)

	var nilDst *model.Post
	err := orig.ShallowCopy(nilDst)

	return map[string]any{
		"orig_after_clone_mutation":  json.RawMessage(origBlob),
		"clone_after_mutation":       json.RawMessage(cloneBlob),
		"props_aliased":              orig.Props["a"] == "mutated",
		"file_ids_aliased":           orig.FileIds[0] == "mutated",
		"metadata_aliased":           orig.Metadata.RedactedFileCount == 99,
		"is_following_deep_copied":   *orig.IsFollowing == true,
		"remote_id_pointer_aliased":  orig.RemoteId == clone.RemoteId,
		"shallow_copy_nil_dst_error": err != nil,
		"shallow_copy_nil_dst_msg":   errString(err),
	}
}

func errString(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

// --- the marshallers IsValid measures ---------------------------------------------------------------

func arrayToJSONAll() []map[string]any {
	inputs := [][]string{
		nil,
		{},
		{""},
		{"a"},
		{"a", "b"},
		{"<b>"},
		{"a&b"},
		{"\u2028\u2029"},
		{"a\"b"},
		{"a\\b"},
		{"a\tb\nc"},
		{"\x00"},
		{"é"},
		{"😀"},
	}
	res := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		s := model.ArrayToJSON(in)
		res = append(res, map[string]any{
			"input": in, "json": s, "rune_count": len([]rune(s)),
		})
	}
	return res
}

func stringInterfaceToJSONAll() []map[string]any {
	inputs := []map[string]any{
		nil,
		{},
		{"a": "b"},
		// Go sorts map keys by byte value.
		{"z": 1, "a": 2, "M": 3, "_": 4},
		{"a": "<b>"},
		{"a": "x&y"},
		{"a": " "},
		{"a": nil},
		{"a": true},
		{"a": float64(1)},
		{"a": float64(1.5)},
		{"a": []any{"x", float64(1)}},
		{"a": map[string]any{"n": "m"}},
		{"<k>": "v"},
		{"é": "😀"},
	}
	res := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		s := model.StringInterfaceToJSON(in)
		blob, _ := json.Marshal(in)
		res = append(res, map[string]any{
			"input": json.RawMessage(blob), "json": s, "rune_count": len([]rune(s)),
		})
	}
	return res
}

// --- small helpers on neighbouring types -------------------------------------------------------------

func fileForIndexingShouldIndexAll() []map[string]any {
	cases := []struct {
		name string
		f    *model.FileForIndexing
	}{
		{"nil", nil},
		{"zero", &model.FileForIndexing{}},
		{"deleted", &model.FileForIndexing{FileInfo: model.FileInfo{DeleteAt: 1, PostId: idA}}},
		{"has_post_id", &model.FileForIndexing{FileInfo: model.FileInfo{PostId: idA}}},
		{"bookmark_owner", &model.FileForIndexing{FileInfo: model.FileInfo{CreatorId: model.BookmarkFileOwner}}},
		{"other_creator_no_post", &model.FileForIndexing{FileInfo: model.FileInfo{CreatorId: idB}}},
		{"deleted_bookmark", &model.FileForIndexing{FileInfo: model.FileInfo{
			DeleteAt: 1, CreatorId: model.BookmarkFileOwner,
		}}},
	}
	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		res = append(res, map[string]any{"name": c.name, "should_index": c.f.ShouldIndex()})
	}
	return res
}

func syncCursorIsEmptyAll() []map[string]any {
	cases := []struct {
		name string
		c    model.GetPostsSinceForSyncCursor
	}{
		{"zero", model.GetPostsSinceForSyncCursor{}},
		{"update_at", model.GetPostsSinceForSyncCursor{LastPostUpdateAt: 1}},
		{"update_id", model.GetPostsSinceForSyncCursor{LastPostUpdateID: idA}},
		{"create_at", model.GetPostsSinceForSyncCursor{LastPostCreateAt: 1}},
		{"create_id", model.GetPostsSinceForSyncCursor{LastPostCreateID: idA}},
	}
	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		res = append(res, map[string]any{"name": c.name, "is_empty": c.c.IsEmpty()})
	}
	return res
}
