package main

// Behavioural oracle for the two decisions `PUT /teams/{team_id}/privacy` makes, written to
// fixtures/behaviour_team_privacy.json.
//
// Neither decision can be *driven* from here — `App.UpdateTeamPrivacy` needs a database and
// `updateTeamPrivacy` needs an HTTP context — so both expressions are **transcribed verbatim**
// from the Go source and evaluated by the Go compiler over every input combination. That is
// weaker than driving the real function and stronger than reading it: it pins the transcription,
// not the call. If either Go site changes shape, this file has to be re-copied for the fixture to
// move, so a stale oracle is a silent one. Said plainly rather than implied.
//
//   - `regenerates_invite_id` copies app/team.go:237 —
//     `(allowOpenInvite != oldTeam.AllowOpenInvite || teamType != oldTeam.Type) &&
//     (!allowOpenInvite || teamType == model.TeamInvite)`.
//     Two ANDed disjunctions. The left half is "something changed", the right half is "the result
//     is not an open team", and getting either backwards leaves a live invite link on a team that
//     was just closed. Enumerated over all 2x2x2x2 combinations of the four inputs.
//
//   - `privacy_switch` copies api4/team.go:596 — the `switch privacy` that turns one request
//     string into both `teamType` and `openInvite`. Only "O" and "I" are accepted; everything
//     else, lower case and the channel privacy letter "P" included, is `SetInvalidParam`.
//
// Determinism: fixed inputs only.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

type teamPrivacyRegenCase struct {
	Name            string `json:"name"`
	AllowOpenInvite bool   `json:"allow_open_invite"`
	TeamType        string `json:"team_type"`
	OldAllowOpen    bool   `json:"old_allow_open_invite"`
	OldTeamType     string `json:"old_team_type"`
	Regenerates     bool   `json:"regenerates"`
}

// Verbatim from app/team.go:237.
func teamPrivacyRegenerates(allowOpenInvite bool, teamType string, oldAllowOpenInvite bool, oldType string) bool {
	return (allowOpenInvite != oldAllowOpenInvite || teamType != oldType) && (!allowOpenInvite || teamType == model.TeamInvite)
}

func teamPrivacyRegenCorpus() []teamPrivacyRegenCase {
	types := []string{model.TeamOpen, model.TeamInvite}
	bools := []bool{false, true}

	out := make([]teamPrivacyRegenCase, 0, 16)
	for _, newOpen := range bools {
		for _, newType := range types {
			for _, oldOpen := range bools {
				for _, oldType := range types {
					name := "new_" + newType + boolTag(newOpen) + "_old_" + oldType + boolTag(oldOpen)
					out = append(out, teamPrivacyRegenCase{
						Name:            name,
						AllowOpenInvite: newOpen,
						TeamType:        newType,
						OldAllowOpen:    oldOpen,
						OldTeamType:     oldType,
						Regenerates:     teamPrivacyRegenerates(newOpen, newType, oldOpen, oldType),
					})
				}
			}
		}
	}
	return out
}

func boolTag(b bool) string {
	if b {
		return "_open"
	}
	return "_closed"
}

type teamPrivacySwitchCase struct {
	Privacy    string `json:"privacy"`
	Accepted   bool   `json:"accepted"`
	TeamType   string `json:"team_type"`
	OpenInvite bool   `json:"open_invite"`
}

// Verbatim from api4/team.go:596.
func teamPrivacySwitch(privacy string) (string, bool, bool) {
	var openInvite bool
	switch privacy {
	case model.TeamOpen:
		openInvite = true
	case model.TeamInvite:
		openInvite = false
	default:
		return "", false, false
	}
	return privacy, openInvite, true
}

func teamPrivacySwitchCorpus() []teamPrivacySwitchCase {
	inputs := []string{"O", "I", "o", "i", "P", "", "OI", "open", " O"}
	out := make([]teamPrivacySwitchCase, 0, len(inputs))
	for _, in := range inputs {
		teamType, openInvite, ok := teamPrivacySwitch(in)
		out = append(out, teamPrivacySwitchCase{
			Privacy:    in,
			Accepted:   ok,
			TeamType:   teamType,
			OpenInvite: openInvite,
		})
	}
	return out
}

func writeTeamPrivacyBehaviourFixture(outDir string) error {
	out := map[string]any{
		"regenerates_invite_id": teamPrivacyRegenCorpus(),
		"privacy_switch":        teamPrivacySwitchCorpus(),
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_team_privacy.json"), append(blob, '\n'), 0o644)
}
