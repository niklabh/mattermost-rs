package main

// Behavioural oracle for model/scheme.go, written to fixtures/behaviour_scheme.json.
//
// A scheme binds a set of roles to a team or a channel, so `IsValidForCreate` is the gate that
// decides which role names a scheme may name at all. It is one boolean built from fourteen
// branches whose applicability depends on `Scope`, and the branches are *not* symmetric: the three
// channel roles are required for **every** scope, the team, playbook and run roles are required
// only for `team`, and `channel` additionally requires the three team roles to be **empty**. The
// `playbook` and `run` scopes constrain neither. A reading of that switch produces a confident
// wrong answer for at least one scope, so the corpus enumerates the cross product instead:
// every scope against every single-field mutation.
//
// `IsValidSchemeName` compiles `^[a-z0-9_]{2,64}$` on every call. Two things there are worth
// probing rather than assuming: the minimum length is **2**, not 1 as for role names, and Go's
// `$` matches only at end of text — unlike PCRE it does not also match before a trailing newline —
// so `"ab\n"` is rejected. Both are enumerated: every ASCII byte in second position, and the
// length boundary on both sides.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeSchemeBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_valid_scheme_name": isValidSchemeNameAll(),
		"is_valid":             schemeIsValidAll(),
		"patch":                schemePatchAll(),
		"auditable":            schemeAuditableAll(),
		"sanitize":             schemeSanitizeAll(),
		"conveyor":             schemeConveyorAll(),
		"constants": map[string]any{
			"display_name_max_length": model.SchemeDisplayNameMaxLength,
			"name_max_length":         model.SchemeNameMaxLength,
			"description_max_length":  model.SchemeDescriptionMaxLength,
			"scope_team":              model.SchemeScopeTeam,
			"scope_channel":           model.SchemeScopeChannel,
			"scope_playbook":          model.SchemeScopePlaybook,
			"scope_run":               model.SchemeScopeRun,
		},
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_scheme.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

func isValidSchemeNameAll() []map[string]any {
	var probes []string
	// Every ASCII byte in second position of a three-character name: the accepted set, enumerated.
	for c := 0; c < 128; c++ {
		probes = append(probes, "a"+string(rune(c))+"c")
	}
	probes = append(probes,
		"", "a", "ab", "a_", "_a", "__", "00",
		strings.Repeat("a", model.SchemeNameMaxLength),
		strings.Repeat("a", model.SchemeNameMaxLength+1),
		// Go's `$` is end-of-text, not end-of-line: a trailing newline is a rejection, which is
		// exactly where a port using a PCRE-flavoured engine would diverge.
		"ab\n", "\nab", "ab\ncd", "ab\r",
		"AB", "Ab", "aB", "ab-cd", "ab cd", "ábc", "ab😀",
	)

	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		out = append(out, map[string]any{
			"in":    p,
			"bytes": len(p),
			"valid": model.IsValidSchemeName(p),
		})
	}
	return out
}

// schemeIsValidAll enumerates the cross product of scope and single-field mutation rather than
// listing hand-picked cases: whichever branch a reader gets wrong, some cell of the grid covers it.
func schemeIsValidAll() []map[string]any {
	// A FIXED id, for the reason behaviour_role.go gives: model.NewId() would rewrite the fixture
	// on every run. Validated rather than trusted.
	const validID = "fm6y6aeyhy8kxzx7b7oobaeqwh"
	if !model.IsValidId(validID) {
		panic("behaviour_scheme: the pinned scheme id is not a valid id")
	}

	base := func(scope string) *model.Scheme {
		s := &model.Scheme{
			Id:                      validID,
			Name:                    "custom_scheme",
			DisplayName:             "Custom Scheme",
			Description:             "a description",
			Scope:                   scope,
			DefaultChannelAdminRole: "custom_channel_admin",
			DefaultChannelUserRole:  "custom_channel_user",
			DefaultChannelGuestRole: "custom_channel_guest",
		}
		// Only the team scope carries the team, playbook and run roles; a channel-scoped scheme
		// that carries team roles is invalid, which is one of the asymmetries under test.
		if scope == model.SchemeScopeTeam {
			s.DefaultTeamAdminRole = "custom_team_admin"
			s.DefaultTeamUserRole = "custom_team_user"
			s.DefaultTeamGuestRole = "custom_team_guest"
			s.DefaultPlaybookAdminRole = "custom_playbook_admin"
			s.DefaultPlaybookMemberRole = "custom_playbook_member"
			s.DefaultRunAdminRole = "custom_run_admin"
			s.DefaultRunMemberRole = "custom_run_member"
		}
		return s
	}

	mutations := []struct {
		name   string
		mutate func(*model.Scheme)
	}{
		{"unchanged", func(s *model.Scheme) {}},
		{"bad_id", func(s *model.Scheme) { s.Id = "nope" }},
		{"empty_id", func(s *model.Scheme) { s.Id = "" }},
		{"empty_display_name", func(s *model.Scheme) { s.DisplayName = "" }},
		{"display_name_at_cap", func(s *model.Scheme) {
			s.DisplayName = strings.Repeat("d", model.SchemeDisplayNameMaxLength)
		}},
		{"display_name_over_cap", func(s *model.Scheme) {
			s.DisplayName = strings.Repeat("d", model.SchemeDisplayNameMaxLength+1)
		}},
		{"display_name_multibyte_over_byte_cap", func(s *model.Scheme) {
			s.DisplayName = strings.Repeat("é", model.SchemeDisplayNameMaxLength/2+1)
		}},
		{"empty_name", func(s *model.Scheme) { s.Name = "" }},
		{"one_character_name", func(s *model.Scheme) { s.Name = "a" }},
		{"bad_name", func(s *model.Scheme) { s.Name = "Custom Scheme" }},
		{"description_at_cap", func(s *model.Scheme) {
			s.Description = strings.Repeat("x", model.SchemeDescriptionMaxLength)
		}},
		{"description_over_cap", func(s *model.Scheme) {
			s.Description = strings.Repeat("x", model.SchemeDescriptionMaxLength+1)
		}},
		{"empty_channel_admin_role", func(s *model.Scheme) { s.DefaultChannelAdminRole = "" }},
		{"bad_channel_admin_role", func(s *model.Scheme) { s.DefaultChannelAdminRole = "Bad Role" }},
		{"empty_channel_user_role", func(s *model.Scheme) { s.DefaultChannelUserRole = "" }},
		{"empty_channel_guest_role", func(s *model.Scheme) { s.DefaultChannelGuestRole = "" }},
		{"empty_team_admin_role", func(s *model.Scheme) { s.DefaultTeamAdminRole = "" }},
		{"empty_team_user_role", func(s *model.Scheme) { s.DefaultTeamUserRole = "" }},
		{"empty_team_guest_role", func(s *model.Scheme) { s.DefaultTeamGuestRole = "" }},
		{"set_team_admin_role", func(s *model.Scheme) { s.DefaultTeamAdminRole = "custom_team_admin" }},
		{"set_team_user_role", func(s *model.Scheme) { s.DefaultTeamUserRole = "custom_team_user" }},
		{"set_team_guest_role", func(s *model.Scheme) { s.DefaultTeamGuestRole = "custom_team_guest" }},
		{"empty_playbook_admin_role", func(s *model.Scheme) { s.DefaultPlaybookAdminRole = "" }},
		{"empty_playbook_member_role", func(s *model.Scheme) { s.DefaultPlaybookMemberRole = "" }},
		{"empty_run_admin_role", func(s *model.Scheme) { s.DefaultRunAdminRole = "" }},
		{"empty_run_member_role", func(s *model.Scheme) { s.DefaultRunMemberRole = "" }},
		{"set_playbook_admin_role", func(s *model.Scheme) { s.DefaultPlaybookAdminRole = "custom_playbook_admin" }},
		{"set_run_member_role", func(s *model.Scheme) { s.DefaultRunMemberRole = "custom_run_member" }},
		{"bad_playbook_admin_role", func(s *model.Scheme) { s.DefaultPlaybookAdminRole = "Bad Role" }},
	}

	scopes := []string{
		model.SchemeScopeTeam,
		model.SchemeScopeChannel,
		model.SchemeScopePlaybook,
		model.SchemeScopeRun,
		"",
		"bogus",
		"Team",
	}

	out := []map[string]any{}
	for _, scope := range scopes {
		for _, m := range mutations {
			scheme := base(scope)
			m.mutate(scheme)
			out = append(out, map[string]any{
				"scope":               scope,
				"mutation":            m.name,
				"scheme":              scheme,
				"is_valid":            scheme.IsValid(),
				"is_valid_for_create": scheme.IsValidForCreate(),
			})
		}
	}
	return out
}

func schemePatchAll() []map[string]any {
	str := func(s string) *string { return &s }
	cases := []struct {
		name  string
		patch *model.SchemePatch
	}{
		{"empty", &model.SchemePatch{}},
		{"name_only", &model.SchemePatch{Name: str("patched_name")}},
		{"display_name_only", &model.SchemePatch{DisplayName: str("Patched Display")}},
		{"description_only", &model.SchemePatch{Description: str("patched description")}},
		{"all_three", &model.SchemePatch{
			Name: str("patched_name"), DisplayName: str("Patched Display"), Description: str("patched description"),
		}},
		{"empty_strings", &model.SchemePatch{Name: str(""), DisplayName: str(""), Description: str("")}},
	}

	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		scheme := &model.Scheme{
			Id: "scheme1jbyqbtxbtqcgy3wa9tjh", Name: "custom_scheme",
			DisplayName: "Custom Scheme", Description: "a description",
			Scope: model.SchemeScopeChannel, CreateAt: 1755000000000,
			DefaultChannelAdminRole: "custom_channel_admin",
		}
		scheme.Patch(c.patch)
		out = append(out, map[string]any{
			"name":            c.name,
			"scheme":          scheme,
			"patch_auditable": c.patch.Auditable(),
			"scheme_id_patch": (&model.SchemeIDPatch{SchemeID: c.patch.Name}).Auditable(),
		})
	}
	return out
}

func schemeAuditableAll() map[string]any {
	scheme := &model.Scheme{
		Id: "scheme1jbyqbtxbtqcgy3wa9tjh", Name: "custom_scheme", DisplayName: "Custom Scheme",
		Description: "a description", CreateAt: 1, UpdateAt: 2, DeleteAt: 3,
		Scope:                     model.SchemeScopeTeam,
		DefaultTeamAdminRole:      "custom_team_admin",
		DefaultTeamUserRole:       "custom_team_user",
		DefaultTeamGuestRole:      "custom_team_guest",
		DefaultChannelAdminRole:   "custom_channel_admin",
		DefaultChannelUserRole:    "custom_channel_user",
		DefaultChannelGuestRole:   "custom_channel_guest",
		DefaultPlaybookAdminRole:  "custom_playbook_admin",
		DefaultPlaybookMemberRole: "custom_playbook_member",
		DefaultRunAdminRole:       "custom_run_admin",
		DefaultRunMemberRole:      "custom_run_member",
	}

	// SchemeRoles.Auditable returns an EMPTY map — the three booleans it carries are not audited,
	// which is a fact about the audit log rather than an oversight to correct.
	roles := &model.SchemeRoles{SchemeAdmin: true, SchemeUser: true, SchemeGuest: true}

	return map[string]any{
		"scheme":             scheme.Auditable(),
		"scheme_source":      scheme,
		"scheme_roles":       roles.Auditable(),
		"scheme_roles_len":   len(roles.Auditable()),
		"scheme_roles_value": roles,
	}
}

func schemeSanitizeAll() []map[string]any {
	cases := []*model.Scheme{
		{Name: "custom_scheme", DisplayName: "Custom Scheme", Description: "a description", Scope: model.SchemeScopeTeam},
		{Name: "", DisplayName: "", Description: "", Scope: model.SchemeScopeChannel},
	}
	out := make([]map[string]any, 0, len(cases))
	for i, s := range cases {
		before := *s
		s.Sanitize()
		out = append(out, map[string]any{
			"case":          i,
			"name_after":    s.Name,
			"display_after": s.DisplayName,
			"desc_after":    s.Description,
			"scope_after":   s.Scope,
			"scope_before":  before.Scope,
			// Sanitize blanks the NAME too, unlike Role.Sanitize which leaves it alone.
			"name_before": before.Name,
		})
	}
	return out
}

func schemeConveyorAll() []map[string]any {
	conveyors := []*model.SchemeConveyor{
		{
			Name: "conveyed_scheme", DisplayName: "Conveyed", Description: "d",
			Scope:     model.SchemeScopeTeam,
			TeamAdmin: "ca_team_admin", TeamUser: "ca_team_user", TeamGuest: "ca_team_guest",
			ChannelAdmin: "ca_channel_admin", ChannelUser: "ca_channel_user", ChannelGuest: "ca_channel_guest",
			PlaybookAdmin: "ca_playbook_admin", PlaybookMember: "ca_playbook_member",
			RunAdmin: "ca_run_admin", RunMember: "ca_run_member",
			Roles: []*model.Role{{Name: "ca_team_admin", DisplayName: "Team Admin"}},
		},
		{Name: "", Scope: ""},
	}

	out := make([]map[string]any, 0, len(conveyors))
	for i, c := range conveyors {
		scheme := c.Scheme()
		out = append(out, map[string]any{
			"case":       i,
			"conveyor":   c,
			"scheme":     scheme,
			"role_count": len(c.Roles),
			// Scheme() carries fourteen fields and drops the roles, the id and the timestamps.
			"id_empty":        scheme.Id == "",
			"timestamps_zero": scheme.CreateAt == 0 && scheme.UpdateAt == 0 && scheme.DeleteAt == 0,
		})
	}
	return out
}
