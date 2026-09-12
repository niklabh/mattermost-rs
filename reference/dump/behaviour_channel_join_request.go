package main

// Behavioural oracle for model/channel_join_request.go, written to
// fixtures/behaviour_channel_join_request.json.
//
// Three methods with branches and one free function:
//
//   - `IsValid` — eleven refusals in a fixed order. Two of them are *cross-field*, and those are
//     the ones a reader gets wrong: a `DenialReason` is only legal on a `denied` request, and
//     `approved`/`denied` both require **both** `ReviewedBy` and `ReviewedAt`. `withdrawn`
//     requires neither, because the requester withdraws their own row.
//   - `PreSave` — fills four fields conditionally and **overwrites `UpdateAt` unconditionally**,
//     so re-saving an existing row rewinds it to `CreateAt`. That assignment is outside every `if`
//     and is easy to read as being inside the `CreateAt` one.
//   - `PreUpdate` — stamps `UpdateAt` and re-sanitises the two free-text fields.
//   - `IsValidChannelJoinRequestStatus` — the four-value allowlist, which is **not** the same set
//     the review patch accepts (that one takes only `approved` and `denied`).
//
// # The rune caps are runes, not bytes
//
// `utf8.RuneCountInString(r.Message) > 500` — so 500 four-byte emoji pass and 501 ASCII do not.
// Driven at both boundaries in both encodings, because a port reaching for `len()` passes every
// ASCII test.
//
// # `SanitizeUnicode` runs on save and on update, and it is not a no-op
//
// It strips the four Unicode *noncharacters* Mattermost refuses (`U+FFFE`, `U+FFFF` and the
// U+xFFFE/U+xFFFF pairs on every plane). That runs **after** the length cap is not yet applied —
// `PreSave` sanitises and `IsValid` counts, in that order at the store — so a message of 501
// characters four of which are stripped is *valid*. Recorded rather than reasoned about.
//
// Determinism: `PreSave` mints an id and reads the clock when they are empty, so those rows are
// recorded as properties (`id_minted`, `create_at_positive`) rather than values. Every other row
// supplies both, which makes the whole corpus fixed.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

const (
	cjrID      = "n9p1jbxkc6jdq3g3r1n6w3w1gh"
	cjrChannel = "u4pxb4wyrg6qowch6dieyeih7y"
	cjrUser    = "zbqemsqnsnums446ozchfcpgzc"
	cjrRevBy   = "p4fr57zha4gsyeccjz5x3thpcy"
)

func writeChannelJoinRequestBehaviourFixture(outDir string) error {
	out := map[string]any{
		"is_valid":    cjrIsValidAll(),
		"pre_save":    cjrPreSaveAll(),
		"pre_update":  cjrPreUpdateAll(),
		"status_enum": cjrStatusEnumAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_join_request.json"), append(blob, '\n'), 0o644)
}

// cjrValid is the row every IsValid case starts from: a pending request that passes.
func cjrValid() model.ChannelJoinRequest {
	return model.ChannelJoinRequest{
		Id:        cjrID,
		ChannelId: cjrChannel,
		UserId:    cjrUser,
		Message:   "let me in",
		Status:    model.ChannelJoinRequestStatusPending,
		CreateAt:  1700442228000,
		UpdateAt:  1704027533000,
	}
}

// --- IsValid -------------------------------------------------------------------------------

func cjrIsValidAll() []map[string]any {
	corpus := []struct {
		name string
		with func(*model.ChannelJoinRequest)
	}{
		{"valid_pending", func(r *model.ChannelJoinRequest) {}},
		{"valid_withdrawn_without_reviewer", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusWithdrawn
		}},
		{"valid_approved_with_reviewer", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusApproved
			r.ReviewedBy = cjrRevBy
			r.ReviewedAt = 1701843785000
		}},
		{"valid_denied_with_reason", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusDenied
			r.DenialReason = "not now"
			r.ReviewedBy = cjrRevBy
			r.ReviewedAt = 1701843785000
		}},

		// --- the eleven refusals, in Go's order ---
		{"empty_id", func(r *model.ChannelJoinRequest) { r.Id = "" }},
		{"short_id", func(r *model.ChannelJoinRequest) { r.Id = "abc" }},
		{"empty_channel_id", func(r *model.ChannelJoinRequest) { r.ChannelId = "" }},
		{"empty_user_id", func(r *model.ChannelJoinRequest) { r.UserId = "" }},
		{"zero_create_at", func(r *model.ChannelJoinRequest) { r.CreateAt = 0 }},
		{"zero_update_at", func(r *model.ChannelJoinRequest) { r.UpdateAt = 0 }},
		{"empty_status", func(r *model.ChannelJoinRequest) { r.Status = "" }},
		{"unknown_status", func(r *model.ChannelJoinRequest) { r.Status = "queued" }},
		{"uppercase_status", func(r *model.ChannelJoinRequest) { r.Status = "Pending" }},

		// The order of the first two: an empty id refuses before an empty channel id, and its
		// details field is empty where every later one carries `id=`.
		{"empty_id_and_channel", func(r *model.ChannelJoinRequest) { r.Id = ""; r.ChannelId = "" }},

		// --- the rune caps, at both boundaries and in both encodings ---
		{"message_500_ascii", func(r *model.ChannelJoinRequest) { r.Message = strings.Repeat("a", 500) }},
		{"message_501_ascii", func(r *model.ChannelJoinRequest) { r.Message = strings.Repeat("a", 501) }},
		{"message_500_emoji", func(r *model.ChannelJoinRequest) { r.Message = strings.Repeat("😀", 500) }},
		{"message_501_emoji", func(r *model.ChannelJoinRequest) { r.Message = strings.Repeat("😀", 501) }},
		{"denial_reason_500", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusDenied
			r.ReviewedBy = cjrRevBy
			r.ReviewedAt = 1
			r.DenialReason = strings.Repeat("b", 500)
		}},
		{"denial_reason_501", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusDenied
			r.ReviewedBy = cjrRevBy
			r.ReviewedAt = 1
			r.DenialReason = strings.Repeat("b", 501)
		}},
		// The length check runs before the status cross-check, so an over-long reason on a
		// *pending* request reports `denial_reason` and not `denial_reason_status`.
		{"denial_reason_501_on_pending", func(r *model.ChannelJoinRequest) {
			r.DenialReason = strings.Repeat("b", 501)
		}},

		// --- the two cross-field rules ---
		{"reason_on_pending", func(r *model.ChannelJoinRequest) { r.DenialReason = "why" }},
		{"reason_on_approved", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusApproved
			r.ReviewedBy = cjrRevBy
			r.ReviewedAt = 1
			r.DenialReason = "why"
		}},
		{"reason_on_withdrawn", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusWithdrawn
			r.DenialReason = "why"
		}},
		{"bad_reviewed_by", func(r *model.ChannelJoinRequest) { r.ReviewedBy = "nope" }},
		// An *empty* ReviewedBy skips the id check — the guard is `!= "" && !IsValidId`.
		{"empty_reviewed_by_on_pending", func(r *model.ChannelJoinRequest) { r.ReviewedBy = "" }},
		{"approved_without_reviewer", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusApproved
		}},
		{"approved_without_reviewed_at", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusApproved
			r.ReviewedBy = cjrRevBy
		}},
		{"approved_without_reviewed_by", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusApproved
			r.ReviewedAt = 1701843785000
		}},
		{"denied_without_reviewer", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusDenied
		}},
		// The status that does **not** demand a reviewer, which is the asymmetry worth pinning.
		{"withdrawn_without_reviewer", func(r *model.ChannelJoinRequest) {
			r.Status = model.ChannelJoinRequestStatusWithdrawn
		}},
		{"pending_with_reviewer", func(r *model.ChannelJoinRequest) {
			r.ReviewedBy = cjrRevBy
			r.ReviewedAt = 1701843785000
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		req := cjrValid()
		c.with(&req)
		row := map[string]any{"name": c.name, "in": mustMarshal(&req)}
		probe(row, func() {
			appErr := req.IsValid()
			row["ok"] = appErr == nil
			if appErr != nil {
				row["id"] = appErr.Id
				row["status_code"] = appErr.StatusCode
				row["where"] = appErr.Where
				row["detailed_error"] = appErr.DetailedError
			} else {
				row["id"] = nil
				row["status_code"] = nil
				row["where"] = nil
				row["detailed_error"] = nil
			}
		})
		res = append(res, row)
	}
	return res
}

// --- PreSave ------------------------------------------------------------------------------

func cjrPreSaveAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.ChannelJoinRequest
	}{
		// Everything supplied: nothing is minted, and UpdateAt is still overwritten.
		{"all_supplied", model.ChannelJoinRequest{
			Id: cjrID, ChannelId: cjrChannel, UserId: cjrUser,
			Message: "hi", Status: model.ChannelJoinRequestStatusApproved,
			CreateAt: 111, UpdateAt: 999,
		}},
		// The overwrite on its own: a much later UpdateAt is rewound to CreateAt.
		{"update_at_rewound", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, UpdateAt: 9999999,
		}},
		{"status_defaulted", model.ChannelJoinRequest{Id: cjrID, CreateAt: 100}},
		// A status that is already set is kept, even one the save path would never produce.
		{"status_kept", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, Status: model.ChannelJoinRequestStatusWithdrawn,
		}},
		{"sanitises_message", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, Message: "a￾b￿c",
		}},
		{"sanitises_denial_reason", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, DenialReason: "x￾y",
		}},
		// A noncharacter on a higher plane — the loop covers all seventeen, not just the BMP.
		{"sanitises_astral_noncharacter", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, Message: "a\U0001FFFEb",
		}},
		// A zero-width space is **not** a noncharacter and survives.
		{"keeps_zero_width_space", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, Message: "a​b",
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		req := c.in
		row := map[string]any{"name": c.name, "in": mustMarshal(&req)}
		probe(row, func() {
			req.PreSave()
			row["json"] = mustMarshal(&req)
			row["id"] = req.Id
			row["status"] = req.Status
			row["create_at"] = req.CreateAt
			row["update_at"] = req.UpdateAt
			row["message"] = req.Message
			row["denial_reason"] = req.DenialReason
		})
		res = append(res, row)
	}

	// The two minted fields, recorded as properties rather than values.
	minted := model.ChannelJoinRequest{}
	mrow := map[string]any{"name": "mints_id_and_create_at", "in": mustMarshal(&minted)}
	probe(mrow, func() {
		minted.PreSave()
		mrow["id_len"] = len(minted.Id)
		mrow["id_is_valid"] = model.IsValidId(minted.Id)
		mrow["create_at_positive"] = minted.CreateAt > 0
		mrow["update_at_equals_create_at"] = minted.UpdateAt == minted.CreateAt
		mrow["status"] = minted.Status
	})
	res = append(res, mrow)

	return res
}

// --- PreUpdate ----------------------------------------------------------------------------

func cjrPreUpdateAll() []map[string]any {
	var res []map[string]any

	// PreUpdate reads the clock unconditionally, so everything here is a property.
	corpus := []struct {
		name string
		in   model.ChannelJoinRequest
	}{
		{"stamps_update_at", model.ChannelJoinRequest{Id: cjrID, CreateAt: 100, UpdateAt: 100}},
		{"sanitises_both_fields", model.ChannelJoinRequest{
			Id: cjrID, CreateAt: 100, Message: "m￾m", DenialReason: "d￿d",
		}},
		{"leaves_create_at_alone", model.ChannelJoinRequest{Id: cjrID, CreateAt: 12345}},
		{"leaves_status_alone", model.ChannelJoinRequest{Id: cjrID, Status: ""}},
	}
	for _, c := range corpus {
		req := c.in
		before := req.CreateAt
		row := map[string]any{"name": c.name, "in": mustMarshal(&req)}
		probe(row, func() {
			req.PreUpdate()
			row["update_at_positive"] = req.UpdateAt > 0
			row["create_at_unchanged"] = req.CreateAt == before
			row["status"] = req.Status
			row["message"] = req.Message
			row["denial_reason"] = req.DenialReason
		})
		res = append(res, row)
	}
	return res
}

// --- IsValidChannelJoinRequestStatus -------------------------------------------------------

func cjrStatusEnumAll() []map[string]any {
	values := []string{
		"pending", "approved", "denied", "withdrawn",
		"", "Pending", "PENDING", "pending ", " pending", "queued", "rejected", "approve",
	}
	var res []map[string]any
	for _, v := range values {
		row := map[string]any{"in": v}
		probe(row, func() {
			row["ok"] = model.IsValidChannelJoinRequestStatus(v)
		})
		res = append(res, row)
	}
	return res
}
