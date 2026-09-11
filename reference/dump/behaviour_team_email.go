package main

// Behavioural oracle for `teams.IsEmailAddressAllowed` (channels/app/teams/utils.go:38) and the
// unexported `normalizeDomains` it calls, written to fixtures/behaviour_team_email.json.
//
// Driven because `App.JoinUserToTeam` refuses a team join outright when it says no
// (`AcceptedDomainError` → `api.team.join_user_to_team.allowed_domains.app_error`, 400), and the
// function's shape is easy to read backwards in three separate places:
//
//   - **The restriction list is an AND, not an OR.** A non-member's list is
//     `[team.AllowedDomains, TeamSettings.RestrictCreationToDomains]` and *every non-empty entry*
//     must be satisfied. So a team that allows `example.com` on a server restricted to
//     `corp.example.com` admits nobody at all. The natural "any of these lists accepts it"
//     reading is the opposite answer for exactly the configuration an administrator is most
//     likely to set.
//   - **An empty restriction is skipped, not matched.** `len(domains) <= 0 { continue }`, which
//     is why the stock configuration (`""` everywhere) admits everybody rather than nobody.
//   - **The suffix carries the `@`.** `strings.HasSuffix(email, "@"+d)` — so `example.com` admits
//     neither `me@sub.example.com` nor `me@notexample.com`. Dropping the `@` admits both, and no
//     test written against ordinary addresses would notice.
//
// `normalizeDomains` is exercised through the same entry point, since it is unexported: `@` and
// `,` both become spaces, the whole string is lower-cased, and the result is `strings.Fields`.
// That makes `"@corp.example.com, example.com"` two domains, and a stray `@` *inside* a name
// split it in two rather than being stripped — `"a@b"` is the two domains `a` and `b`.
//
// The caller lower-cases the address before calling (`strings.ToLower(user.Email)`), so the
// corpus feeds lower-cased addresses except where an upper-cased one is the point: the function
// itself does **not** lower-case its `email` argument, only the domains.
//
// Determinism: fixed inputs only.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/v8/channels/app/teams"
)

type teamEmailCase struct {
	Name         string   `json:"name"`
	Email        string   `json:"email"`
	Restrictions []string `json:"restrictions"`
	Allowed      bool     `json:"allowed"`
}

func teamEmailCorpus() []teamEmailCase {
	cases := []struct {
		name         string
		email        string
		restrictions []string
	}{
		// --- the stock configuration: two empty restrictions ----------------------------------
		{"no_restrictions_at_all", "me@example.com", []string{"", ""}},
		{"empty_list", "me@example.com", []string{}},
		{"one_empty_one_set_matching", "me@example.com", []string{"", "example.com"}},
		{"one_empty_one_set_failing", "me@other.com", []string{"", "example.com"}},
		{"whitespace_only_restriction_is_empty", "me@other.com", []string{"   ", ""}},

		// --- the AND across restrictions -------------------------------------------------------
		{"both_lists_accept", "me@example.com", []string{"example.com", "example.com"}},
		{"first_accepts_second_refuses", "me@example.com", []string{"example.com", "corp.example.com"}},
		{"second_accepts_first_refuses", "me@corp.example.com", []string{"example.com", "corp.example.com"}},
		{"neither_accepts", "me@nope.com", []string{"example.com", "corp.example.com"}},
		{"both_name_it_among_others", "me@example.com", []string{"a.com example.com", "example.com b.com"}},

		// --- the OR inside one restriction -----------------------------------------------------
		{"second_domain_of_one_restriction", "me@b.com", []string{"a.com b.com c.com", ""}},
		{"none_of_one_restriction", "me@d.com", []string{"a.com b.com c.com", ""}},

		// --- the `@` in the suffix -------------------------------------------------------------
		{"subdomain_is_not_the_domain", "me@sub.example.com", []string{"example.com", ""}},
		{"suffix_without_at_is_not_a_match", "me@notexample.com", []string{"example.com", ""}},
		{"the_domain_itself_as_the_whole_address", "example.com", []string{"example.com", ""}},
		{"empty_local_part_still_matches", "@example.com", []string{"example.com", ""}},

		// --- normalizeDomains ------------------------------------------------------------------
		{"leading_at_is_stripped", "me@example.com", []string{"@example.com", ""}},
		{"comma_separated", "me@b.com", []string{"a.com,b.com", ""}},
		{"comma_and_space_separated", "me@b.com", []string{"@a.com, b.com", ""}},
		{"runs_of_whitespace", "me@b.com", []string{"a.com \t\n b.com", ""}},
		{"upper_case_restriction", "me@example.com", []string{"EXAMPLE.COM", ""}},
		{"upper_case_email_is_not_lowered_here", "ME@EXAMPLE.COM", []string{"example.com", ""}},
		{"at_inside_a_name_splits_it", "me@b", []string{"a@b", ""}},
		{"at_inside_a_name_splits_it_other_half", "me@a", []string{"a@b", ""}},
		{"only_commas", "me@example.com", []string{",,,", ""}},
		{"only_at_signs", "me@example.com", []string{"@@@", ""}},

		// --- the guest path, which passes exactly one restriction --------------------------------
		{"guest_single_empty_restriction", "guest@anywhere.com", []string{""}},
		{"guest_single_matching", "guest@example.com", []string{"example.com"}},
		{"guest_single_refusing", "guest@other.com", []string{"example.com"}},
	}

	out := make([]teamEmailCase, 0, len(cases))
	for _, c := range cases {
		out = append(out, teamEmailCase{
			Name:         c.name,
			Email:        c.email,
			Restrictions: c.restrictions,
			Allowed:      teams.IsEmailAddressAllowed(c.email, c.restrictions),
		})
	}
	return out
}

func writeTeamEmailBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_email_address_allowed": teamEmailCorpus(),
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_team_email.json"), append(blob, '\n'), 0o644)
}
