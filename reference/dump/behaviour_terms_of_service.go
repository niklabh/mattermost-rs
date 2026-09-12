package main

// Behavioural oracle for `model/terms_of_service.go` and `model/user_terms_of_service.go`.
//
// Both types are four fields and a validator, and both validators are reached from a write route
// (`POST /api/v4/terms_of_service`, `POST /api/v4/users/{user_id}/terms_of_service`) through the
// store's `Save`. The branches are where the interest is, and two of them are the kind a careful
// reader still gets wrong:
//
//   - `TermsOfService.IsValid` builds `MaxLength` into its i18n params on **every** branch,
//     including the three that have nothing to do with length. `AppError.params` is unexported
//     (utils.go:240), so that one cannot be read from here and is transcribed on the Rust side;
//     everything else below is measured.
//   - `UserTermsOfService.IsValid` passes `ut.UserId` as the detail on **all three** branches, so
//     the `terms_of_service_id` failure reports the user id and never the value it rejected.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeTermsOfServiceBehaviourFixture(outDir string) error {
	out := map[string]any{
		"terms_of_service_is_valid":      termsOfServiceIsValidAll(),
		"terms_of_service_pre_save":      termsOfServicePreSaveAll(),
		"user_terms_of_service_is_valid": userTermsOfServiceIsValidAll(),
		"user_terms_of_service_pre_save": userTermsOfServicePreSaveAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_terms_of_service.json"), append(blob, '\n'), 0o644)
}

type tosValidCase struct {
	Name    string          `json:"name"`
	Subject json.RawMessage `json:"subject"`
	// `Text` is blanked in `Subject` when it is long, so the fixture stays readable. These two
	// say how to rebuild it: `TextRepeatCount` copies of `TextRepeatRune`.
	TextRepeatRune  string `json:"text_repeat_rune"`
	TextRepeatCount int    `json:"text_repeat_count"`
	ErrorID  string          `json:"error_id"`
	Detailed string          `json:"detailed"`
	Status   int             `json:"status_code"`
	Where    string          `json:"where"`
}

func termsOfServiceIsValidAll() []tosValidCase {
	muts := []struct {
		name string
		fn   func(t *model.TermsOfService)
	}{
		{"valid", func(t *model.TermsOfService) {}},
		{"id_empty", func(t *model.TermsOfService) { t.Id = "" }},
		{"id_short", func(t *model.TermsOfService) { t.Id = repeat("a", 25) }},
		{"create_at_zero", func(t *model.TermsOfService) { t.CreateAt = 0 }},
		{"user_id_empty", func(t *model.TermsOfService) { t.UserId = "" }},
		{"user_id_nonsense", func(t *model.TermsOfService) { t.UserId = "nope" }},
		{"text_empty", func(t *model.TermsOfService) { t.Text = "" }},
		{"text_at_limit", func(t *model.TermsOfService) { t.Text = repeat("a", model.PostMessageMaxRunesV2) }},
		{"text_over_limit", func(t *model.TermsOfService) { t.Text = repeat("a", model.PostMessageMaxRunesV2+1) }},
		// Runes, not bytes: the same rune count in multi-byte characters still passes.
		{"text_at_limit_multibyte", func(t *model.TermsOfService) { t.Text = repeat("é", model.PostMessageMaxRunesV2) }},
		{"text_over_limit_multibyte", func(t *model.TermsOfService) { t.Text = repeat("é", model.PostMessageMaxRunesV2+1) }},
		// The order of the checks: a row wrong in two ways reports the first.
		{"id_and_create_at_both_wrong", func(t *model.TermsOfService) { t.Id = ""; t.CreateAt = 0 }},
		{"create_at_and_user_id_both_wrong", func(t *model.TermsOfService) { t.CreateAt = 0; t.UserId = "" }},
	}

	var res []tosValidCase
	for _, m := range muts {
		t := &model.TermsOfService{
			Id:       idA,
			CreateAt: 1700000000000,
			UserId:   idB,
			Text:     "the terms",
		}
		m.fn(t)

		// The text fields make the fixture enormous if echoed; record how to rebuild them.
		echo := *t
		repeatRune, repeatCount := "", 0
		if len([]rune(echo.Text)) > 64 {
			repeatRune = string([]rune(echo.Text)[0])
			repeatCount = len([]rune(echo.Text))
			echo.Text = ""
		}
		blob, err := json.Marshal(&echo)
		if err != nil {
			panic(err)
		}

		c := tosValidCase{
			Name:            m.name,
			Subject:         blob,
			TextRepeatRune:  repeatRune,
			TextRepeatCount: repeatCount,
		}
		if appErr := t.IsValid(); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
			c.Status = appErr.StatusCode
			c.Where = appErr.Where
		}
		res = append(res, c)
	}
	return res
}

type tosPreSaveCase struct {
	Name        string `json:"name"`
	InID        string `json:"in_id"`
	IDPreserved bool   `json:"id_preserved"`
	IDGenerated bool   `json:"id_generated"`
	// CreateAt comes from GetMillis() and cannot be pinned; this records that it moved.
	CreateOverwritten bool `json:"create_at_overwritten"`
}

func termsOfServicePreSaveAll() []tosPreSaveCase {
	cases := []string{"", idA}
	var res []tosPreSaveCase
	for _, id := range cases {
		t := &model.TermsOfService{Id: id, CreateAt: 12345, UserId: idB, Text: "x"}
		t.PreSave()
		name := "empty_id_is_generated"
		if id != "" {
			name = "existing_id_kept"
		}
		res = append(res, tosPreSaveCase{
			Name:              name,
			InID:              id,
			IDPreserved:       id != "" && t.Id == id,
			IDGenerated:       id == "" && len(t.Id) == 26,
			CreateOverwritten: t.CreateAt != 12345,
		})
	}
	return res
}

func userTermsOfServiceIsValidAll() []tosValidCase {
	muts := []struct {
		name string
		fn   func(u *model.UserTermsOfService)
	}{
		{"valid", func(u *model.UserTermsOfService) {}},
		{"user_id_empty", func(u *model.UserTermsOfService) { u.UserId = "" }},
		{"user_id_short", func(u *model.UserTermsOfService) { u.UserId = repeat("a", 25) }},
		{"user_id_nonsense", func(u *model.UserTermsOfService) { u.UserId = "nope" }},
		{"terms_id_empty", func(u *model.UserTermsOfService) { u.TermsOfServiceId = "" }},
		{"terms_id_short", func(u *model.UserTermsOfService) { u.TermsOfServiceId = repeat("a", 25) }},
		{"create_at_zero", func(u *model.UserTermsOfService) { u.CreateAt = 0 }},
		// Both ids wrong: the user id is reported first.
		{"both_ids_wrong", func(u *model.UserTermsOfService) { u.UserId = ""; u.TermsOfServiceId = "" }},
		// The detail on the terms-id branch is the *user* id, which this row makes visible.
		{"terms_id_empty_user_id_present", func(u *model.UserTermsOfService) { u.TermsOfServiceId = "" }},
	}

	var res []tosValidCase
	for _, m := range muts {
		u := &model.UserTermsOfService{
			UserId:           idA,
			TermsOfServiceId: idB,
			CreateAt:         1700000000000,
		}
		m.fn(u)

		blob, err := json.Marshal(u)
		if err != nil {
			panic(err)
		}
		c := tosValidCase{Name: m.name, Subject: blob}
		if appErr := u.IsValid(); appErr != nil {
			c.ErrorID = appErr.Id
			c.Detailed = appErr.DetailedError
			c.Status = appErr.StatusCode
			c.Where = appErr.Where
		}
		res = append(res, c)
	}
	return res
}

func userTermsOfServicePreSaveAll() []tosPreSaveCase {
	cases := []string{"", idA}
	var res []tosPreSaveCase
	for _, id := range cases {
		u := &model.UserTermsOfService{UserId: id, TermsOfServiceId: idB, CreateAt: 12345}
		u.PreSave()
		name := "empty_user_id_is_generated"
		if id != "" {
			name = "existing_user_id_kept"
		}
		res = append(res, tosPreSaveCase{
			Name:              name,
			InID:              id,
			IDPreserved:       id != "" && u.UserId == id,
			IDGenerated:       id == "" && len(u.UserId) == 26,
			CreateOverwritten: u.CreateAt != 12345,
		})
	}
	return res
}
