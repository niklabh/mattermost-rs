package main

// Behavioural oracle for model/scheduled_post.go, written to
// fixtures/behaviour_scheduled_post.json.
//
// `ScheduledPost` **embeds `Draft`**, which is the first anonymous struct field in the ported
// tree and changes three things at once:
//
//  1. **The wire form is one flat object.** Go inlines an embedded struct's keys into the
//     parent, so a scheduled post is Draft's nine keys followed by its own six. Field order is
//     emission order, so the embedded half comes first — which a `#[serde(flatten)]` port does
//     NOT reproduce, because serde emits flattened fields last. The wire probes are recorded
//     byte-exact for that reason.
//
//  2. **Draft's methods are promoted.** `s.Message`, `s.FileIds` and `s.GetProps()` in this file
//     all reach through the embed, and `IsValid` calls `s.Draft.IsValid` explicitly *and*
//     `s.BaseIsValid`, which itself calls `s.Draft.BaseIsValid`. So Draft's message-length check
//     runs once and its base checks run twice.
//
//  3. **`Id` is Draft's missing field.** A draft has no id; a scheduled post does, and it is
//     validated here rather than in Draft.
//
// The traps, all measured:
//
//   - `scheduledPostMaxTimeGap` is **unexported** and negative (-5000): a scheduled_at up to five
//     seconds in the past is accepted. Extracted with go/parser rather than transcribed, the way
//     behaviour_version.go recovers `versions` — see [D-021].
//
//   - **`time.LoadLocation` reads the HOST's tzdata**, so the accepted set of `repeat_timezone`
//     values is a deployment artifact, not a property of Go. Same shape of problem as
//     `mime.TypeByExtension` in [D-030]. The corpus records what the generating machine answered,
//     which is evidence rather than a target.
//
//   - **`ToPost`'s three priority assertions are all-or-nothing.** A priority map missing
//     `requested_ack` fails the whole conversion, because the type assertion on an absent key
//     yields the zero value with ok=false. So `{"priority":"urgent"}` alone is an error.
//
//   - **`ToPost` drops `Priority` when the map is empty but keeps a nil `Metadata` nil**, and it
//     does NOT carry `Id`, `CreateAt`, `UpdateAt` or the draft's `DeleteAt`.

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"strconv"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeScheduledPostBehaviourFixture(outDir string) error {
	gap, err := parseScheduledPostMaxTimeGap("../mattermost/server/public/model/scheduled_post.go")
	if err != nil {
		return err
	}

	out := map[string]any{
		"constants":                    scheduledPostConstants(gap),
		"wire":                         scheduledPostWireAll(),
		"is_valid":                     scheduledPostIsValidAll(),
		"timezones":                    scheduledPostTimezonesAll(),
		"pre_save":                     scheduledPostPreSaveAll(),
		"pre_update":                   scheduledPostPreUpdateAll(),
		"to_post":                      scheduledPostToPostAll(),
		"restore_non_updatable_fields": scheduledPostRestoreAll(),
		"sanitize_input":               scheduledPostSanitizeAll(),
		"get_priority":                 scheduledPostGetPriorityAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_scheduled_post.json"), append(blob, '\n'), 0o644)
}

// --- constants -------------------------------------------------------------------------------

// parseScheduledPostMaxTimeGap reads `const scheduledPostMaxTimeGap = -5000` out of the source.
// It is unexported, so calling the package cannot recover it, and the Rust port has to hold the
// number — transcribing it unchecked is exactly what this technique exists to avoid.
func parseScheduledPostMaxTimeGap(path string) (int64, error) {
	fset := token.NewFileSet()
	file, err := parser.ParseFile(fset, path, nil, 0)
	if err != nil {
		return 0, fmt.Errorf("parsing %s: %w", path, err)
	}

	for _, decl := range file.Decls {
		gen, ok := decl.(*ast.GenDecl)
		if !ok || gen.Tok != token.CONST {
			continue
		}
		for _, spec := range gen.Specs {
			value, ok := spec.(*ast.ValueSpec)
			if !ok || len(value.Names) != 1 || value.Names[0].Name != "scheduledPostMaxTimeGap" {
				continue
			}
			if len(value.Values) != 1 {
				return 0, fmt.Errorf("scheduledPostMaxTimeGap has %d values", len(value.Values))
			}
			// The literal is `-5000`, i.e. a unary expression over a basic literal.
			unary, ok := value.Values[0].(*ast.UnaryExpr)
			if !ok || unary.Op != token.SUB {
				return 0, fmt.Errorf("scheduledPostMaxTimeGap is not a negated literal")
			}
			basic, ok := unary.X.(*ast.BasicLit)
			if !ok || basic.Kind != token.INT {
				return 0, fmt.Errorf("scheduledPostMaxTimeGap is not an int literal")
			}
			n, err := strconv.ParseInt(basic.Value, 10, 64)
			if err != nil {
				return 0, err
			}
			return -n, nil
		}
	}
	return 0, fmt.Errorf("scheduledPostMaxTimeGap not found in %s", path)
}

func scheduledPostConstants(gap int64) map[string]any {
	return map[string]any{
		"ScheduledPostErrorUnknownError":            model.ScheduledPostErrorUnknownError,
		"ScheduledPostErrorCodeChannelArchived":     model.ScheduledPostErrorCodeChannelArchived,
		"ScheduledPostErrorCodeRestrictedDM":        model.ScheduledPostErrorCodeRestrictedDM,
		"ScheduledPostErrorCodeChannelNotFound":     model.ScheduledPostErrorCodeChannelNotFound,
		"ScheduledPostErrorCodeUserDoesNotExist":    model.ScheduledPostErrorCodeUserDoesNotExist,
		"ScheduledPostErrorCodeUserDeleted":         model.ScheduledPostErrorCodeUserDeleted,
		"ScheduledPostErrorCodeNoChannelPermission": model.ScheduledPostErrorCodeNoChannelPermission,
		"ScheduledPostErrorNoChannelMember":         model.ScheduledPostErrorNoChannelMember,
		"ScheduledPostErrorThreadDeleted":           model.ScheduledPostErrorThreadDeleted,
		"ScheduledPostErrorUnableToSend":            model.ScheduledPostErrorUnableToSend,
		"ScheduledPostErrorInvalidPost":             model.ScheduledPostErrorInvalidPost,
		// scheduled_post_recurrence.go's, borrowed because IsValid's switch needs them.
		"ScheduledPostRepeatTypeNone":   model.ScheduledPostRepeatTypeNone,
		"ScheduledPostRepeatTypeWeekly": model.ScheduledPostRepeatTypeWeekly,
		// Unexported; read out of the source.
		"scheduledPostMaxTimeGap": gap,
	}
}

// --- wire ------------------------------------------------------------------------------------

func scheduledPostWireAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty_object", `{}`},
		{"all_fields", `{"create_at":1700000000000,"update_at":1700000001000,"delete_at":1700000002000,` +
			`"user_id":"` + idA + `","channel_id":"` + idB + `","root_id":"` + idC + `",` +
			`"message":"hello","type":"custom_x","props":{"a":"b"},"file_ids":["f1"],` +
			`"metadata":{"emojis":[{"name":"smile"}]},"priority":{"priority":"urgent"},` +
			`"id":"` + idA + `","scheduled_at":1700000003000,"processed_at":1700000004000,` +
			`"error_code":"unknown","repeat_type":"weekly","repeat_timezone":"UTC"}`},
		// The embed's own quirks travel with it: props has no omitempty, the other three do.
		{"props_null", `{"props":null,"id":"x"}`},
		{"props_empty", `{"props":{},"id":"x"}`},
		{"file_ids_empty", `{"file_ids":[],"id":"x"}`},
		{"metadata_empty", `{"metadata":{},"id":"x"}`},
		{"priority_empty", `{"priority":{},"id":"x"}`},
		// None of ScheduledPost's own six has omitempty, so all six are always present.
		{"own_fields_only", `{"id":"i","scheduled_at":1,"processed_at":2,"error_code":"e",` +
			`"repeat_type":"weekly","repeat_timezone":"UTC"}`},
		{"draft_fields_only", `{"message":"m","user_id":"u"}`},
		// A key that belongs to neither half.
		{"unknown_key", `{"nope":1,"id":"x"}`},
		{"escapes", `{"message":"a<b>c&d","repeat_timezone":"a&b"}`},
		// Go leaves the destination untouched on null; serde does not ([D-057]).
		{"null_scalars", `{"id":null,"scheduled_at":null,"message":null}`},
	}

	var res []map[string]any
	for _, c := range docs {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var sp model.ScheduledPost
			if err := json.Unmarshal([]byte(c.doc), &sp); err != nil {
				row["err"] = err.Error()
				return
			}
			row["err"] = nil
			row["out"] = mustMarshal(&sp)
			row["props_nil"] = sp.Props == nil
			row["file_ids_nil"] = sp.FileIds == nil
			row["priority_nil"] = sp.Priority == nil
			row["metadata_nil"] = sp.Metadata == nil
		})
		res = append(res, row)
	}

	res = append(res, map[string]any{
		"name": "zero_value", "in": "", "err": nil, "panicked": false,
		"out":       mustMarshal(&model.ScheduledPost{}),
		"props_nil": true, "file_ids_nil": true, "priority_nil": true, "metadata_nil": true,
	})
	return res
}

// --- IsValid / BaseIsValid -------------------------------------------------------------------

type scheduledPostValidCase struct {
	Name string          `json:"name"`
	Post json.RawMessage `json:"post"`
	// scheduled_at is clock-relative — IsValid compares it against GetMillis(). Recorded as an
	// offset from "now" so the fixture stays deterministic ([D-032]); the Rust side rebuilds it.
	// Every offset is at least a second clear of the -5000 boundary, so the microseconds between
	// building the case and validating it cannot flip an answer.
	ScheduledAtOffset int64 `json:"scheduled_at_offset"`
	MaxMessageSize    int   `json:"max_message_size"`

	Where    string `json:"where"`
	ErrorID  string `json:"error_id"`
	Detailed string `json:"detailed"`
	Status   int    `json:"status"`
	// BaseIsValid, which skips the Draft message-length check but still runs Draft.BaseIsValid.
	BaseWhere    string `json:"base_where"`
	BaseErrorID  string `json:"base_error_id"`
	BaseDetailed string `json:"base_detailed"`
}

func scheduledPostIsValidAll() []scheduledPostValidCase {
	type mut struct {
		name   string
		offset int64
		max    int
		fn     func(s *model.ScheduledPost)
	}
	const dflt = 4000
	const soon = int64(3600000) // an hour out: comfortably valid

	muts := []mut{
		{"valid", soon, dflt, func(s *model.ScheduledPost) {}},

		// Draft's checks run first, through the explicit s.Draft.IsValid call.
		{"draft_message_too_long", soon, 3, func(s *model.ScheduledPost) { s.Message = "abcd" }},
		{"draft_create_at_zero", soon, dflt, func(s *model.ScheduledPost) { s.CreateAt = 0 }},
		{"draft_update_at_zero", soon, dflt, func(s *model.ScheduledPost) { s.UpdateAt = 0 }},
		{"draft_user_id_bad", soon, dflt, func(s *model.ScheduledPost) { s.UserId = "nope" }},
		{"draft_channel_id_bad", soon, dflt, func(s *model.ScheduledPost) { s.ChannelId = "nope" }},
		{"draft_root_id_bad", soon, dflt, func(s *model.ScheduledPost) { s.RootId = "nope" }},

		// Id is ScheduledPost's own, and Draft has no such field.
		{"id_empty", soon, dflt, func(s *model.ScheduledPost) { s.Id = "" }},
		// Only emptiness is checked — it is never run through IsValidId.
		{"id_nonsense", soon, dflt, func(s *model.ScheduledPost) { s.Id = "nope" }},

		// An empty post: neither a message NOR a file. Either one alone is enough.
		{"empty_message_and_files", soon, dflt, func(s *model.ScheduledPost) {
			s.Message = ""
			s.FileIds = nil
		}},
		{"empty_message_with_files", soon, dflt, func(s *model.ScheduledPost) { s.Message = "" }},
		{"empty_files_with_message", soon, dflt, func(s *model.ScheduledPost) { s.FileIds = nil }},
		{"empty_message_empty_file_slice", soon, dflt, func(s *model.ScheduledPost) {
			s.Message = ""
			s.FileIds = model.StringArray{}
		}},
		// The message check is len() in BYTES, and a whitespace-only message is not empty.
		{"whitespace_message", soon, dflt, func(s *model.ScheduledPost) { s.Message = " " }},

		// scheduled_at, well clear of the five-second grace window in both directions.
		{"scheduled_far_future", 86400000, dflt, func(s *model.ScheduledPost) {}},
		{"scheduled_just_past_within_grace", -1000, dflt, func(s *model.ScheduledPost) {}},
		{"scheduled_past_beyond_grace", -60000, dflt, func(s *model.ScheduledPost) {}},
		{"scheduled_long_past", -86400000, dflt, func(s *model.ScheduledPost) {}},

		{"processed_at_zero", soon, dflt, func(s *model.ScheduledPost) { s.ProcessedAt = 0 }},
		{"processed_at_negative", soon, dflt, func(s *model.ScheduledPost) { s.ProcessedAt = -1 }},

		// repeat_type accepts exactly two values, one of which is the empty string.
		{"repeat_none", soon, dflt, func(s *model.ScheduledPost) { s.RepeatType = "" }},
		{"repeat_weekly", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.FileIds = nil
			s.RepeatTimezone = "UTC"
		}},
		{"repeat_daily_rejected", soon, dflt, func(s *model.ScheduledPost) { s.RepeatType = "daily" }},
		{"repeat_wrong_case", soon, dflt, func(s *model.ScheduledPost) { s.RepeatType = "Weekly" }},

		// A weekly repeat with files: rejected, because files bind to the first post.
		{"repeat_weekly_with_files", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.RepeatTimezone = "UTC"
		}},
		{"repeat_weekly_empty_file_slice", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.FileIds = model.StringArray{}
			s.RepeatTimezone = "UTC"
		}},
		{"repeat_weekly_no_timezone", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.FileIds = nil
		}},
		{"repeat_weekly_local_rejected", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.FileIds = nil
			s.RepeatTimezone = "Local"
		}},
		{"repeat_weekly_bad_timezone", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.FileIds = nil
			s.RepeatTimezone = "Nowhere/Nothing"
		}},
		{"repeat_weekly_iana_timezone", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatType = model.ScheduledPostRepeatTypeWeekly
			s.FileIds = nil
			s.RepeatTimezone = "America/New_York"
		}},
		// The timezone fields are ignored entirely when the repeat type is none.
		{"timezone_ignored_when_not_repeating", soon, dflt, func(s *model.ScheduledPost) {
			s.RepeatTimezone = "Nowhere/Nothing"
		}},
	}

	res := make([]scheduledPostValidCase, 0, len(muts))
	for _, m := range muts {
		s := newValidScheduledPost()
		s.ScheduledAt = model.GetMillis() + m.offset
		m.fn(s)

		validErr := s.IsValid(m.max)
		baseErr := s.BaseIsValid()

		// Blank the clock-derived value before marshalling so the fixture is deterministic.
		scheduledAt := s.ScheduledAt
		s.ScheduledAt = 0
		blob, err := json.Marshal(s)
		if err != nil {
			panic(err)
		}
		s.ScheduledAt = scheduledAt

		c := scheduledPostValidCase{
			Name: m.name, Post: blob, ScheduledAtOffset: m.offset, MaxMessageSize: m.max,
		}
		if validErr != nil {
			c.Where, c.ErrorID, c.Detailed, c.Status =
				validErr.Where, validErr.Id, validErr.DetailedError, validErr.StatusCode
		}
		if baseErr != nil {
			c.BaseWhere, c.BaseErrorID, c.BaseDetailed =
				baseErr.Where, baseErr.Id, baseErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

// newValidScheduledPost returns a post that passes IsValid with a future ScheduledAt. Built fresh
// per case: ScheduledPost embeds Draft, which embeds a sync.RWMutex, so a struct copy copies a
// lock.
func newValidScheduledPost() *model.ScheduledPost {
	s := &model.ScheduledPost{
		Id:             idA,
		ProcessedAt:    0,
		ErrorCode:      "",
		RepeatType:     model.ScheduledPostRepeatTypeNone,
		RepeatTimezone: "",
	}
	s.CreateAt = 1700000000000
	s.UpdateAt = 1700000001000
	s.UserId = idA
	s.ChannelId = idB
	s.RootId = idC
	s.Message = "hello"
	s.Props = model.StringInterface{"a": "b"}
	s.FileIds = model.StringArray{idA}
	return s
}

// --- time.LoadLocation -------------------------------------------------------------------------

// scheduledPostTimezonesAll records what THIS machine's tzdata accepts. time.LoadLocation reads
// $ZONEINFO and then the host's zoneinfo directory, so the answer is a deployment artifact rather
// than a property of Go — the same shape of problem as mime.TypeByExtension in [D-030]. Recorded
// as evidence for the Rust port's choice of timezone database, not as a target it must hit
// exactly.
func scheduledPostTimezonesAll() []map[string]any {
	names := []string{
		"", "UTC", "Local", "GMT", "EST", "MST", "HST", "EST5EDT",
		"America/New_York", "america/new_york", "AMERICA/NEW_YORK",
		"Europe/London", "Europe/Berlin", "Asia/Kolkata", "Asia/Calcutta",
		"Australia/Sydney", "Pacific/Auckland", "Africa/Cairo",
		"US/Pacific", "US/Eastern", "Canada/Eastern", "Etc/UTC", "Etc/GMT", "Etc/GMT+5",
		"Etc/GMT-5", "Zulu", "Universal", "Greenwich", "CET", "MET", "WET", "EET", "PST8PDT",
		"America/Argentina/Buenos_Aires", "Antarctica/Troll",
		// Deprecated links that some tzdata builds drop.
		"US/Pacific-New", "Asia/Rangoon", "Europe/Kiev", "Europe/Kyiv", "Pacific/Enderbury",
		// Rejections.
		"Nowhere/Nothing", "America/Nonexistent", "UTC+2", "+05:30", "utc",
		"../etc/passwd", "America/New_York/", "/UTC", "America//New_York", " UTC",
	}

	var res []map[string]any
	for _, name := range names {
		row := map[string]any{"name": name}
		probe(row, func() {
			loc, err := time.LoadLocation(name)
			if err != nil {
				row["ok"] = false
				row["err"] = err.Error()
				return
			}
			row["ok"] = true
			row["err"] = nil
			row["loc"] = loc.String()
		})
		res = append(res, row)
	}
	return res
}

// --- PreSave / PreUpdate -----------------------------------------------------------------------

type scheduledPostHookCase struct {
	Name string          `json:"name"`
	In   json.RawMessage `json:"in"`

	IdWasMinted   bool   `json:"id_was_minted"`
	IdOut         string `json:"id_out"` // empty when minted
	ProcessedAt   int64  `json:"processed_at_out"`
	ErrorCode     string `json:"error_code_out"`
	DeleteAt      int64  `json:"delete_at_out"`
	CreateAtKept  bool   `json:"create_at_was_kept"`
	CreateAtValue int64  `json:"create_at_value"`
	UpdateAtMoved bool   `json:"update_at_moved"`
	UpdateAtEqCr  bool   `json:"update_at_equals_create_at"`

	PropsNilOut   bool            `json:"props_nil_out"`
	PropsOut      json.RawMessage `json:"props_out"`
	FileIdsNilOut bool            `json:"file_ids_nil_out"`
	FileIdsOut    []string        `json:"file_ids_out"`
}

func scheduledPostHookDocs() []struct{ name, doc string } {
	return []struct{ name, doc string }{
		{"all_zero", `{}`},
		{"id_set", `{"id":"already","create_at":1700000000000}`},
		{"processed_and_error_set", `{"id":"i","create_at":1,"processed_at":99,"error_code":"unknown"}`},
		{"delete_at_set", `{"id":"i","create_at":1,"delete_at":1700000005000}`},
		{"props_null", `{"id":"i","create_at":1,"props":null}`},
		{"props_set", `{"id":"i","create_at":1,"props":{"b":2,"a":1}}`},
		{"file_ids_dupes", `{"id":"i","create_at":1,"file_ids":["b","a","b"]}`},
		{"file_ids_null", `{"id":"i","create_at":1,"file_ids":null}`},
		{"create_at_zero_id_set", `{"id":"i"}`},
	}
}

func scheduledPostPreSaveAll() []scheduledPostHookCase {
	return scheduledPostHooksOver(func(s *model.ScheduledPost) { s.PreSave() })
}

func scheduledPostPreUpdateAll() []scheduledPostHookCase {
	return scheduledPostHooksOver(func(s *model.ScheduledPost) { s.PreUpdate() })
}

func scheduledPostHooksOver(run func(*model.ScheduledPost)) []scheduledPostHookCase {
	docs := scheduledPostHookDocs()
	res := make([]scheduledPostHookCase, 0, len(docs))

	for _, c := range docs {
		var s model.ScheduledPost
		if err := json.Unmarshal([]byte(c.doc), &s); err != nil {
			panic(err)
		}
		inID, inCreate, inUpdate := s.Id, s.CreateAt, s.UpdateAt

		run(&s)

		out := scheduledPostHookCase{
			Name:          c.name,
			In:            json.RawMessage(c.doc),
			IdWasMinted:   s.Id != inID,
			ProcessedAt:   s.ProcessedAt,
			ErrorCode:     s.ErrorCode,
			DeleteAt:      s.DeleteAt,
			CreateAtKept:  s.CreateAt == inCreate,
			CreateAtValue: s.CreateAt,
			UpdateAtMoved: s.UpdateAt != inUpdate,
			UpdateAtEqCr:  s.UpdateAt == s.CreateAt,
			PropsNilOut:   s.GetProps() == nil,
			PropsOut:      json.RawMessage(mustMarshal(s.GetProps())),
			FileIdsNilOut: s.FileIds == nil,
			FileIdsOut:    []string(s.FileIds),
		}
		if !out.IdWasMinted {
			out.IdOut = s.Id
		}
		// A minted id is a CSPRNG value ([D-032]); only its length is recorded, via the flag.
		if out.CreateAtKept {
			out.CreateAtValue = s.CreateAt
		} else {
			out.CreateAtValue = 0
		}
		res = append(res, out)
	}
	return res
}

// --- ToPost ------------------------------------------------------------------------------------

type scheduledPostToPostCase struct {
	Name string          `json:"name"`
	In   json.RawMessage `json:"in"`
	// The marshalled *Post as a STRING, so the Rust side can assert Go's bytes and therefore its
	// field order. Empty when ToPost returned an error.
	Post string `json:"post"`
	Err  string `json:"err"`
}

func scheduledPostToPostAll() []scheduledPostToPostCase {
	docs := []struct{ name, doc string }{
		{"minimal", `{"id":"i","user_id":"u","channel_id":"c","message":"m"}`},
		{"every_carried_field", `{"id":"i","create_at":1,"update_at":2,"delete_at":3,` +
			`"user_id":"u","channel_id":"c","root_id":"r","message":"m","type":"custom_x",` +
			`"file_ids":["f1","f2"],"metadata":{"emojis":[{"name":"smile"}]},` +
			`"scheduled_at":9,"processed_at":8,"error_code":"unknown"}`},
		{"props_are_copied", `{"user_id":"u","props":{"b":2,"a":"x"}}`},
		{"props_null", `{"user_id":"u","props":null}`},
		{"props_empty", `{"user_id":"u","props":{}}`},
		// Priority: all three keys or nothing.
		{"priority_complete", `{"user_id":"u","priority":{"priority":"urgent",` +
			`"requested_ack":true,"persistent_notifications":false}}`},
		{"priority_complete_with_metadata", `{"user_id":"u","metadata":{"emojis":[{"name":"a"}]},` +
			`"priority":{"priority":"important","requested_ack":false,` +
			`"persistent_notifications":true}}`},
		{"priority_empty_map", `{"user_id":"u","priority":{}}`},
		{"priority_null", `{"user_id":"u","priority":null}`},
		{"priority_missing_requested_ack", `{"user_id":"u","priority":{"priority":"urgent"}}`},
		{"priority_missing_persistent", `{"user_id":"u","priority":{"priority":"urgent",` +
			`"requested_ack":true}}`},
		{"priority_wrong_type", `{"user_id":"u","priority":{"priority":1,"requested_ack":true,` +
			`"persistent_notifications":true}}`},
		{"priority_ack_is_a_string", `{"user_id":"u","priority":{"priority":"urgent",` +
			`"requested_ack":"true","persistent_notifications":true}}`},
		{"priority_extra_key", `{"user_id":"u","priority":{"priority":"urgent",` +
			`"requested_ack":true,"persistent_notifications":true,"extra":1}}`},
		{"priority_empty_string", `{"user_id":"u","priority":{"priority":"",` +
			`"requested_ack":false,"persistent_notifications":false}}`},
	}

	res := make([]scheduledPostToPostCase, 0, len(docs))
	for _, c := range docs {
		var s model.ScheduledPost
		if err := json.Unmarshal([]byte(c.doc), &s); err != nil {
			panic(err)
		}

		out := scheduledPostToPostCase{Name: c.name, In: json.RawMessage(c.doc)}
		post, err := s.ToPost()
		if err != nil {
			out.Err = err.Error()
		} else {
			out.Post = mustMarshal(post)
		}
		res = append(res, out)
	}
	return res
}

// --- the three small mutators --------------------------------------------------------------------

func scheduledPostRestoreAll() []map[string]any {
	cases := []struct{ name, target, original string }{
		{"all_differ",
			`{"id":"new","create_at":2,"update_at":22,"user_id":"nu","channel_id":"nc",` +
				`"root_id":"nr","type":"nt","message":"new message","scheduled_at":5}`,
			`{"id":"old","create_at":1,"update_at":11,"user_id":"ou","channel_id":"oc",` +
				`"root_id":"or","type":"ot","message":"old message","scheduled_at":9}`},
		{"original_is_zero",
			`{"id":"new","create_at":2,"user_id":"nu","channel_id":"nc","root_id":"nr","type":"nt"}`,
			`{}`},
		{"identical", `{"id":"x","user_id":"u"}`, `{"id":"x","user_id":"u"}`},
	}

	var res []map[string]any
	for _, c := range cases {
		var target, original model.ScheduledPost
		if err := json.Unmarshal([]byte(c.target), &target); err != nil {
			panic(err)
		}
		if err := json.Unmarshal([]byte(c.original), &original); err != nil {
			panic(err)
		}
		target.RestoreNonUpdatableFields(&original)
		res = append(res, map[string]any{
			"name": c.name, "target": c.target, "original": c.original,
			"out": mustMarshal(&target),
		})
	}
	return res
}

func scheduledPostSanitizeAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"create_at_set", `{"id":"i","create_at":1700000000000,"update_at":5}`},
		{"metadata_nil", `{"id":"i","create_at":1}`},
		{"metadata_with_embeds", `{"id":"i","create_at":1,"metadata":{"embeds":[{"type":"link"}],` +
			`"emojis":[{"name":"smile"}]}}`},
		{"metadata_without_embeds", `{"id":"i","create_at":1,"metadata":{"emojis":[{"name":"a"}]}}`},
	}

	var res []map[string]any
	for _, c := range docs {
		var s model.ScheduledPost
		if err := json.Unmarshal([]byte(c.doc), &s); err != nil {
			panic(err)
		}
		s.SanitizeInput()
		row := map[string]any{"name": c.name, "in": c.doc, "out": mustMarshal(&s)}
		row["metadata_nil"] = s.Metadata == nil
		if s.Metadata != nil {
			row["embeds_nil"] = s.Metadata.Embeds == nil
		}
		res = append(res, row)
	}
	return res
}

func scheduledPostGetPriorityAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"metadata_nil", `{"id":"i"}`},
		{"metadata_without_priority", `{"id":"i","metadata":{"emojis":[{"name":"a"}]}}`},
		{"metadata_with_priority", `{"id":"i","metadata":{"priority":{"priority":"urgent",` +
			`"requested_ack":true}}}`},
		// The `priority` FIELD on the draft is not what GetPriority reads.
		{"draft_priority_only", `{"id":"i","priority":{"priority":"urgent"}}`},
	}

	var res []map[string]any
	for _, c := range docs {
		var s model.ScheduledPost
		if err := json.Unmarshal([]byte(c.doc), &s); err != nil {
			panic(err)
		}
		p := s.GetPriority()
		row := map[string]any{"name": c.name, "in": c.doc, "nil": p == nil}
		if p != nil {
			row["out"] = mustMarshal(p)
		}
		res = append(res, row)
	}
	return res
}
