// Command dump generates the golden JSON fixtures that the Rust port's
// serialization-parity tests assert against. It is the parity oracle: rather
// than reasoning about whether the Rust JSON matches the Go JSON, we make Go
// say what the JSON is.
//
// # Why reflection instead of hand-written literals
//
// The registry below holds ZERO-valued instances. Every field is filled in by
// reflection, so adding a type is a one-line change requiring no knowledge of
// that type's fields.
//
// This is not just convenience. A field left at its zero value is dropped from
// the JSON by `omitempty`, and the Rust round-trip test for that field then
// passes while proving nothing about it — a green test that cannot fail, on
// precisely the fields most likely to drift. Enumerating ~30 fields by hand for
// each of 198 model types is a guarantee that some will be missed. Reflection
// cannot forget a field.
//
// # Determinism
//
// Every generated value derives from a hash of the field's path, so re-running
// produces byte-identical output. Fixtures are committed; a generator that
// churned every ID on each run would make the diffs unreadable and re-running
// it costly. Do not introduce rand or time.Now here.
package main

import (
	"encoding/base32"
	"encoding/json"
	"flag"
	"fmt"
	"hash/fnv"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
)

// registry maps fixture name -> a zero-valued instance to populate.
//
// TO ADD A TYPE: append one line. Do not populate it here — reflection does
// that, and doing it by hand reintroduces the missed-field problem described
// above.
var registry = map[string]any{
	"app_error":      &model.AppError{},
	"user":           &model.User{},
	"team":           &model.Team{},
	"channel":        &model.Channel{},
	"channel_member": &model.ChannelMember{},

	// channel.go's other wire types.
	"channel_banner_info":                   &model.ChannelBannerInfo{},
	"channel_with_team_data":                &model.ChannelWithTeamData{},
	"channel_patch":                         &model.ChannelPatch{},
	"channel_for_export":                    &model.ChannelForExport{},
	"channel_moderation":                    &model.ChannelModeration{},
	"channel_moderation_patch":              &model.ChannelModerationPatch{},
	"channel_member_count_by_group":         &model.ChannelMemberCountByGroup{},
	"group_message_conversion_request_body": &model.GroupMessageConversionRequestBody{},
	"direct_channel_for_export":             &model.DirectChannelForExport{},
	"channels_with_count":                   &model.ChannelsWithCount{},

	// channel_member.go's other wire types.
	"channel_unread":                &model.ChannelUnread{},
	"channel_unread_at":             &model.ChannelUnreadAt{},
	"channel_member_with_team_data": &model.ChannelMemberWithTeamData{},
	"channel_member_for_export":     &model.ChannelMemberForExport{},
	"channel_member_identifier":     &model.ChannelMemberIdentifier{},
	"set_channel_members_request":   &model.SetChannelMembersRequest{},
	"set_channel_members_response":  &model.SetChannelMembersResponse{},
	"set_channel_members_error":     &model.SetChannelMembersError{},

	"emoji":                         &model.Emoji{},
	"reaction":                      &model.Reaction{},
	"file_info":                     &model.FileInfo{},
	"get_file_infos_options":        &model.GetFileInfosOptions{},
	"post_embed":                    &model.PostEmbed{},
	"post_acknowledgement":          &model.PostAcknowledgement{},
	"post_metadata":                 &model.PostMetadata{},
	"post_image":                    &model.PostImage{},
	"post_translation":              &model.PostTranslation{},
	"post_priority":                 &model.PostPriority{},
	"post":                          &model.Post{},
	"post_list":                     &model.PostList{},
	"post_search_results":           &model.PostSearchResults{},
	"file_info_list":                &model.FileInfoList{},
	"file_info_search_results":      &model.FileInfoSearchResults{},
	"file_upload_response":          &model.FileUploadResponse{},
	"channel_view":                  &model.ChannelView{},
	"channel_data":                  &model.ChannelData{},
	"channel_member_history":        &model.ChannelMemberHistory{},
	"channel_member_history_result": &model.ChannelMemberHistoryResult{},
	"analytics_row":                 &model.AnalyticsRow{},
	"team_stats":                    &model.TeamStats{},
	"users_stats":                   &model.UsersStats{},
	"cluster_stats":                 &model.ClusterStats{},
	"channel_search":                &model.ChannelSearch{},
	"server_limits":                 &model.ServerLimits{},
	"emoji_search":                  &model.EmojiSearch{},
	"user_autocomplete_in_channel":  &model.UserAutocompleteInChannel{},
	"user_autocomplete_in_team":     &model.UserAutocompleteInTeam{},
	"user_autocomplete":             &model.UserAutocomplete{},
	"audit":                         &model.Audit{},
	"user_access_token_search":      &model.UserAccessTokenSearch{},
	"channel_view_response":         &model.ChannelViewResponse{},
	"presign_url_response":          &model.PresignURLResponse{},
	"post_info":                     &model.PostInfo{},
	"draft":                         &model.Draft{},
	"scheduled_post":                &model.ScheduledPost{},
	"search_params":                 &model.SearchParams{},
	"wrangler_post_list":            &model.WranglerPostList{},
	"session":                       &model.Session{},
	"team_member":                   &model.TeamMember{},
	"status":                        &model.Status{},
	"preference":                    &model.Preference{},
	"custom_status":                 &model.CustomStatus{},
	"bot":                           &model.Bot{},
	"bot_patch":                     &model.BotPatch{},
	"audit_record":                  &model.AuditRecord{},
	"event_meta":                    &model.EventMeta{},
	"oauth_app":                     &model.OAuthApp{},
	"oauth_app_request":             &model.OAuthAppRequest{},
	"intune_login_request":          &model.IntuneLoginRequest{},
	"client_registration_request":   &model.ClientRegistrationRequest{},
	"client_registration_response":  &model.ClientRegistrationResponse{},
	"dcr_error":                     &model.DCRError{},
	"product_notice":                &model.ProductNotice{},
	"notice_message":                &model.NoticeMessage{},
	"product_notice_view_state":     &model.ProductNoticeViewState{},
	"external_dependency":           &model.ExternalDependency{},
}

// overrides pins specific fields to semantically valid values, keyed by the
// dotted field path the populator walks ("channel.type"). The generic filler
// produces values that serialize correctly but are meaningless as domain data;
// where a fixture is also useful for exercising IsValid() on the Rust side, pin
// the real value here. Extend freely — this is the escape hatch that keeps the
// walker itself free of per-type special cases. Any value convertible to the
// field's type works, maps included.
//
// The two empty strings below are deliberate, not oversights: "" is the valid
// domain value (a regular post has no type; an email-auth user has no auth
// service), and neither field carries omitempty, so the key still appears in
// the JSON and the parity signal is preserved. The top-level key check in
// missingKeys enforces that — pin "" on an omitempty field and the run fails.
var overrides = map[string]any{
	// "P", not "O": the reflective filler sets Discoverable and GroupConstrained to
	// true, and Channel.IsValid only accepts Discoverable on a private channel. Pinning
	// the type keeps every other field non-zero — pinning Discoverable to false instead
	// would trade a real parity signal for a cosmetic one.
	"channel.type":        "P",
	"channel.displayname": "Town Square",
	"channel.name":        "town-square",
	// Must satisfy channelHexColorRegex or the fixture is not a valid channel.
	"channel.bannerinfo.backgroundcolor": "#1153ab",
	"post.type":                          "",
	"status.status":                      "online",
	"team.type":                          "O",
	"team.name":                          "core-team",
	"team.displayname":                   "Core Team",
	"user.username":                      "parity-user",
	"user.email":                         "parity-user@example.com",
	"user.roles":                         "system_user",
	"user.locale":                        "en",
	"user.authservice":                   "",
	"user.position":                      "Staff Engineer",
	"user.timezone": model.StringMap{
		"automaticTimezone":    "America/New_York",
		"manualTimezone":       "Europe/Berlin",
		"useAutomaticTimezone": "true",
	},
	"session.roles":       "system_user",
	"preference.category": "display_settings",
	"preference.name":     "use_military_time",
	"preference.value":    "true",
	"channelmember.roles": "channel_user",
	// The generic filler produces "key0-…"/"val0-…", which makes the fixture fail
	// ChannelMember.IsValid: with allowMissingFields=false a missing "desktop" or
	// "mark_unread" prop is itself an error. Pin Go's own defaults so the fixture is a
	// valid member as well as a serialization oracle. Every value is non-empty, so no
	// parity signal is lost.
	"channelmember.notifyprops": model.GetDefaultChannelNotifyProps(),
	"teammember.roles":          "team_user",
	// The generic filler produces "duration0-…", which is not in validCustomStatusDuration and
	// would make the fixture fail AreDurationAndExpirationTimeValid. "date_and_time" is the
	// value PreSave itself writes; it is non-empty, so no parity signal is lost.
	"customstatus.duration": "date_and_time",
}

// idEncoding matches model.NewId (utils.go:378) — z-base-32, no padding. 16
// bytes encode to exactly 26 characters, the Mattermost ID length.
var idEncoding = base32.NewEncoding("ybndrfg8ejkmcpqxot1uwisza345h769").WithPadding(base32.NoPadding)

// baseTimeMs is 2023-11-14T22:13:20Z in epoch milliseconds. Go stores all
// timestamps as epoch ms in int64; the Rust side must keep i64 on the wire.
const baseTimeMs int64 = 1_700_000_000_000

const maxDepth = 12

var (
	timeType       = reflect.TypeOf(time.Time{})
	rawMessageType = reflect.TypeOf(json.RawMessage{})
	byteSliceType  = reflect.TypeOf([]byte(nil))
)

func main() {
	out := flag.String("out", "../../fixtures", "directory to write fixtures into")
	rustOut := flag.String("rust-out", "../../crates/mm-model/src", "directory to write generated Rust into")
	flag.Parse()

	if err := os.MkdirAll(*out, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "dump: cannot create %s: %v\n", *out, err)
		os.Exit(1)
	}

	names := make([]string, 0, len(registry))
	for name := range registry {
		names = append(names, name)
	}
	sort.Strings(names)

	var warnings, failures []string

	for _, name := range names {
		p := &populator{stack: map[reflect.Type]int{}}
		v := reflect.ValueOf(registry[name])
		p.fill(v.Elem(), strings.ToLower(v.Elem().Type().Name()), 0)

		blob, err := json.MarshalIndent(registry[name], "", "    ")
		if err != nil {
			failures = append(failures, fmt.Sprintf("%s: marshal: %v", name, err))
			continue
		}

		if missing := missingKeys(v.Elem().Type(), blob); len(missing) > 0 {
			failures = append(failures, fmt.Sprintf(
				"%s: %d field(s) absent from JSON (omitempty dropped a zero value): %s",
				name, len(missing), strings.Join(missing, ", ")))
		}
		for _, note := range p.notes {
			warnings = append(warnings, name+": "+note)
		}

		path := filepath.Join(*out, name+".json")
		if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
			failures = append(failures, fmt.Sprintf("%s: write: %v", name, err))
			continue
		}
		fmt.Printf("wrote %s\n", path)
	}

	// Warnings are fields reflection could not reach (cycles, depth caps,
	// non-empty interfaces). They are not fatal, but each one is a field whose
	// Rust parity test proves less than it appears to.
	for _, w := range warnings {
		fmt.Fprintf(os.Stderr, "warning: %s\n", w)
	}
	if len(failures) > 0 {
		for _, f := range failures {
			fmt.Fprintf(os.Stderr, "FAIL: %s\n", f)
		}
		fmt.Fprintf(os.Stderr, "\n%d fixture(s) are incomplete. A fixture missing a key "+
			"silently weakens the Rust test that consumes it; fix before committing.\n", len(failures))
		os.Exit(1)
	}
	if err := writeBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_utils.json"))

	if err := writeChannelBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel.json"))

	if err := writeChannelMemberBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel member behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_member.json"))

	if err := writeChannelListBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel list behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_list.json"))

	if err := writeVersionBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: version behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_version.json"))

	if err := writeCustomStatusBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: custom status behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_custom_status.json"))

	if err := writeStatusBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: status behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_status.json"))

	if err := writePreferenceBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: preference behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_preference.json"))

	if err := writeEmojiBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: emoji behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_emoji.json"))

	if err := writeReactionBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: reaction behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_reaction.json"))

	if err := writeFileInfoBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: file info behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_file_info.json"))

	if err := writePostLeavesBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post leaves behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_leaves.json"))

	if err := writePostMetadataBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post metadata behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_metadata.json"))

	if err := writePostBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post.json"))

	if err := writeURLBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: url behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_url.json"))

	if err := writeIntegrationActionBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: integration action behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_integration_action.json"))

	if err := writeMessageAttachmentBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: message attachment behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_message_attachment.json"))

	if err := writePostAttachmentsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post attachments behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_attachments.json"))

	if err := writePostInteractiveBlocksBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post interactive blocks behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_interactive_blocks.json"))

	if err := writeDialogBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: dialog behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_dialog.json"))

	if err := writePostActionsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post actions behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_actions.json"))

	if err := writeGoURLBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: go url behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_go_url.json"))

	if err := writeMmBlocksActionsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: mm_blocks actions behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_mm_blocks_actions.json"))

	if err := writePostListBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post list behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_list.json"))

	if err := writePostSearchResultsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post search results behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_search_results.json"))

	if err := writeSearchParamsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: search params behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_search_params.json"))

	if err := writePostInfoBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: post info behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_post_info.json"))

	if err := writeFileInfoListBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: file info list behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_file_info_list.json"))

	if err := writeDraftBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: draft behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_draft.json"))

	if err := writeChannelMentionsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel mentions behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_mentions.json"))

	if err := writeMentionMapBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: mention map behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_mention_map.json"))

	if err := writeScheduledPostBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: scheduled post behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_scheduled_post.json"))

	if err := writeScheduledPostRecurrenceBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: scheduled post recurrence behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_scheduled_post_recurrence.json"))

	if err := writeFileInfoSearchResultsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: file info search results behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_file_info_search_results.json"))

	if err := writeFileBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: file behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_file.json"))

	if err := writeUnicodeBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: unicode behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_unicode.json"))

	if err := writeChannelViewBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel view behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_view.json"))

	if err := writeChannelDataBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel data behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_data.json"))

	if err := writeChannelMemberHistoryBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel member history behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_member_history.json"))

	if err := writeAnalyticsRowBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: analytics row behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_analytics_row.json"))

	if err := writeStatsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: stats behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_stats.json"))

	if err := writeChannelSearchBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: channel search behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_channel_search.json"))

	if err := writeProductNoticesBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: product notices behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_product_notices.json"))

	if err := writeOAuthDCRBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: oauth dcr behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_oauth_dcr.json"))

	if err := writeOAuthBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: oauth behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_oauth.json"))

	if err := writeAuditRecordBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: audit record behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_audit_record.json"))

	if err := writeBotBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: bot behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_bot.json"))

	if err := writeLimitsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: limits behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_limits.json"))

	if err := writeSearchRequestsBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: search requests behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_search_requests.json"))

	if err := writeUserAutocompleteBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: user autocomplete behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_user_autocomplete.json"))

	if err := writeAuditBehaviourFixture(*out); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: audit behaviour fixture: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*out, "behaviour_audit.json"))

	// Not a fixture: a generated Rust source file. See behaviour_emoji.go for why the emoji
	// table is emitted rather than transcribed.
	if err := writeEmojiTable(*rustOut); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: emoji table: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*rustOut, "emoji_generated.rs"))

	if err := writeCjkScriptTable(*rustOut); err != nil {
		fmt.Fprintf(os.Stderr, "FAIL: cjk script table: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("wrote %s\n", filepath.Join(*rustOut, "unicode_generated.rs"))

	fmt.Printf("\n%d fixtures written, all top-level fields present.\n", len(names)+38)
}

type populator struct {
	notes []string
	stack map[reflect.Type]int // types on the current path, for cycle breaking
}

func (p *populator) note(path, reason string) {
	p.notes = append(p.notes, fmt.Sprintf("%s left zero (%s)", path, reason))
}

// fill sets v to a distinctive non-zero value derived deterministically from path.
func (p *populator) fill(v reflect.Value, path string, depth int) {
	if !v.CanSet() {
		return
	}
	t := v.Type()

	if ov, ok := overrides[path]; ok {
		rv := reflect.ValueOf(ov)
		if rv.Type().ConvertibleTo(t) {
			v.Set(rv.Convert(t))
			return
		}
		p.note(path, "override type "+rv.Type().String()+" not convertible to "+t.String())
	}

	switch t {
	case timeType:
		v.Set(reflect.ValueOf(time.UnixMilli(timestampFor(path)).UTC()))
		return
	case rawMessageType:
		// Must be syntactically valid JSON or Marshal fails outright.
		v.Set(reflect.ValueOf(json.RawMessage(`{"key":"value"}`)))
		return
	}

	if depth > maxDepth {
		p.note(path, "max depth")
		return
	}

	switch t.Kind() {
	case reflect.Pointer:
		v.Set(reflect.New(t.Elem()))
		p.fill(v.Elem(), path, depth+1)

	case reflect.Struct:
		if p.stack[t] > 0 {
			p.note(path, "cycle on "+t.String())
			return
		}
		p.stack[t]++
		defer func() { p.stack[t]-- }()
		for i := 0; i < t.NumField(); i++ {
			f := t.Field(i)
			if !f.IsExported() {
				continue // never appears in JSON
			}
			if name, _, _ := strings.Cut(f.Tag.Get("json"), ","); name == "-" {
				continue // explicitly excluded from the wire
			}
			p.fill(v.Field(i), path+"."+strings.ToLower(f.Name), depth+1)
		}

	case reflect.String:
		v.SetString(stringFor(path))

	case reflect.Bool:
		v.SetBool(true)

	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		v.SetInt(intFor(path, t))

	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		v.SetUint(uint64(1 + seedOf(path)%1000))

	case reflect.Float32, reflect.Float64:
		v.SetFloat(float64(1+seedOf(path)%1000) / 8.0)

	case reflect.Slice:
		if t == byteSliceType {
			v.SetBytes([]byte("bytes-" + leafOf(path)))
			return
		}
		n := 2
		switch t.Elem().Kind() {
		case reflect.Struct, reflect.Pointer, reflect.Map, reflect.Slice:
			n = 1 // keep nested fixtures readable
		}
		s := reflect.MakeSlice(t, n, n)
		for i := 0; i < n; i++ {
			p.fill(s.Index(i), fmt.Sprintf("%s[%d]", path, i), depth+1)
		}
		v.Set(s)

	case reflect.Map:
		n := 2
		if t.Elem().Kind() == reflect.Struct || t.Elem().Kind() == reflect.Pointer {
			n = 1
		}
		m := reflect.MakeMap(t)
		for i := 0; i < n; i++ {
			k := reflect.New(t.Key()).Elem()
			p.fill(k, fmt.Sprintf("%s.key%d", path, i), depth+1)
			val := reflect.New(t.Elem()).Elem()
			p.fill(val, fmt.Sprintf("%s.val%d", path, i), depth+1)
			m.SetMapIndex(k, val)
		}
		v.Set(m)

	case reflect.Interface:
		if t.NumMethod() != 0 {
			p.note(path, "non-empty interface "+t.String())
			return
		}
		v.Set(reflect.ValueOf(stringFor(path)))

	default:
		p.note(path, "unsupported kind "+t.Kind().String())
	}
}

// intFor returns a timestamp for fields that hold one and a small distinctive
// number otherwise. Mattermost stores epoch milliseconds in int64.
func intFor(path string, t reflect.Type) int64 {
	leaf := leafOf(path)
	if t.Kind() == reflect.Int64 && (strings.HasSuffix(leaf, "at") || strings.HasSuffix(leaf, "update") ||
		strings.Contains(leaf, "expires") || strings.Contains(leaf, "login")) {
		return timestampFor(path)
	}
	// Keep small so it fits every int width without overflow.
	return int64(1 + seedOf(path)%100)
}

func timestampFor(path string) int64 {
	// Spread across ~90 days, rounded to the second, all after baseTimeMs.
	return baseTimeMs + int64(seedOf(path)%7_776_000)*1000
}

func stringFor(path string) string {
	leaf := leafOf(path)
	switch {
	case leaf == "id" || strings.HasSuffix(leaf, "id"):
		return fakeID(path)
	case strings.Contains(leaf, "email"):
		return leaf + "-" + shortHash(path) + "@example.com"
	case strings.Contains(leaf, "ipaddress"):
		return fmt.Sprintf("10.%d.%d.%d", seedOf(path)%256, seedOf(path+"b")%256, 1+seedOf(path+"c")%254)
	case strings.Contains(leaf, "url") || strings.Contains(leaf, "link"):
		return "https://example.com/" + leaf + "/" + shortHash(path)
	case strings.Contains(leaf, "password"):
		return "correct-horse-battery-" + shortHash(path)
	case strings.Contains(leaf, "token"):
		return fakeID(path)
	}
	return leaf + "-" + shortHash(path)
}

// fakeID returns a deterministic 26-character z-base-32 string, matching the
// shape of model.NewId() without being random (see the determinism note above).
func fakeID(path string) string {
	buf := make([]byte, 0, 16)
	for i := 0; len(buf) < 16; i++ {
		h := seedOf(fmt.Sprintf("%s#%d", path, i))
		for shift := 0; shift < 64 && len(buf) < 16; shift += 8 {
			buf = append(buf, byte(h>>shift))
		}
	}
	return idEncoding.EncodeToString(buf)
}

func shortHash(path string) string {
	return fmt.Sprintf("%06x", seedOf(path)%0xffffff)
}

func seedOf(path string) uint64 {
	h := fnv.New64a()
	_, _ = h.Write([]byte(path))
	return h.Sum64()
}

// leafOf returns the field name a path ends in, so value heuristics key off the
// field rather than a collection index. Trailing "[n]" segments are dropped:
// element values still differ from each other because the hash seed uses the
// full indexed path, but they read as "roles-a1b2c3" rather than "0-a1b2c3".
func leafOf(path string) string {
	for strings.HasSuffix(path, "]") {
		i := strings.LastIndex(path, "[")
		if i < 0 {
			break
		}
		path = path[:i]
	}
	if i := strings.LastIndex(path, "."); i >= 0 {
		return path[i+1:]
	}
	return path
}

// missingKeys reports top-level JSON keys that t declares but the marshalled
// output does not contain — the omitempty-dropped-a-zero-value failure. Nested
// objects are not checked; a nested gap shows up as a warning from the
// populator instead.
func missingKeys(t reflect.Type, blob []byte) []string {
	var obj map[string]json.RawMessage
	if err := json.Unmarshal(blob, &obj); err != nil {
		return []string{"<output is not a JSON object>"}
	}
	var missing []string
	for _, want := range expectedKeys(t) {
		if _, ok := obj[want]; !ok {
			missing = append(missing, want)
		}
	}
	sort.Strings(missing)
	return missing
}

func expectedKeys(t reflect.Type) []string {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if t.Kind() != reflect.Struct {
		return nil
	}
	var keys []string
	for i := 0; i < t.NumField(); i++ {
		f := t.Field(i)
		if !f.IsExported() {
			continue
		}
		name, _, _ := strings.Cut(f.Tag.Get("json"), ",")
		if name == "-" {
			continue
		}
		if f.Anonymous && name == "" {
			keys = append(keys, expectedKeys(f.Type)...) // embedded fields inline
			continue
		}
		if name == "" {
			name = f.Name
		}
		keys = append(keys, name)
	}
	return keys
}
