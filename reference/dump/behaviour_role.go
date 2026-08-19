package main

// Behavioural oracle for model/role.go, written to fixtures/behaviour_role.json.
//
// The data half of role.go is emitted by role_gen.go; this is the other half — the twenty-odd
// functions. Three of them return a slice built by ranging a **map**, so Go's answer has no
// order at all, and that is measured here rather than assumed: each is called fifty times and the
// fixture records whether the order ever varied. A port that pinned Go's "order" would be pinning
// one run of a randomised iterator.
//
// Two functions panic on input the Rust type system makes unrepresentable
// (`RolePatchFromChannelModerationsPatch` dereferences both `Name` and `Roles` unguarded), and
// those are probed under `recover` so the divergence is recorded rather than argued about.
//
// Where a corpus can be enumerated it is enumerated: `IsValidRoleName` is probed with every ASCII
// byte in second position, `GetChannelModeratedPermissions` with every moderated permission id
// against every channel type, and `AddAncillaryPermissions` with every sysconsole key. Hand-picked
// probes decide nothing they were not chosen to decide.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

// orderProbeRuns is how many times a map-ranging function is called to see whether its output
// order is stable. Go randomises map iteration per range, so two distinct elements diverge with
// probability 1-2^-49 over this many runs.
const orderProbeRuns = 50

func writeRoleBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_valid_role_name":                        isValidRoleNameAll(),
		"is_valid_user_roles":                       isValidUserRolesAll(),
		"is_valid_channel_member_roles":             isValidChannelMemberRolesAll(),
		"is_built_in_role":                          isBuiltInRoleAll(),
		"is_channel_scoped_built_in_role":           isChannelScopedBuiltInRoleAll(),
		"clean_role_names":                          cleanRoleNamesAll(),
		"unknown_permissions":                       unknownPermissionsAll(),
		"is_valid":                                  roleIsValidAll(),
		"clone":                                     roleCloneAll(),
		"auditable":                                 roleAuditableAll(),
		"sanitize":                                  roleSanitizeAll(),
		"patch":                                     rolePatchAll(),
		"float_accessors":                           roleFloatAccessorsAll(),
		"permissions_changed_by_patch":              permissionsChangedByPatchAll(),
		"channel_moderated_permissions_changed":     channelModeratedChangedAll(),
		"get_channel_moderated_permissions":         getChannelModeratedPermissionsAll(),
		"merge_channel_higher_scoped_permissions":   mergeHigherScopedAll(),
		"role_patch_from_channel_moderations_patch": rolePatchFromModerationsAll(),
		"add_ancillary_permissions":                 addAncillaryPermissionsAll(),
		"constants": map[string]any{
			"role_name_max_length":         model.RoleNameMaxLength,
			"role_display_name_max_length": model.RoleDisplayNameMaxLength,
			"role_description_max_length":  model.RoleDescriptionMaxLength,
			"fake_setting":                 model.FakeSetting,
			"role_scope_system":            string(model.RoleScopeSystem),
			"role_scope_team":              string(model.RoleScopeTeam),
			"role_scope_channel":           string(model.RoleScopeChannel),
			"role_scope_group":             string(model.RoleScopeGroup),
			"role_type_guest":              string(model.RoleTypeGuest),
			"role_type_user":               string(model.RoleTypeUser),
			"role_type_admin":              string(model.RoleTypeAdmin),
		},
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_role.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

// --- name validation ----------------------------------------------------------

// isValidRoleNameAll enumerates rather than samples: every ASCII byte is probed in second
// position, which is what actually establishes the accepted set, plus the length boundary either
// side of RoleNameMaxLength and a few multi-byte cases. `TrimLeft` takes a **cutset**, so the
// check is "every character is in [a-z0-9_]", not a prefix test — an input that starts with an
// illegal character is rejected for the same reason one that ends with it is.
func isValidRoleNameAll() []map[string]any {
	var probes []string
	for c := 0; c < 128; c++ {
		probes = append(probes, "a"+string(rune(c)))
	}
	probes = append(probes,
		"", "a", "_", "0", "system_admin", "channel_user",
		strings.Repeat("a", model.RoleNameMaxLength),
		strings.Repeat("a", model.RoleNameMaxLength+1),
		strings.Repeat("é", model.RoleNameMaxLength/2),   // 64 bytes, 32 runes
		strings.Repeat("é", model.RoleNameMaxLength/2+1), // 66 bytes: over the byte cap
		"é", "rôle", "ROLE", "Role", "role-name", "role name", " role", "role ",
	)

	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		out = append(out, map[string]any{
			"in":    p,
			"bytes": len(p),
			"valid": model.IsValidRoleName(p),
		})
	}
	return out
}

func isValidUserRolesAll() []map[string]any {
	probes := []string{
		"", " ", "\t", "\n",
		"system_admin",
		"system_admin ",
		" system_admin",
		"system_admin system_user",
		"system_user system_admin",
		"system_user",
		"system_user  system_admin", // two spaces: Fields collapses runs
		"system_user\tsystem_admin",
		"system_admin system_admin",
		"System_Admin",
		"not_a_role",
		"role-with-hyphen",
		"system_user role-with-hyphen",
	}
	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		out = append(out, map[string]any{
			"in":     p,
			"fields": strings.Fields(p),
			"valid":  model.IsValidUserRoles(p),
		})
	}
	return out
}

func isValidChannelMemberRolesAll() []map[string]any {
	var probes []string
	for _, id := range model.BuiltInSchemeManagedRoleIDs {
		probes = append(probes, id)
		probes = append(probes, id+" channel_user")
	}
	probes = append(probes,
		"", " ", "channel_user channel_admin", "channel_guest",
		"custom_role", "custom_role channel_user", "system_admin",
	)
	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		out = append(out, map[string]any{
			"in":            p,
			"valid":         model.IsValidChannelMemberRoles(p),
			"valid_as_user": model.IsValidUserRoles(p),
		})
	}
	return out
}

func isBuiltInRoleAll() []map[string]any {
	var probes []string
	probes = append(probes, model.BuiltInSchemeManagedRoleIDs...)
	for key := range model.MakeDefaultRoles() {
		probes = append(probes, key)
	}
	probes = append(probes, "", "custom_role", "System_Admin", "system_admin ")
	sort.Strings(probes)

	seen := map[string]bool{}
	out := []map[string]any{}
	for _, p := range probes {
		if seen[p] {
			continue
		}
		seen[p] = true
		out = append(out, map[string]any{"in": p, "built_in": model.IsBuiltInRole(p)})
	}
	return out
}

func isChannelScopedBuiltInRoleAll() []map[string]any {
	var probes []string
	probes = append(probes, model.BuiltInSchemeManagedRoleIDs...)
	probes = append(probes, "", "custom_role")
	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		out = append(out, map[string]any{
			"in":             p,
			"channel_scoped": model.IsChannelScopedBuiltInRole(p),
		})
	}
	return out
}

// cleanRoleNamesAll pins the two things a reading gets wrong: an all-whitespace entry is *dropped*
// rather than rejected, and on rejection the function returns the **original** slice unchanged
// alongside false, not the partially cleaned one.
func cleanRoleNamesAll() []map[string]any {
	cases := [][]string{
		nil,
		{},
		{"system_user"},
		{"system_user", "channel_admin"},
		{"", "system_user"},
		{"   ", "system_user"},
		{"\t\n", "system_user"},
		{"system_user", ""},
		{"system_user", "BAD"},
		{"BAD", "system_user"},
		{" system_user "},
		{"role-with-hyphen"},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		cleaned, ok := model.CleanRoleNames(c)
		out = append(out, map[string]any{
			"in":          c,
			"in_nil":      c == nil,
			"cleaned":     cleaned,
			"cleaned_nil": cleaned == nil,
			"ok":          ok,
			// Whether the returned slice is the argument itself, which is what the failure path
			// returns and the success path never does.
			"returned_input": ok == false && reflect.ValueOf(cleaned).Pointer() == reflect.ValueOf(c).Pointer(),
		})
	}
	return out
}

// --- validation ---------------------------------------------------------------

func unknownPermissionsAll() []map[string]any {
	deprecated := model.DeprecatedPermissions[0].Id
	cases := [][]string{
		nil,
		{},
		{model.PermissionCreatePost.Id},
		{deprecated},
		{model.PermissionCreatePost.Id, deprecated},
		{"nope"},
		{"nope", model.PermissionCreatePost.Id, "also_nope"},
		{""},
		{"CREATE_POST"},
		{model.PermissionCreatePost.Id, model.PermissionCreatePost.Id},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		r := &model.Role{Permissions: c}
		unknown := r.UnknownPermissions()
		out = append(out, map[string]any{
			"permissions": c,
			"unknown":     unknown,
			"unknown_nil": unknown == nil,
		})
	}
	return out
}

func roleIsValidAll() []map[string]any {
	// A FIXED id, not model.NewId(): a random one rewrites twenty lines of a committed fixture on
	// every generator run and destroys the "a clean run touches only new files" signal — the same
	// defect as [D-032]. Validated below rather than trusted, so a mistyped literal fails here.
	const validID = "mxry3atgeme67kggbutncoqt7a"
	if !model.IsValidId(validID) {
		panic("behaviour_role: the pinned role id is not a valid id")
	}
	base := func() *model.Role {
		return &model.Role{
			Id:          validID,
			Name:        "custom_role",
			DisplayName: "Custom Role",
			Description: "a description",
			Permissions: []string{model.PermissionCreatePost.Id},
		}
	}

	type roleCase struct {
		name   string
		mutate func(*model.Role)
	}
	cases := []roleCase{
		{"valid", func(r *model.Role) {}},
		{"bad_id", func(r *model.Role) { r.Id = "nope" }},
		{"empty_id", func(r *model.Role) { r.Id = "" }},
		{"bad_name", func(r *model.Role) { r.Name = "Bad Name" }},
		{"empty_name", func(r *model.Role) { r.Name = "" }},
		{"name_too_long", func(r *model.Role) { r.Name = strings.Repeat("a", model.RoleNameMaxLength+1) }},
		{"name_at_cap", func(r *model.Role) { r.Name = strings.Repeat("a", model.RoleNameMaxLength) }},
		{"empty_display_name", func(r *model.Role) { r.DisplayName = "" }},
		{"display_name_at_cap", func(r *model.Role) { r.DisplayName = strings.Repeat("d", model.RoleDisplayNameMaxLength) }},
		{"display_name_over_cap", func(r *model.Role) { r.DisplayName = strings.Repeat("d", model.RoleDisplayNameMaxLength+1) }},
		{"display_name_multibyte_over_byte_cap", func(r *model.Role) {
			r.DisplayName = strings.Repeat("é", model.RoleDisplayNameMaxLength/2+1)
		}},
		{"description_at_cap", func(r *model.Role) { r.Description = strings.Repeat("x", model.RoleDescriptionMaxLength) }},
		{"description_over_cap", func(r *model.Role) { r.Description = strings.Repeat("x", model.RoleDescriptionMaxLength+1) }},
		{"empty_description", func(r *model.Role) { r.Description = "" }},
		{"nil_permissions", func(r *model.Role) { r.Permissions = nil }},
		{"unknown_permission", func(r *model.Role) { r.Permissions = []string{"nope"} }},
		{"two_unknown_permissions", func(r *model.Role) { r.Permissions = []string{"nope", "also_nope"} }},
		{"deprecated_permission", func(r *model.Role) { r.Permissions = []string{model.DeprecatedPermissions[0].Id} }},
		{"quote_in_name", func(r *model.Role) { r.Name = "bad\"name" }},
		// Three probes for the error text's %q, which is Go's strconv.Quote and not obviously the
		// same function as Rust's Debug formatting for str.
		{"unicode_name", func(r *model.Role) { r.Name = "rôle" }},
		{"control_char_name", func(r *model.Role) { r.Name = "a\x7fb" }},
		{"emoji_name", func(r *model.Role) { r.Name = "role\U0001F600" }},
	}

	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		withID := base()
		c.mutate(withID)
		withoutID := base()
		c.mutate(withoutID)

		errWith := withID.IsValid()
		errWithout := withoutID.IsValidWithoutId()
		out = append(out, map[string]any{
			"name":           c.name,
			"role":           withID,
			"is_valid_err":   errString(errWith),
			"is_valid_ok":    errWith == nil,
			"without_id_err": errString(errWithout),
			"without_id_ok":  errWithout == nil,
		})
	}
	return out
}

// --- value semantics ----------------------------------------------------------

func roleCloneAll() []map[string]any {
	schemeID := "scheme1jbyqbtxbtqcgy3wa9tjhy"
	cases := []struct {
		name string
		role *model.Role
	}{
		{"full", &model.Role{
			Id: "role1jbyqbtxbtqcgy3wa9tjhyc", Name: "custom_role", DisplayName: "Custom",
			Description: "d", CreateAt: 1755000000000, UpdateAt: 1755000000001, DeleteAt: 0,
			Permissions: []string{"create_post", "edit_post"}, SchemeManaged: true, BuiltIn: true,
			SchemeId: &schemeID,
		}},
		{"nil_permissions", &model.Role{Name: "custom_role", Permissions: nil}},
		{"empty_permissions", &model.Role{Name: "custom_role", Permissions: []string{}}},
		{"nil_scheme_id", &model.Role{Name: "custom_role", SchemeId: nil}},
	}

	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		clone := c.role.Clone()

		// Mutating the clone must not reach the original. Both fields are probed because Clone
		// copies the struct by value first, so only the two explicitly deep-copied ones matter.
		var permsAliased, schemeAliased bool
		if len(clone.Permissions) > 0 {
			clone.Permissions[0] = "MUTATED"
			permsAliased = c.role.Permissions[0] == "MUTATED"
			clone.Permissions[0] = c.role.Permissions[0]
		}
		if clone.SchemeId != nil {
			*clone.SchemeId = "MUTATED"
			schemeAliased = *c.role.SchemeId == "MUTATED"
			*clone.SchemeId = *c.role.SchemeId
		}

		out = append(out, map[string]any{
			"name":                c.name,
			"clone":               clone,
			"equal":               reflect.DeepEqual(c.role, clone),
			"permissions_nil":     clone.Permissions == nil,
			"permissions_aliased": permsAliased,
			"scheme_id_aliased":   schemeAliased,
			"scheme_id_nil":       clone.SchemeId == nil,
		})
	}
	return out
}

func roleAuditableAll() []map[string]any {
	schemeID := "scheme1jbyqbtxbtqcgy3wa9tjhy"
	cases := []*model.Role{
		{
			Id: "role1jbyqbtxbtqcgy3wa9tjhyc", Name: "custom_role", DisplayName: "Custom",
			Description: "d", CreateAt: 1, UpdateAt: 2, DeleteAt: 3,
			Permissions: []string{"create_post"}, SchemeManaged: true, BuiltIn: true, SchemeId: &schemeID,
		},
		{Name: "custom_role"},
	}
	out := make([]map[string]any, 0, len(cases))
	for i, r := range cases {
		auditable := r.Auditable()
		keys := make([]string, 0, len(auditable))
		for k := range auditable {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		out = append(out, map[string]any{
			"case":      i,
			"role":      r,
			"auditable": auditable,
			"keys":      keys,
		})
	}
	return out
}

func roleSanitizeAll() []map[string]any {
	cases := []*model.Role{
		{Name: "custom_role", DisplayName: "Custom", Description: "a description"},
		{Name: "custom_role", DisplayName: "", Description: ""},
	}
	out := make([]map[string]any, 0, len(cases))
	for i, r := range cases {
		before := *r
		r.Sanitize()
		out = append(out, map[string]any{
			"case":                 i,
			"display_name_before":  before.DisplayName,
			"description_before":   before.Description,
			"display_name_after":   r.DisplayName,
			"description_after":    r.Description,
			"other_fields_touched": r.Name != before.Name || r.Id != before.Id,
		})
	}
	return out
}

func rolePatchAll() []map[string]any {
	empty := []string{}
	replacement := []string{"edit_post"}
	cases := []struct {
		name  string
		patch *model.RolePatch
	}{
		{"nil_permissions", &model.RolePatch{Permissions: nil}},
		{"empty_permissions", &model.RolePatch{Permissions: &empty}},
		{"replacement", &model.RolePatch{Permissions: &replacement}},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		r := &model.Role{Name: "custom_role", Permissions: []string{"create_post"}}
		r.Patch(c.patch)
		out = append(out, map[string]any{
			"name":            c.name,
			"permissions":     r.Permissions,
			"permissions_nil": r.Permissions == nil,
			"auditable":       c.patch.Auditable(),
		})
	}
	return out
}

func roleFloatAccessorsAll() []map[string]any {
	probes := []int64{0, 1, -1, 1755000000000, 9007199254740993, -9007199254740993}
	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		r := &model.Role{CreateAt: p, UpdateAt: p, DeleteAt: p}
		out = append(out, map[string]any{
			"in":        p,
			"create_at": r.CreateAt_(),
			"update_at": r.UpdateAt_(),
			"delete_at": r.DeleteAt_(),
		})
	}
	return out
}

// --- patch diffing ------------------------------------------------------------

// permissionsChangedByPatchAll: the result IS ordered — the function ranges two slices, not maps —
// so this corpus asserts the order as well as the membership.
func permissionsChangedByPatchAll() []map[string]any {
	cases := []struct {
		name  string
		role  []string
		patch *[]string
	}{
		{"nil_patch", []string{"create_post"}, nil},
		{"identical", []string{"create_post", "edit_post"}, ptr([]string{"create_post", "edit_post"})},
		{"reordered", []string{"create_post", "edit_post"}, ptr([]string{"edit_post", "create_post"})},
		{"added", []string{"create_post"}, ptr([]string{"create_post", "edit_post"})},
		{"removed", []string{"create_post", "edit_post"}, ptr([]string{"create_post"})},
		{"disjoint", []string{"create_post"}, ptr([]string{"edit_post"})},
		{"empty_patch", []string{"create_post"}, ptr([]string{})},
		{"empty_role", nil, ptr([]string{"create_post"})},
		{"both_empty", nil, ptr([]string{})},
		{"duplicates_in_role", []string{"create_post", "create_post"}, ptr([]string{"edit_post"})},
		{"duplicates_in_patch", []string{"edit_post"}, ptr([]string{"create_post", "create_post"})},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		role := &model.Role{Permissions: c.role}
		patch := &model.RolePatch{Permissions: c.patch}
		result := model.PermissionsChangedByPatch(role, patch)
		out = append(out, map[string]any{
			"name":        c.name,
			"role":        c.role,
			"patch":       c.patch,
			"changed":     result,
			"changed_nil": result == nil,
		})
	}
	return out
}

// channelModeratedChangedAll records the sorted result and, separately, whether the order ever
// varied across fifty calls — because the function builds its result by ranging two maps.
func channelModeratedChangedAll() []map[string]any {
	cases := []struct {
		name    string
		role    []string
		patch   *[]string
		nilRole bool
	}{
		{"nil_role", nil, ptr([]string{"create_post"}), true},
		{"nil_patch", []string{"create_post"}, nil, false},
		{"identical", []string{"create_post"}, ptr([]string{"create_post"}), false},
		{"one_removed", []string{"create_post"}, ptr([]string{}), false},
		{"one_added", []string{}, ptr([]string{"create_post"}), false},
		{"two_changed", []string{"create_post", "add_reaction"}, ptr([]string{"use_channel_mentions"}), false},
		{"three_changed", []string{"create_post", "add_reaction", "manage_public_channel_members"},
			ptr([]string{"use_channel_mentions", "add_bookmark_public_channel"}), false},
		{"reaction_pair_is_one_control", []string{"add_reaction"}, ptr([]string{"remove_reaction"}), false},
		{"unmoderated_permissions_ignored", []string{"manage_team"}, ptr([]string{"manage_system"}), false},
		{"bookmarks_collapse", []string{"add_bookmark_public_channel"},
			ptr([]string{"edit_bookmark_private_channel"}), false},
	}

	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		var role *model.Role
		if !c.nilRole {
			role = &model.Role{Permissions: c.role}
		}
		patch := &model.RolePatch{Permissions: c.patch}

		first := model.ChannelModeratedPermissionsChangedByPatch(role, patch)
		orderVaried := false
		for i := 0; i < orderProbeRuns; i++ {
			again := model.ChannelModeratedPermissionsChangedByPatch(role, patch)
			if !reflect.DeepEqual(first, again) {
				orderVaried = true
			}
		}
		sorted := append([]string(nil), first...)
		sort.Strings(sorted)

		out = append(out, map[string]any{
			"name":         c.name,
			"role":         c.role,
			"role_nil":     c.nilRole,
			"patch":        c.patch,
			"changed":      sorted,
			"changed_nil":  first == nil,
			"order_varied": orderVaried,
		})
	}
	return out
}

// --- moderation ---------------------------------------------------------------

// getChannelModeratedPermissionsAll enumerates every moderated permission id against every channel
// type, which is what establishes the two special cases: manage_members and the bookmark family
// answer differently for public and private channels, and every other control answers true
// regardless. The result is a map, so no order is involved.
func getChannelModeratedPermissionsAll() []map[string]any {
	channelTypes := []model.ChannelType{
		model.ChannelTypeOpen, model.ChannelTypePrivate, model.ChannelTypeDirect, model.ChannelTypeGroup,
	}

	moderated := make([]string, 0, len(model.ChannelModeratedPermissionsMap))
	for id := range model.ChannelModeratedPermissionsMap {
		moderated = append(moderated, id)
	}
	sort.Strings(moderated)

	out := []map[string]any{}
	for _, id := range moderated {
		for _, ct := range channelTypes {
			r := &model.Role{Permissions: []string{id}}
			out = append(out, map[string]any{
				"permissions":  []string{id},
				"channel_type": string(ct),
				"result":       r.GetChannelModeratedPermissions(ct),
			})
		}
	}

	// Multi-permission roles, including the two members permissions together, and a role holding
	// no moderated permission at all.
	combos := [][]string{
		{},
		{"manage_team"},
		{"manage_public_channel_members", "manage_private_channel_members"},
		{"add_bookmark_public_channel", "add_bookmark_private_channel"},
		{"delete_bookmark_public_channel"},
		{"create_post", "add_reaction", "remove_reaction", "use_channel_mentions"},
	}
	for _, combo := range combos {
		for _, ct := range channelTypes {
			r := &model.Role{Permissions: combo}
			out = append(out, map[string]any{
				"permissions":  combo,
				"channel_type": string(ct),
				"result":       r.GetChannelModeratedPermissions(ct),
			})
		}
	}
	return out
}

// mergeHigherScopedAll: the result IS ordered, because the function ranges AllPermissions rather
// than a map — it is the one function here whose order can be asserted, and the order it produces
// is AllPermissions' order filtered to channel scope, not the role's own order.
func mergeHigherScopedAll() []map[string]any {
	channelUser := model.MakeDefaultRoles()[model.ChannelUserRoleId].Permissions
	cases := []struct {
		name       string
		role       []string
		higherID   string
		higherPerm []string
	}{
		{"channel_admin_takes_higher_scope", []string{}, model.ChannelAdminRoleId, channelUser},
		{"channel_admin_ignores_role", []string{"create_post"}, model.ChannelAdminRoleId, []string{"edit_post"}},
		{"member_moderated_needs_both", []string{"create_post"}, model.ChannelUserRoleId, []string{"create_post"}},
		{"member_moderated_role_only", []string{"create_post"}, model.ChannelUserRoleId, []string{}},
		{"member_moderated_higher_only", []string{}, model.ChannelUserRoleId, []string{"create_post"}},
		{"member_unmoderated_higher_only", []string{}, model.ChannelUserRoleId, []string{"upload_file"}},
		{"member_unmoderated_role_only", []string{"upload_file"}, model.ChannelUserRoleId, []string{}},
		{"non_channel_scope_dropped", []string{"manage_team"}, model.ChannelUserRoleId, []string{"manage_team"}},
		{"full_channel_user", channelUser, model.ChannelUserRoleId, channelUser},
		{"guest_role_id", []string{"create_post"}, model.ChannelGuestRoleId, []string{"create_post"}},
	}

	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		r := &model.Role{Permissions: append([]string(nil), c.role...)}
		r.MergeChannelHigherScopedPermissions(&model.RolePermissions{
			RoleID:      c.higherID,
			Permissions: c.higherPerm,
		})
		out = append(out, map[string]any{
			"name":           c.name,
			"role":           c.role,
			"higher_role_id": c.higherID,
			"higher":         c.higherPerm,
			"merged":         r.Permissions,
			"merged_nil":     r.Permissions == nil,
		})
	}
	return out
}

// rolePatchFromModerationsAll also probes the two unguarded dereferences under recover: Go panics
// on a nil Name and on nil Roles, which the Rust signature cannot express.
func rolePatchFromModerationsAll() []map[string]any {
	name := func(s string) *string { return &s }
	boolp := func(b bool) *bool { return &b }

	cases := []struct {
		name     string
		role     []string
		roleName string
		patches  []*model.ChannelModerationPatch
	}{
		{"empty_patch_keeps_moderated", []string{"create_post", "manage_team"}, "members", nil},
		{"disable_members", []string{"create_post"}, "members", []*model.ChannelModerationPatch{
			{Name: name("create_post"), Roles: &model.ChannelModeratedRolesPatch{Members: boolp(false)}},
		}},
		{"disable_guests_leaves_members", []string{"create_post"}, "members", []*model.ChannelModerationPatch{
			{Name: name("create_post"), Roles: &model.ChannelModeratedRolesPatch{Guests: boolp(false)}},
		}},
		{"enable_adds_permission", []string{}, "members", []*model.ChannelModerationPatch{
			{Name: name("create_reactions"), Roles: &model.ChannelModeratedRolesPatch{Members: boolp(true)}},
		}},
		{"enable_for_guests", []string{}, "guests", []*model.ChannelModerationPatch{
			{Name: name("create_reactions"), Roles: &model.ChannelModeratedRolesPatch{Guests: boolp(true)}},
		}},
		{"enable_bookmarks_adds_all_eight", []string{}, "members", []*model.ChannelModerationPatch{
			{Name: name("manage_bookmarks"), Roles: &model.ChannelModeratedRolesPatch{Members: boolp(true)}},
		}},
		{"unknown_role_name", []string{"create_post"}, "admins", []*model.ChannelModerationPatch{
			{Name: name("create_post"), Roles: &model.ChannelModeratedRolesPatch{Members: boolp(false)}},
		}},
		{"nil_bool_is_not_false", []string{"create_post"}, "members", []*model.ChannelModerationPatch{
			{Name: name("create_post"), Roles: &model.ChannelModeratedRolesPatch{}},
		}},
	}

	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		r := &model.Role{Permissions: c.role}
		first := r.RolePatchFromChannelModerationsPatch(c.patches, c.roleName)

		orderVaried := false
		for i := 0; i < orderProbeRuns; i++ {
			again := r.RolePatchFromChannelModerationsPatch(c.patches, c.roleName)
			if !reflect.DeepEqual(first.Permissions, again.Permissions) {
				orderVaried = true
			}
		}
		sorted := append([]string(nil), *first.Permissions...)
		sort.Strings(sorted)

		out = append(out, map[string]any{
			"name":            c.name,
			"role":            c.role,
			"role_name":       c.roleName,
			"patch_count":     len(c.patches),
			"permissions":     sorted,
			"permissions_nil": first.Permissions == nil,
			"order_varied":    orderVaried,
		})
	}

	// The two panics, measured rather than reasoned about.
	out = append(out,
		map[string]any{
			"name": "nil_name_panics",
			"panics": panicsOnModerationPatch(&model.ChannelModerationPatch{
				Roles: &model.ChannelModeratedRolesPatch{Members: ptr(true)},
			}),
		},
		map[string]any{
			"name":   "nil_roles_panics",
			"panics": panicsOnModerationPatch(&model.ChannelModerationPatch{Name: name("create_post")}),
		},
	)
	return out
}

func panicsOnModerationPatch(patch *model.ChannelModerationPatch) (panicked bool) {
	defer func() {
		if recover() != nil {
			panicked = true
		}
	}()
	r := &model.Role{Permissions: []string{"create_post"}}
	r.RolePatchFromChannelModerationsPatch([]*model.ChannelModerationPatch{patch}, "members")
	return false
}

// --- ancillary permissions ----------------------------------------------------

// addAncillaryPermissionsAll enumerates every sysconsole key. Two behaviours it pins that a
// reading misses: the function **appends to the slice it was given** and returns it, so the
// caller's slice may or may not be aliased depending on capacity; and it ranges the original
// slice header, so ancillary permissions added during the loop are never themselves expanded.
func addAncillaryPermissionsAll() []map[string]any {
	keys := make([]string, 0, len(model.SysconsoleAncillaryPermissions))
	for k := range model.SysconsoleAncillaryPermissions {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	out := []map[string]any{}
	for _, key := range keys {
		in := []string{key}
		out = append(out, map[string]any{
			"in":  []string{key},
			"out": model.AddAncillaryPermissions(in),
		})
	}

	extra := [][]string{
		nil,
		{},
		{"not_a_sysconsole_permission"},
		{"create_post"},
		{keys[0], keys[1]},
		{keys[0], keys[0]},
		{"create_post", keys[0], "manage_team"},
	}
	for _, in := range extra {
		input := append([]string(nil), in...)
		result := model.AddAncillaryPermissions(input)
		out = append(out, map[string]any{
			"in":      in,
			"out":     result,
			"out_nil": result == nil,
		})
	}
	return out
}
