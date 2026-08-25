package main

// Behavioural oracle for the **model logic the 2026-08-24 sweep ported**, written to
// fixtures/behaviour_sweep_models.json.
//
// The 245 serialization fixtures that sweep added pin the wire *shape* of every json-tagged type
// it touched. They cannot pin two things:
//
//   - **types whose wire form is not their struct.** `GroupSyncable` renames a `json:"-"` field
//     per its type and errors on a third; `PluginPropertyOption` marshals its inner map
//     unwrapped. Neither is describable by a reflective fixture — both are recorded here, fully
//     populated, in every shape they have.
//   - **the validators, builders and string manipulation.** `AccessControlPolicy.IsValid` alone is
//     five independent version validators; `ChannelBookmark.IsValid` is a per-type matrix whose
//     interesting cases are all negative. Reading those and reasoning about them is the step that
//     produces confident, wrong translations.
//
// Determinism: fixed corpora only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeSweepModelsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"group_syncable_marshal":     groupSyncableMarshalAll(),
		"group_syncable_unmarshal":   groupSyncableUnmarshalAll(),
		"plugin_property_option":     pluginPropertyOptionAll(),
		"access_policy_is_valid":     accessPolicyIsValidAll(),
		"access_policy_inherit":      accessPolicyInheritAll(),
		"channel_bookmark_is_valid":  channelBookmarkIsValidAll(),
		"manifest_is_valid":          manifestIsValidAll(),
		"manifest_executable":        manifestExecutableAll(),
		"manifest_client":            manifestClientAll(),
		"remote_cluster_names":       remoteClusterNameAll(),
		"remote_cluster_topics":      remoteClusterTopicsAll(),
		"remote_cluster_site_url":    remoteClusterSiteURLAll(),
		"compliance_post_row":        compliancePostRowAll(),
		"sanitize_property_value":    sanitizePropertyValueAll(),
		"subject_scoped_roles":       subjectScopedRolesAll(),
		"feature_flags_to_map":       featureFlagsToMapAll(),
		"license_features":           licenseFeaturesAll(),
		"outgoing_webhook_triggers":  outgoingWebhookTriggerAll(),
		"scheduled_recap_time_valid": scheduledRecapTimeOfDayAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_sweep_models.json"), append(blob, '\n'), 0o644)
}

// --- GroupSyncable ------------------------------------------------------------------------------

// fullGroupSyncable populates every field, including the seven the wire form renames or drops, so
// the recorded output shows which of them each shape actually emits.
func fullGroupSyncable(t model.GroupSyncableType) *model.GroupSyncable {
	return &model.GroupSyncable{
		GroupId:            "g5m1zdc3jpn19eh4h6c9j3xyzz",
		SyncableId:         "s7k2waa4hqm28fj5j7d8k4abcd",
		AutoAdd:            true,
		SchemeAdmin:        true,
		CreateAt:           baseTimeMs,
		DeleteAt:           baseTimeMs + 1,
		UpdateAt:           baseTimeMs + 2,
		Type:               t,
		ChannelDisplayName: "Channel Display",
		TeamDisplayName:    "Team Display",
		TeamType:           "O",
		ChannelType:        "P",
		TeamID:             "t9j3xbb5irn39gk6k8e9l5efgh",
	}
}

func groupSyncableMarshalAll() []map[string]any {
	cases := []struct {
		name string
		in   *model.GroupSyncable
	}{
		{"team_full", fullGroupSyncable(model.GroupSyncableTypeTeam)},
		{"channel_full", fullGroupSyncable(model.GroupSyncableTypeChannel)},
		{"team_minimal", &model.GroupSyncable{
			GroupId: "g5m1zdc3jpn19eh4h6c9j3xyzz", SyncableId: "s7k2waa4hqm28fj5j7d8k4abcd",
			Type: model.GroupSyncableTypeTeam,
		}},
		{"channel_minimal", &model.GroupSyncable{
			GroupId: "g5m1zdc3jpn19eh4h6c9j3xyzz", SyncableId: "s7k2waa4hqm28fj5j7d8k4abcd",
			Type: model.GroupSyncableTypeChannel,
		}},
		// The third shape: no shape at all. Marshal *fails*.
		{"unset_type", &model.GroupSyncable{GroupId: "g5m1zdc3jpn19eh4h6c9j3xyzz"}},
		{"unknown_type", &model.GroupSyncable{
			GroupId: "g5m1zdc3jpn19eh4h6c9j3xyzz", Type: model.GroupSyncableType("Nope"),
		}},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		blob, err := json.Marshal(c.in)
		if err != nil {
			row["error"] = err.Error()
		} else {
			row["out"] = string(blob)
		}
		rows = append(rows, row)
	}
	return rows
}

// groupSyncableUnmarshalCorpus records what survives the deliberately lossy UnmarshalJSON: only
// four keys are read, and the presence of channel_id — not the `type` key — picks the type.
var groupSyncableUnmarshalCorpus = []string{
	`{"team_id":"t9j3xbb5irn39gk6k8e9l5efgh","group_id":"g5m1zdc3jpn19eh4h6c9j3xyzz","auto_add":true}`,
	`{"channel_id":"c1a2b3c4d5e6f7g8h9i0j1k2l3","group_id":"g5m1zdc3jpn19eh4h6c9j3xyzz","auto_add":false}`,
	`{"channel_id":"c1a2b3c4d5e6f7g8h9i0j1k2l3","team_id":"t9j3xbb5irn39gk6k8e9l5efgh","group_id":"g5m1zdc3jpn19eh4h6c9j3xyzz"}`,
	`{"team_id":"","channel_id":"","group_id":"g5m1zdc3jpn19eh4h6c9j3xyzz"}`,
	// Everything below the four read keys is dropped on the floor.
	`{"team_id":"t9j3xbb5irn39gk6k8e9l5efgh","group_id":"g5m1zdc3jpn19eh4h6c9j3xyzz","scheme_admin":true,"create_at":123,"update_at":456,"delete_at":789,"type":"Channel"}`,
	`{}`,
}

func groupSyncableUnmarshalAll() []map[string]any {
	rows := make([]map[string]any, 0, len(groupSyncableUnmarshalCorpus))
	for _, in := range groupSyncableUnmarshalCorpus {
		row := map[string]any{"in": in}
		var gs model.GroupSyncable
		if err := json.Unmarshal([]byte(in), &gs); err != nil {
			row["error"] = err.Error()
			rows = append(rows, row)
			continue
		}
		// The struct after decoding, field by field — the `json:"-"` ones are the point.
		row["group_id"] = gs.GroupId
		row["syncable_id"] = gs.SyncableId
		row["type"] = string(gs.Type)
		row["auto_add"] = gs.AutoAdd
		row["scheme_admin"] = gs.SchemeAdmin
		row["create_at"] = gs.CreateAt
		row["team_id"] = gs.TeamID
		// And what it marshals back to, which is not the input.
		if blob, err := json.Marshal(&gs); err == nil {
			row["remarshalled"] = string(blob)
		} else {
			row["remarshal_error"] = err.Error()
		}
		rows = append(rows, row)
	}
	return rows
}

// --- PluginPropertyOption -----------------------------------------------------------------------

func pluginPropertyOptionAll() []map[string]any {
	cases := []struct {
		name string
		in   *model.PluginPropertyOption
	}{
		{"nil_data", &model.PluginPropertyOption{}},
		{"empty_data", &model.PluginPropertyOption{Data: map[string]string{}}},
		{"id_and_name", model.NewPluginPropertyOption("o99t9rfydganbi87d6ekygfory", "Engineering")},
		{"extra_keys", &model.PluginPropertyOption{Data: map[string]string{
			"id": "o99t9rfydganbi87d6ekygfory", "name": "Engineering", "color": "#112233",
		}}},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		blob, err := json.Marshal(c.in)
		if err != nil {
			row["error"] = err.Error()
			rows = append(rows, row)
			continue
		}
		row["out"] = string(blob)
		row["get_id"] = c.in.GetID()
		row["get_name"] = c.in.GetName()
		if verr := c.in.IsValid(); verr != nil {
			row["is_valid_error"] = verr.Error()
		}
		// The unwrapped form decodes straight back into Data.
		var back model.PluginPropertyOption
		if err := json.Unmarshal(blob, &back); err == nil {
			row["round_trip_id"] = back.GetID()
			row["round_trip_name"] = back.GetName()
		}
		rows = append(rows, row)
	}
	return rows
}

// --- AccessControlPolicy ------------------------------------------------------------------------

func policy(mut func(*model.AccessControlPolicy)) *model.AccessControlPolicy {
	p := &model.AccessControlPolicy{
		ID:      "p1a2b3c4d5e6f7g8h9i0j1k2l3",
		Name:    "Engineering only",
		Type:    model.AccessControlPolicyTypeChannel,
		Version: model.AccessControlPolicyVersionV0_3,
		Rules: []model.AccessControlPolicyRule{{
			Actions:    []string{model.AccessControlPolicyActionMembership},
			Expression: `user.attributes.team == "Engineering"`,
		}},
	}
	mut(p)
	return p
}

// accessPolicyCases walks each version's own rules. The names say what is being tested because the
// interesting half is which version *stops* caring about a thing the previous one enforced.
func accessPolicyIsValidAll() []map[string]any {
	cases := []struct {
		name string
		in   *model.AccessControlPolicy
	}{
		{"v0_3_channel_ok", policy(func(p *model.AccessControlPolicy) {})},
		{"unknown_version", policy(func(p *model.AccessControlPolicy) { p.Version = "v9.9" })},
		{"empty_version", policy(func(p *model.AccessControlPolicy) { p.Version = "" })},
		{"bad_id", policy(func(p *model.AccessControlPolicy) { p.ID = "short" })},
		{"negative_revision", policy(func(p *model.AccessControlPolicy) { p.Revision = -1 })},
		{"unknown_type", policy(func(p *model.AccessControlPolicy) { p.Type = "nope" })},

		// v0.1 is the only version that requires a channel policy to *have* rules and caps imports.
		{"v0_1_channel_rules_only", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_1
		})},
		{"v0_1_channel_imports_only", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_1
			p.Rules = nil
			p.Imports = []string{"i1a2b3c4d5e6f7g8h9i0j1k2l3"}
		})},
		{"v0_1_channel_two_imports", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_1
			p.Imports = []string{"i1a2b3c4d5e6f7g8h9i0j1k2l3", "i2a2b3c4d5e6f7g8h9i0j1k2l3"}
		})},
		{"v0_2_channel_imports_only", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_2
			p.Rules = nil
			p.Imports = []string{"i1a2b3c4d5e6f7g8h9i0j1k2l3"}
		})},
		{"v0_2_channel_neither", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_2
			p.Rules = nil
		})},

		// Parent policies: rules required, imports forbidden, name required.
		{"v0_3_parent_ok", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypeParent
		})},
		{"v0_3_parent_no_name", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypeParent
			p.Name = ""
		})},
		{"v0_3_parent_with_imports", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypeParent
			p.Imports = []string{"i1a2b3c4d5e6f7g8h9i0j1k2l3"}
		})},
		{"v0_3_channel_no_name", policy(func(p *model.AccessControlPolicy) { p.Name = "" })},

		// Permission policies want exactly one role.
		{"v0_3_permission_one_role", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypePermission
			p.Roles = []string{"system_user"}
		})},
		{"v0_3_permission_no_roles", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypePermission
		})},
		{"v0_3_permission_two_roles", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypePermission
			p.Roles = []string{"system_user", "system_admin"}
		})},
		{"v0_3_permission_blank_role", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypePermission
			p.Roles = []string{"   "}
		})},

		// Actions.
		{"v0_3_no_actions", policy(func(p *model.AccessControlPolicy) {
			p.Rules[0].Actions = nil
		})},
		{"v0_3_unknown_action", policy(func(p *model.AccessControlPolicy) {
			p.Rules[0].Actions = []string{"teleport"}
		})},
		{"v0_3_session_on_membership", policy(func(p *model.AccessControlPolicy) {
			p.Rules[0].Expression = `user.session.ip_range == "10.0.0.0/8"`
		})},
		{"v0_4_session_on_membership", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules[0].Expression = `user.session.ip_range == "10.0.0.0/8"`
		})},

		// v0.4 permission rules.
		{"v0_4_permission_rule_ok", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules = []model.AccessControlPolicyRule{{
				Actions:    []string{model.AccessControlPolicyActionUploadFileAttachment},
				Expression: "true",
				Name:       "Uploads",
				Role:       model.ChannelUserRoleId,
			}}
		})},
		{"v0_4_permission_rule_no_name", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules = []model.AccessControlPolicyRule{{
				Actions: []string{model.AccessControlPolicyActionUploadFileAttachment},
				Role:    model.ChannelUserRoleId,
			}}
		})},
		{"v0_4_permission_rule_bad_role", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules = []model.AccessControlPolicyRule{{
				Actions: []string{model.AccessControlPolicyActionUploadFileAttachment},
				Name:    "Uploads",
				Role:    "system_user",
			}}
		})},
		{"v0_4_duplicate_rule_names_after_trim", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules = []model.AccessControlPolicyRule{
				{Actions: []string{model.AccessControlPolicyActionUploadFileAttachment}, Name: "Uploads", Role: model.ChannelUserRoleId},
				{Actions: []string{model.AccessControlPolicyActionDownloadFileAttachment}, Name: "Uploads ", Role: model.ChannelAdminRoleId},
			}
		})},
		{"v0_4_membership_with_role", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules[0].Role = model.ChannelUserRoleId
		})},
		{"v0_4_membership_and_permission_in_one_rule", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Rules[0].Actions = []string{
				model.AccessControlPolicyActionMembership,
				model.AccessControlPolicyActionUploadFileAttachment,
			}
		})},
		{"v0_4_permission_rule_on_parent", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
			p.Type = model.AccessControlPolicyTypeParent
			p.Rules = []model.AccessControlPolicyRule{{
				Actions: []string{model.AccessControlPolicyActionUploadFileAttachment},
				Name:    "Uploads",
				Role:    model.ChannelUserRoleId,
			}}
		})},

		// v0.5, the plugin lane.
		{"v0_5_ok", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
			p.Type = "com.mattermost.ai:agent"
			p.Rules = []model.AccessControlPolicyRule{{Actions: []string{"invoke_agent"}, Expression: "true"}}
		})},
		{"v0_5_core_type", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
			p.Rules = []model.AccessControlPolicyRule{{Actions: []string{"invoke_agent"}}}
		})},
		{"v0_5_malformed_action", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
			p.Type = "com.mattermost.ai:agent"
			p.Rules = []model.AccessControlPolicyRule{{Actions: []string{"Invoke_Agent"}}}
		})},
		{"v0_5_wildcard_action", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
			p.Type = "com.mattermost.ai:agent"
			p.Rules = []model.AccessControlPolicyRule{{Actions: []string{"*"}}}
		})},
		{"v0_5_with_role", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
			p.Type = "com.mattermost.ai:agent"
			p.Roles = []string{"system_user"}
			p.Rules = []model.AccessControlPolicyRule{{Actions: []string{"invoke_agent"}}}
		})},
		{"v0_5_team_scope", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
			p.Type = "com.mattermost.ai:agent"
			p.Scope = model.AccessControlPolicyScopeTeam
			p.ScopeID = "t9j3xbb5irn39gk6k8e9l5efgh"
			p.Rules = []model.AccessControlPolicyRule{{Actions: []string{"invoke_agent"}}}
		})},

		// Scope validation runs before the version dispatch.
		{"scope_id_without_scope", policy(func(p *model.AccessControlPolicy) {
			p.ScopeID = "t9j3xbb5irn39gk6k8e9l5efgh"
		})},
		{"team_scope_bad_id", policy(func(p *model.AccessControlPolicy) {
			p.Scope = model.AccessControlPolicyScopeTeam
			p.ScopeID = "short"
		})},
		{"unknown_scope", policy(func(p *model.AccessControlPolicy) {
			p.Scope = "galaxy"
			p.ScopeID = "t9j3xbb5irn39gk6k8e9l5efgh"
		})},
		{"team_scope_ok", policy(func(p *model.AccessControlPolicy) {
			p.Scope = model.AccessControlPolicyScopeTeam
			p.ScopeID = "t9j3xbb5irn39gk6k8e9l5efgh"
		})},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{
			"name":                  c.name,
			"has_permission_action": c.in.HasPermissionRuleAction(),
		}
		if appErr := c.in.IsValid(); appErr != nil {
			row["error_id"] = appErr.Id
			row["error_where"] = appErr.Where
			row["error_details"] = appErr.DetailedError
		}
		rows = append(rows, row)
	}
	return rows
}

func accessPolicyInheritAll() []map[string]any {
	cases := []struct {
		name   string
		child  *model.AccessControlPolicy
		parent *model.AccessControlPolicy
	}{
		{"v0_1", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_1
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
			p.Version = model.AccessControlPolicyVersionV0_1
		})},
		{"v0_2", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_2
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
			p.Version = model.AccessControlPolicyVersionV0_2
		})},
		{"v0_2_duplicate", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_2
			p.Imports = []string{"q1a2b3c4d5e6f7g8h9i0j1k2l3"}
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
			p.Version = model.AccessControlPolicyVersionV0_2
		})},
		{"v0_3_ok", policy(func(p *model.AccessControlPolicy) {}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
		})},
		{"v0_3_parent_is_channel", policy(func(p *model.AccessControlPolicy) {}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"v0_3_parent_wrong_version", policy(func(p *model.AccessControlPolicy) {}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
			p.Version = model.AccessControlPolicyVersionV0_2
		})},
		{"v0_3_permission_child", policy(func(p *model.AccessControlPolicy) {
			p.Type = model.AccessControlPolicyTypePermission
			p.Roles = []string{"system_user"}
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
		})},
		{"v0_4_ok", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
			p.Version = model.AccessControlPolicyVersionV0_4
		})},
		{"v0_4_parent_v0_3", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_4
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
			p.Version = model.AccessControlPolicyVersionV0_3
		})},
		{"v0_5_unsupported", policy(func(p *model.AccessControlPolicy) {
			p.Version = model.AccessControlPolicyVersionV0_5
		}), policy(func(p *model.AccessControlPolicy) {
			p.ID = "q1a2b3c4d5e6f7g8h9i0j1k2l3"
			p.Type = model.AccessControlPolicyTypeParent
		})},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		if appErr := c.child.Inherit(c.parent); appErr != nil {
			row["error_id"] = appErr.Id
			row["error_where"] = appErr.Where
			row["error_details"] = appErr.DetailedError
		}
		// The receiver *after* the call: v0.1 replaces Imports, the rest append, and a failed
		// v0.4 inherit must leave it untouched.
		row["imports"] = c.child.Imports
		row["rule_expressions"] = ruleExpressions(c.child)
		rows = append(rows, row)
	}
	return rows
}

func ruleExpressions(p *model.AccessControlPolicy) []string {
	out := make([]string, 0, len(p.Rules))
	for _, r := range p.Rules {
		out = append(out, r.Expression)
	}
	return out
}

// --- ChannelBookmark ----------------------------------------------------------------------------

func bookmark(mut func(*model.ChannelBookmark)) *model.ChannelBookmark {
	b := &model.ChannelBookmark{
		Id:          "b1a2b3c4d5e6f7g8h9i0j1k2l3",
		CreateAt:    baseTimeMs,
		UpdateAt:    baseTimeMs,
		ChannelId:   "c1a2b3c4d5e6f7g8h9i0j1k2l3",
		OwnerId:     "o1a2b3c4d5e6f7g8h9i0j1k2l3",
		DisplayName: "Runbook",
		Type:        model.ChannelBookmarkLink,
		LinkUrl:     "https://example.com/runbook",
	}
	mut(b)
	return b
}

func channelBookmarkIsValidAll() []map[string]any {
	cases := []struct {
		name string
		in   *model.ChannelBookmark
	}{
		{"link_ok", bookmark(func(b *model.ChannelBookmark) {})},
		{"link_with_file", bookmark(func(b *model.ChannelBookmark) {
			b.FileId = "f1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"link_no_url", bookmark(func(b *model.ChannelBookmark) { b.LinkUrl = "" })},
		{"link_relative_url", bookmark(func(b *model.ChannelBookmark) { b.LinkUrl = "/relative" })},
		{"link_with_image", bookmark(func(b *model.ChannelBookmark) {
			b.ImageUrl = "https://example.com/i.png"
		})},
		{"link_with_bad_image", bookmark(func(b *model.ChannelBookmark) { b.ImageUrl = "not a url" })},
		{"link_with_target", bookmark(func(b *model.ChannelBookmark) {
			b.TargetId = "t1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"file_ok", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkFile
			b.LinkUrl = ""
			b.FileId = "f1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"file_with_link", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkFile
			b.FileId = "f1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"file_no_file", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkFile
			b.LinkUrl = ""
		})},
		{"board_ok", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkBoard
			b.TargetId = "t1a2b3c4d5e6f7g8h9i0j1k2l3"
			b.LinkUrl = "/boards/team/1/2"
		})},
		{"board_absolute_url", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkBoard
			b.TargetId = "t1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"board_protocol_relative", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkBoard
			b.TargetId = "t1a2b3c4d5e6f7g8h9i0j1k2l3"
			b.LinkUrl = "//evil.example.com/x"
		})},
		{"board_scheme_inside", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkBoard
			b.TargetId = "t1a2b3c4d5e6f7g8h9i0j1k2l3"
			b.LinkUrl = "/redirect?to=https://evil.example.com"
		})},
		{"board_no_target", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkBoard
			b.LinkUrl = "/boards/team/1/2"
		})},
		{"board_with_file", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkBoard
			b.TargetId = "t1a2b3c4d5e6f7g8h9i0j1k2l3"
			b.LinkUrl = "/boards/team/1/2"
			b.FileId = "f1a2b3c4d5e6f7g8h9i0j1k2l3"
		})},
		{"image_and_file", bookmark(func(b *model.ChannelBookmark) {
			b.Type = model.ChannelBookmarkFile
			b.LinkUrl = ""
			b.FileId = "f1a2b3c4d5e6f7g8h9i0j1k2l3"
			b.ImageUrl = "https://example.com/i.png"
		})},
		{"unknown_type", bookmark(func(b *model.ChannelBookmark) { b.Type = "sticker" })},
		{"empty_display_name", bookmark(func(b *model.ChannelBookmark) { b.DisplayName = "" })},
		{"long_display_name", bookmark(func(b *model.ChannelBookmark) {
			b.DisplayName = sweepRepeat('é', 65)
		})},
		{"display_name_at_cap", bookmark(func(b *model.ChannelBookmark) {
			b.DisplayName = sweepRepeat('é', 64)
		})},
		{"bad_original_id", bookmark(func(b *model.ChannelBookmark) { b.OriginalId = "short" })},
		{"bad_parent_id", bookmark(func(b *model.ChannelBookmark) { b.ParentId = "short" })},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		if appErr := c.in.IsValid(); appErr != nil {
			row["error_id"] = appErr.Id
			row["error_details"] = appErr.DetailedError
		}
		rows = append(rows, row)
	}

	// PreSave's emoji trimming and unconditional update_at.
	for _, emoji := range []string{"", ":tada:", "tada", "::tada::", ":::", "a:b"} {
		b := bookmark(func(b *model.ChannelBookmark) { b.Emoji = emoji })
		before := b.CreateAt
		b.PreSave()
		rows = append(rows, map[string]any{
			"name":               "presave_emoji:" + emoji,
			"emoji_after":        b.Emoji,
			"create_at_retained": b.CreateAt == before,
			"update_at_equals":   b.UpdateAt == b.CreateAt,
		})
	}
	return rows
}

func sweepRepeat(r rune, n int) string {
	out := make([]rune, n)
	for i := range out {
		out[i] = r
	}
	return string(out)
}

// --- Manifest -----------------------------------------------------------------------------------

func manifestIsValidAll() []map[string]any {
	base := func(mut func(*model.Manifest)) *model.Manifest {
		m := &model.Manifest{
			Id:      "com.example.plugin",
			Name:    "Example",
			Version: "1.2.3",
		}
		mut(m)
		return m
	}
	cases := []struct {
		name string
		in   *model.Manifest
	}{
		{"ok", base(func(m *model.Manifest) {})},
		{"no_version", base(func(m *model.Manifest) { m.Version = "" })},
		{"loose_version", base(func(m *model.Manifest) { m.Version = "1.2" })},
		{"v_prefixed_version", base(func(m *model.Manifest) { m.Version = "v1.2.3" })},
		{"short_id", base(func(m *model.Manifest) { m.Id = "ab" })},
		{"bad_id_chars", base(func(m *model.Manifest) { m.Id = "com example" })},
		{"blank_name", base(func(m *model.Manifest) { m.Name = "   " })},
		{"bad_homepage", base(func(m *model.Manifest) { m.HomepageURL = "not a url" })},
		{"good_homepage", base(func(m *model.Manifest) { m.HomepageURL = "https://example.com" })},
		{"bad_min_server_version", base(func(m *model.Manifest) { m.MinServerVersion = "5.6" })},
		{"good_min_server_version", base(func(m *model.Manifest) { m.MinServerVersion = "5.6.0" })},
		{"regenerate_on_text", base(func(m *model.Manifest) {
			m.SettingsSchema = &model.PluginSettingsSchema{Settings: []*model.PluginSetting{{
				Key: "k", Type: "text", RegenerateHelpText: "no",
			}}}
		})},
		{"placeholder_on_bool", base(func(m *model.Manifest) {
			m.SettingsSchema = &model.PluginSettingsSchema{Settings: []*model.PluginSetting{{
				Key: "k", Type: "bool", Placeholder: "no",
			}}}
		})},
		{"options_on_bool", base(func(m *model.Manifest) {
			m.SettingsSchema = &model.PluginSettingsSchema{Settings: []*model.PluginSetting{{
				Key: "k", Type: "bool", Options: []*model.PluginOption{},
			}}}
		})},
		{"empty_option", base(func(m *model.Manifest) {
			m.SettingsSchema = &model.PluginSettingsSchema{Settings: []*model.PluginSetting{{
				Key: "k", Type: "dropdown", Options: []*model.PluginOption{{DisplayName: "", Value: "v"}},
			}}}
		})},
		{"unknown_setting_type", base(func(m *model.Manifest) {
			m.SettingsSchema = &model.PluginSettingsSchema{Settings: []*model.PluginSetting{{
				Key: "k", Type: "colour",
			}}}
		})},
		{"section_without_key", base(func(m *model.Manifest) {
			m.SettingsSchema = &model.PluginSettingsSchema{
				Sections: []*model.PluginSettingsSection{{Key: ""}},
			}
		})},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		if err := c.in.IsValid(); err != nil {
			row["error"] = err.Error()
		}
		rows = append(rows, row)
	}

	// MeetMinServerVersion: strict on the manifest, lenient on the server.
	for _, pair := range [][2]string{
		{"5.6.0", "5.7.0"}, {"5.6.0", "5.6.0"}, {"5.7.0", "5.6.0"},
		{"5.6.0", "5.6"}, {"5.6.0", "v5.7.0"}, {"5.6", "5.7.0"},
	} {
		m := base(func(m *model.Manifest) { m.MinServerVersion = pair[0] })
		row := map[string]any{"name": "meet:" + pair[0] + "|" + pair[1]}
		ok, err := m.MeetMinServerVersion(pair[1])
		if err != nil {
			row["error"] = err.Error()
		} else {
			row["meets"] = ok
		}
		rows = append(rows, row)
	}
	return rows
}

func manifestExecutableAll() []map[string]any {
	cases := []struct {
		name   string
		server *model.ManifestServer
		os     string
		arch   string
	}{
		{"no_server", nil, "linux", "amd64"},
		{"single", &model.ManifestServer{Executable: "plugin"}, "linux", "amd64"},
		{"map_hit", &model.ManifestServer{
			Executable:  "fallback",
			Executables: map[string]string{"linux-amd64": "dist/linux", "darwin-arm64": "dist/darwin"},
		}, "linux", "amd64"},
		{"map_miss", &model.ManifestServer{
			Executable:  "fallback",
			Executables: map[string]string{"linux-amd64": "dist/linux"},
		}, "windows", "amd64"},
		{"map_empty", &model.ManifestServer{
			Executable: "fallback", Executables: map[string]string{},
		}, "linux", "amd64"},
		{"map_hit_empty_value", &model.ManifestServer{
			Executable:  "fallback",
			Executables: map[string]string{"linux-amd64": ""},
		}, "linux", "amd64"},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		m := &model.Manifest{Id: "com.example.plugin", Server: c.server}
		rows = append(rows, map[string]any{
			"name": c.name,
			"out":  m.GetExecutableForRuntime(c.os, c.arch),
		})
	}
	return rows
}

func manifestClientAll() []map[string]any {
	cases := []struct {
		name string
		in   *model.Manifest
	}{
		{"no_webapp", &model.Manifest{Id: "com.example.plugin", Description: "desc"}},
		{"webapp_no_hash", &model.Manifest{
			Id: "com.example.plugin", Description: "desc",
			Server: &model.ManifestServer{Executable: "plugin"},
			Webapp: &model.ManifestWebapp{BundlePath: "webapp/dist/main.js"},
		}},
		{"webapp_with_hash", &model.Manifest{
			Id: "com.example.plugin", Description: "desc",
			Webapp: &model.ManifestWebapp{BundlePath: "webapp/dist/main.js", BundleHash: []byte{0x0a, 0xff, 0x01}},
		}},
	}

	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		cm := c.in.ClientManifest()
		row := map[string]any{
			"name":        c.name,
			"description": cm.Description,
			"has_server":  cm.Server != nil,
			"has_client":  c.in.HasClient(),
		}
		if cm.Webapp != nil {
			row["bundle_path"] = cm.Webapp.BundlePath
		}
		rows = append(rows, row)
	}
	return rows
}

// --- RemoteCluster ------------------------------------------------------------------------------

var remoteNameCorpus = []string{
	"", "a", "A", "Remote One", "remote_one", "remote.one", "remote-one",
	"  spaced  ", "--dashes--", "Ünïcøde", "MiXeD CaSe", "a b c",
	"tab\there", "sym!bols@here", sweepRepeat('a', 64), sweepRepeat('a', 65),
	sweepRepeat('a', 70) + " tail", "-" + sweepRepeat('a', 63) + "-",
}

func remoteClusterNameAll() []map[string]any {
	rows := make([]map[string]any, 0, len(remoteNameCorpus))
	for _, in := range remoteNameCorpus {
		cleaned := model.CleanRemoteName(in)
		row := map[string]any{
			"in":             in,
			"is_valid":       model.IsValidRemoteName(in),
			"normalized":     model.NormalizeRemoteName(in),
			"cleaned_valid":  model.IsValidRemoteName(cleaned),
			"cleaned_length": len(cleaned),
		}
		// A cleaned value that fell back to a fresh ID is not reproducible, so record only the
		// properties that are — the Rust side asserts the same ones.
		if model.IsValidRemoteName(cleanCandidate(in)) {
			row["cleaned"] = cleaned
		} else {
			row["cleaned_is_new_id"] = true
		}
		rows = append(rows, row)
	}
	return rows
}

// cleanCandidate mirrors CleanRemoteName up to — but not including — its NewId fallback, so the
// oracle can tell "this input has a deterministic answer" from "this one gets a random ID".
func cleanCandidate(s string) string {
	cleaned := model.CleanRemoteName(s)
	// A 26-character z-base-32 string that differs from every deterministic transformation of the
	// input is the fallback; the simplest reliable check is whether cleaning twice is stable.
	if model.CleanRemoteName(s) != cleaned {
		return ""
	}
	return cleaned
}

var topicsCorpus = []string{
	"", " ", "*", " * ", "a", "a b", "  a   b  ", "a\tb", "\na\n", "a  b  c",
	"sharedchannel", " sharedchannel dm ",
}

func remoteClusterTopicsAll() []map[string]any {
	rows := make([]map[string]any, 0, len(topicsCorpus))
	for _, in := range topicsCorpus {
		rc := &model.RemoteCluster{
			RemoteId: "r1a2b3c4d5e6f7g8h9i0j1k2l3",
			Name:     "remote",
			Topics:   in,
			CreateAt: baseTimeMs,
		}
		rc.PreUpdate()
		rows = append(rows, map[string]any{"in": in, "out": rc.Topics})
	}
	return rows
}

func remoteClusterSiteURLAll() []map[string]any {
	corpus := []struct {
		siteURL  string
		pluginID string
	}{
		{"", ""},
		{"https://remote.example.com", ""},
		{"pending_https://remote.example.com", ""},
		{"plugin_com.example", ""},
		{"", "com.example"},
		{"https://remote.example.com", "com.example"},
		{"pending_", ""},
	}
	rows := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		rc := &model.RemoteCluster{SiteURL: c.siteURL, PluginID: c.pluginID}
		rows = append(rows, map[string]any{
			"site_url":     c.siteURL,
			"plugin_id":    c.pluginID,
			"display":      rc.GetSiteURL(),
			"is_confirmed": rc.IsConfirmed(),
			"is_plugin":    rc.IsPlugin(),
		})
	}
	return rows
}

// --- CompliancePost -----------------------------------------------------------------------------

func compliancePostRowAll() []map[string]any {
	base := model.CompliancePost{
		TeamName: "core", TeamDisplayName: "Core",
		ChannelName: "town-square", ChannelDisplayName: "Town Square", ChannelType: "O",
		UserUsername: "parity-user", UserEmail: "parity@example.com", UserNickname: "PU",
		PostId: "p1a2b3c4d5e6f7g8h9i0j1k2l3", PostCreateAt: baseTimeMs, PostUpdateAt: baseTimeMs,
		PostRootId: "", PostOriginalId: "", PostMessage: "hello", PostType: "",
		PostProps: `{"key":"value"}`, PostHashtags: "#tag", PostFileIds: "f1 f2",
	}

	cases := []struct {
		name string
		mut  func(*model.CompliancePost)
	}{
		{"plain", func(p *model.CompliancePost) {}},
		{"edited", func(p *model.CompliancePost) { p.PostUpdateAt = baseTimeMs + 5000 }},
		{"deleted", func(p *model.CompliancePost) { p.PostDeleteAt = baseTimeMs + 9000 }},
		{"bot", func(p *model.CompliancePost) { p.IsBot = true }},
		{"formula_message", func(p *model.CompliancePost) { p.PostMessage = "=SUM(A1:A2)" }},
		{"plus_message", func(p *model.CompliancePost) { p.PostMessage = "+1" }},
		{"minus_message", func(p *model.CompliancePost) { p.PostMessage = "-1" }},
		{"leading_space_formula", func(p *model.CompliancePost) { p.PostMessage = "  =cmd" }},
		{"tab_formula", func(p *model.CompliancePost) { p.PostMessage = "\t=cmd" }},
		{"nbsp_formula", func(p *model.CompliancePost) { p.PostMessage = " =cmd" }},
		{"formula_username", func(p *model.CompliancePost) { p.UserUsername = "=evil" }},
		{"formula_props_untouched", func(p *model.CompliancePost) { p.PostProps = "=notcleaned" }},
		{"fractional_millis", func(p *model.CompliancePost) {
			p.PostCreateAt = baseTimeMs + 123
			p.PostUpdateAt = baseTimeMs + 456
		}},
	}

	rows := make([]map[string]any, 0, len(cases)+1)
	rows = append(rows, map[string]any{"name": "header", "header": model.CompliancePostHeader()})
	for _, c := range cases {
		p := base
		c.mut(&p)
		rows = append(rows, map[string]any{"name": c.name, "row": p.Row()})
	}
	return rows
}

// --- SanitizePropertyValue ----------------------------------------------------------------------

var sanitizeCorpus = []string{
	`""`, `"  "`, `" a "`, `"a"`, `"\ta\n"`,
	`[]`, `["a"]`, `[" a "]`, `["", "a"]`, `["  ", "b"]`, `[" a ", " ", "b "]`,
	`[1, 2]`, `["a", 1]`, `{"k":"v"}`, `null`, `42`, `true`, `1.5`,
	`["already","clean"]`,
}

func sanitizePropertyValueAll() []map[string]any {
	rows := make([]map[string]any, 0, len(sanitizeCorpus))
	for _, in := range sanitizeCorpus {
		out := model.SanitizePropertyValue(json.RawMessage(in))
		rows = append(rows, map[string]any{
			"in":  in,
			"out": string(out),
			// Go returns the *original bytes* when nothing changed, which callers use to skip a
			// write. Recorded so the port's documented divergence is visible, not assumed.
			"unchanged": string(out) == in,
		})
	}
	return rows
}

// --- Subject scoped roles -----------------------------------------------------------------------

func subjectScopedRolesAll() []map[string]any {
	cases := []struct {
		name  string
		build func() *model.Subject
	}{
		{"legacy_only", func() *model.Subject {
			return &model.Subject{Role: "system_admin"}
		}},
		{"channel_only", func() *model.Subject {
			return &model.Subject{Role: "system_admin", ScopedRoles: []model.ScopedRole{
				{Scope: model.AccessControlSubjectScopeChannel, Role: "channel_admin"},
			}}
		}},
		{"both", func() *model.Subject {
			return &model.Subject{Role: "system_admin", ScopedRoles: []model.ScopedRole{
				{Scope: model.AccessControlSubjectScopeSystem, Role: "system_user"},
				{Scope: model.AccessControlSubjectScopeChannel, Role: "channel_user"},
			}}
		}},
		{"duplicate_scopes", func() *model.Subject {
			return &model.Subject{ScopedRoles: []model.ScopedRole{
				{Scope: model.AccessControlSubjectScopeChannel, Role: "channel_user"},
				{Scope: model.AccessControlSubjectScopeChannel, Role: "channel_admin"},
			}}
		}},
	}

	rows := make([]map[string]any, 0, len(cases)*4)
	for _, c := range cases {
		s := c.build()
		rows = append(rows, map[string]any{
			"name":          c.name + ":read",
			"system_role":   s.RoleForScope(model.AccessControlSubjectScopeSystem),
			"channel_role":  s.RoleForScope(model.AccessControlSubjectScopeChannel),
			"unknown_scope": s.RoleForScope("team"),
			"system_roles":  s.RolesForScope(model.AccessControlSubjectScopeSystem),
			"channel_roles": s.RolesForScope(model.AccessControlSubjectScopeChannel),
		})

		for _, set := range []struct{ scope, role string }{
			{model.AccessControlSubjectScopeChannel, "channel_guest"},
			{model.AccessControlSubjectScopeChannel, ""},
			{"", "ignored"},
			{model.AccessControlSubjectScopeSystem, "system_guest"},
		} {
			s := c.build()
			s.SetScopedRole(set.scope, set.role)
			rows = append(rows, map[string]any{
				"name":         c.name + ":set(" + set.scope + "," + set.role + ")",
				"scoped_roles": s.ScopedRoles,
				"system_role":  s.RoleForScope(model.AccessControlSubjectScopeSystem),
				"channel_role": s.RoleForScope(model.AccessControlSubjectScopeChannel),
			})
		}
	}
	return rows
}

// --- FeatureFlags / License Features ------------------------------------------------------------

func featureFlagsToMapAll() map[string]any {
	var defaults model.FeatureFlags
	defaults.SetDefaults()

	// A second instance with the two umbrella dependencies deliberately inconsistent.
	inconsistent := defaults
	inconsistent.PermissionPolicies = false

	return map[string]any{
		"defaults":                        defaults.ToMap(),
		"defaults_channel_policies":       defaults.IsChannelPermissionPoliciesEnabled(),
		"defaults_policy_simulation":      defaults.IsPolicySimulationEnabled(),
		"umbrella_off_channel_policies":   inconsistent.IsChannelPermissionPoliciesEnabled(),
		"umbrella_off_policy_simulation":  inconsistent.IsPolicySimulationEnabled(),
		"zero_value_map":                  (&model.FeatureFlags{}).ToMap(),
		"defaults_test_feature":           defaults.TestFeature,
		"defaults_cluster_graceful_drain": defaults.ClusterGracefulDrain,
	}
}

func licenseFeaturesAll() map[string]any {
	var f model.Features
	f.SetDefaults()

	falseFuture := model.Features{FutureFeatures: model.NewPointer(false)}
	falseFuture.SetDefaults()

	return map[string]any{
		"defaults":               featuresSnapshot(&f),
		"defaults_map":           f.ToMap(),
		"future_false":           featuresSnapshot(&falseFuture),
		"future_false_map":       falseFuture.ToMap(),
		"auto_translation_unset": f.AutoTranslation == nil,
	}
}

// featuresSnapshot records the resolved value of every flag SetDefaults touches, so the four that
// do *not* follow FutureFeatures are visible.
func featuresSnapshot(f *model.Features) map[string]any {
	blob, err := json.Marshal(f)
	if err != nil {
		return nil
	}
	var decoded map[string]any
	if err := json.Unmarshal(blob, &decoded); err != nil {
		return nil
	}
	return decoded
}

// --- OutgoingWebhook trigger words --------------------------------------------------------------

func outgoingWebhookTriggerAll() []map[string]any {
	hook := &model.OutgoingWebhook{TriggerWords: model.StringArray{"deploy", "dep", "status"}}
	corpus := []string{"", "deploy", "dep", "deployment", "de", "status", "statuses", "unrelated", "DEPLOY"}

	rows := make([]map[string]any, 0, len(corpus))
	for _, word := range corpus {
		rows = append(rows, map[string]any{
			"word":              word,
			"exact":             hook.TriggerWordExactMatch(word),
			"starts_with":       hook.TriggerWordStartsWith(word),
			"get_exact":         hook.GetTriggerWord(word, true),
			"get_starts_with":   hook.GetTriggerWord(word, false),
			"empty_trigger_set": (&model.OutgoingWebhook{}).TriggerWordStartsWith(word),
		})
	}
	return rows
}

// --- ScheduledRecap time-of-day -----------------------------------------------------------------

var timeOfDayCorpus = []string{
	"", "0", "00", "0:00", "9:00", "09:00", "23:59", "24:00", "23:60",
	"00:00", "19:05", "1:5", "011:00", "09:0", "09:000", "0900", "09-00",
	"٠٩:٠٠", "09:00 ", " 09:00",
}

func scheduledRecapTimeOfDayAll() []map[string]any {
	rows := make([]map[string]any, 0, len(timeOfDayCorpus))
	for _, in := range timeOfDayCorpus {
		sr := &model.ScheduledRecap{
			Id:          "s1a2b3c4d5e6f7g8h9i0j1k2l3",
			UserId:      "u1a2b3c4d5e6f7g8h9i0j1k2l3",
			Title:       "Daily",
			DaysOfWeek:  model.EveryDay,
			TimeOfDay:   in,
			Timezone:    "Asia/Kolkata",
			TimePeriod:  model.TimePeriodLast24h,
			ChannelMode: model.ChannelModeAllUnreads,
			AgentId:     "agent",
		}
		row := map[string]any{"in": in}
		if appErr := sr.IsValid(); appErr != nil {
			row["error_id"] = appErr.Id
		}
		rows = append(rows, row)
	}

	// deduplicateChannelIDs is unexported; PreSave is its only caller.
	for _, ids := range [][]string{
		nil, {}, {"a"}, {"a", "a"}, {"a", "b", "a"}, {"b", "a", "b", "c"},
	} {
		sr := &model.ScheduledRecap{ChannelIds: model.StringArray(ids)}
		sr.PreUpdate()
		rows = append(rows, map[string]any{
			"dedup_in":  ids,
			"dedup_out": []string(sr.ChannelIds),
		})
	}
	return rows
}
