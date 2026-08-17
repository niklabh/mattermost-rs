package main

// Behavioural oracle for model/audit_record.go, written to fixtures/behaviour_audit_record.json.
//
// Three things here are not what reading the file suggests.
//
// # The constant and the tag disagree
//
//	AuditKeyEventData = "event_data"
//	...
//	EventData AuditEventData `json:"event"`
//
// The field carrying the event data is on the wire as **"event"**, while the constant that names
// it says "event_data". Both are exported and both are used elsewhere in the tree, so a port that
// trusts the constant emits a key nothing reads. The key list is read off the struct tags by
// reflection rather than transcribed, for exactly this reason.
//
// # AddMeta panics where the parameter adders do not
//
// Every `AddEventParameter*` function lazily creates `rec.EventData.Parameters` if it is nil.
// `AddMeta` does not:
//
//	func (rec *AuditRecord) AddMeta(name string, val any) {
//	    rec.Meta[name] = val
//	}
//
// `Meta` is a `map[string]any` with no constructor anywhere in this file, so `AddMeta` on a
// zero-valued AuditRecord assigns to a nil map and panics. The asymmetry is invisible unless the
// two are probed side by side, which is what the "nil_maps" section does.
//
// # EventMeta is declared and never used
//
// `EventMeta` has json tags and looks like the type of `AuditRecord.Meta`, but that field is a
// bare `map[string]any`. Nothing in this file constructs an `EventMeta`. Recorded because a
// translator will reasonably assume it is the meta type and produce a typed field where Go has an
// open map — which changes what an arbitrary `AddMeta` call can store.
//
// Determinism: fixed values only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeAuditRecordBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":      auditRecordConstants(),
		"keys":           auditRecordKeys(),
		"wire":           auditRecordWireAll(),
		"nil_maps":       auditRecordNilMapAll(),
		"status_setters": auditRecordStatusAll(),
		"parameters":     auditRecordParametersAll(),
		"states":         auditRecordStateAll(),
		"errors":         auditRecordErrorAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_audit_record.json"), append(blob, '\n'), 0o644)
}

func auditRecordConstants() map[string]any {
	return map[string]any{
		"AuditKeyActor":     model.AuditKeyActor,
		"AuditKeyAPIPath":   model.AuditKeyAPIPath,
		"AuditKeyEvent":     model.AuditKeyEvent,
		"AuditKeyEventData": model.AuditKeyEventData,
		"AuditKeyEventName": model.AuditKeyEventName,
		"AuditKeyMeta":      model.AuditKeyMeta,
		"AuditKeyError":     model.AuditKeyError,
		"AuditKeyStatus":    model.AuditKeyStatus,
		"AuditKeyUserID":    model.AuditKeyUserID,
		"AuditKeySessionID": model.AuditKeySessionID,
		"AuditKeyClient":    model.AuditKeyClient,
		"AuditKeyIPAddress": model.AuditKeyIPAddress,
		"AuditKeyClusterID": model.AuditKeyClusterID,

		"AuditStatusSuccess": model.AuditStatusSuccess,
		"AuditStatusAttempt": model.AuditStatusAttempt,
		"AuditStatusFail":    model.AuditStatusFail,
	}
}

// The tag lists, read off the types. `record` is the one that matters: its third entry is
// "event", not the "event_data" the constant would suggest.
func auditRecordKeys() map[string]any {
	return map[string]any{
		"record":      expectedKeys(reflect.TypeOf(model.AuditRecord{})),
		"event_data":  expectedKeys(reflect.TypeOf(model.AuditEventData{})),
		"actor":       expectedKeys(reflect.TypeOf(model.AuditEventActor{})),
		"event_meta":  expectedKeys(reflect.TypeOf(model.EventMeta{})),
		"event_error": expectedKeys(reflect.TypeOf(model.AuditEventError{})),
	}
}

// --- the wire format -------------------------------------------------------------------------

func auditRecordWireAll() []map[string]any {
	full := model.AuditRecord{
		EventName: "some.event",
		Status:    model.AuditStatusSuccess,
		EventData: model.AuditEventData{
			Parameters:  map[string]any{"key": "value"},
			PriorState:  map[string]any{"before": float64(1)},
			ResultState: map[string]any{"after": float64(2)},
			ObjectType:  "bot",
		},
		Actor: model.AuditEventActor{
			UserId:        "y9i4er48tt8bukijy7i3u5y9ar",
			SessionId:     "6hoahfi4zircinud9y93beegwc",
			Client:        "mattermost-webapp",
			IpAddress:     "192.0.2.1",
			XForwardedFor: "198.51.100.1",
		},
		Meta:  map[string]any{"api_path": "/api/v4/bots"},
		Error: model.AuditEventError{Description: "boom", Code: 500},
	}

	corpus := []struct {
		name string
		in   model.AuditRecord
	}{
		// Nothing on AuditRecord carries omitempty, so every key is present even when the maps
		// are nil — and a nil map is `null`, not `{}`.
		{"zero", model.AuditRecord{}},
		{"full", full},
		// AuditEventError is the only nested type with omitempty, on BOTH fields, so a
		// zero-valued error object is `{}` rather than two keys.
		{"no_error", func() model.AuditRecord { r := full; r.Error = model.AuditEventError{}; return r }()},
		// Code omitempty means a zero status code vanishes while a description survives.
		{"error_desc_only", func() model.AuditRecord {
			r := full
			r.Error = model.AuditEventError{Description: "only a description"}
			return r
		}()},
		{"error_code_only", func() model.AuditRecord {
			r := full
			r.Error = model.AuditEventError{Code: 404}
			return r
		}()},
		// Empty (non-nil) maps serialise as `{}`, which is a different document from `null`.
		{"empty_maps", func() model.AuditRecord {
			r := model.AuditRecord{}
			r.EventData.Parameters = map[string]any{}
			r.EventData.PriorState = map[string]any{}
			r.EventData.ResultState = map[string]any{}
			r.Meta = map[string]any{}
			return r
		}()},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		blob, err := json.Marshal(&c.in)
		if err != nil {
			panic(err)
		}
		out = append(out, map[string]any{"name": c.name, "json": string(blob)})
	}

	// EventMeta is unused by AuditRecord but is an exported type with tags, so its wire form is
	// recorded too.
	metaBlob, err := json.Marshal(&model.EventMeta{ApiPath: "/api/v4/bots", ClusterId: "cluster-1"})
	if err != nil {
		panic(err)
	}
	zeroMetaBlob, err := json.Marshal(&model.EventMeta{})
	if err != nil {
		panic(err)
	}
	out = append(out,
		map[string]any{"name": "event_meta_full", "json": string(metaBlob)},
		map[string]any{"name": "event_meta_zero", "json": string(zeroMetaBlob)},
	)
	return out
}

// --- the nil-map asymmetry -------------------------------------------------------------------

// probe runs f and reports whether it panicked.
func auditProbe(f func()) bool {
	panicked := false
	func() {
		defer func() {
			if r := recover(); r != nil {
				panicked = true
			}
		}()
		f()
	}()
	return panicked
}

func auditRecordNilMapAll() []map[string]any {
	out := make([]map[string]any, 0)

	// The parameter adders create the map when it is nil.
	rec1 := model.AuditRecord{}
	p1 := auditProbe(func() { model.AddEventParameterToAuditRec(&rec1, "k", "v") })
	out = append(out, map[string]any{
		"name":             "AddEventParameterToAuditRec_on_nil_map",
		"panics":           p1,
		"parameters_after": mustJSON(rec1.EventData.Parameters),
	})

	rec2 := model.AuditRecord{}
	bot := model.Bot{UserId: "y9i4er48tt8bukijy7i3u5y9ar", Username: "b"}
	p2 := auditProbe(func() { model.AddEventParameterAuditableToAuditRec(&rec2, "bot", &bot) })
	out = append(out, map[string]any{
		"name":             "AddEventParameterAuditableToAuditRec_on_nil_map",
		"panics":           p2,
		"parameters_after": mustJSON(rec2.EventData.Parameters),
	})

	rec3 := model.AuditRecord{}
	p3 := auditProbe(func() {
		model.AddEventParameterAuditableArrayToAuditRec(&rec3, "bots", []*model.Bot{&bot, &bot})
	})
	out = append(out, map[string]any{
		"name":             "AddEventParameterAuditableArrayToAuditRec_on_nil_map",
		"panics":           p3,
		"parameters_after": mustJSON(rec3.EventData.Parameters),
	})

	// AddMeta does NOT. This is the asymmetry.
	rec4 := model.AuditRecord{}
	p4 := auditProbe(func() { rec4.AddMeta("k", "v") })
	out = append(out, map[string]any{
		"name":   "AddMeta_on_nil_map",
		"panics": p4,
	})

	// With a map already present it works.
	rec5 := model.AuditRecord{Meta: map[string]any{}}
	p5 := auditProbe(func() { rec5.AddMeta("k", "v") })
	out = append(out, map[string]any{
		"name":       "AddMeta_on_existing_map",
		"panics":     p5,
		"meta_after": mustJSON(rec5.Meta),
	})

	// The state setters assign rather than index, so they are safe on a zero record.
	rec6 := model.AuditRecord{}
	p6 := auditProbe(func() { rec6.AddEventPriorState(&bot) })
	out = append(out, map[string]any{
		"name":              "AddEventPriorState_on_zero_record",
		"panics":            p6,
		"prior_state_after": mustJSON(rec6.EventData.PriorState),
	})

	return out
}

// --- status / parameters / states / errors ---------------------------------------------------

func auditRecordStatusAll() []map[string]any {
	success := model.AuditRecord{Status: "whatever"}
	success.Success()
	fail := model.AuditRecord{Status: "whatever"}
	fail.Fail()
	return []map[string]any{
		{"name": "Success", "status": success.Status},
		{"name": "Fail", "status": fail.Status},
	}
}

func auditRecordParametersAll() []map[string]any {
	// The generic constraint is string | bool | int | int64 | []string | map[string]string. Each
	// lands in a map[string]any and marshals as its own JSON type, so the parameter map is
	// heterogeneous by design.
	rec := model.AuditRecord{}
	model.AddEventParameterToAuditRec(&rec, "a_string", "text")
	model.AddEventParameterToAuditRec(&rec, "a_bool", true)
	model.AddEventParameterToAuditRec(&rec, "an_int", 42)
	model.AddEventParameterToAuditRec(&rec, "an_int64", int64(9007199254740993))
	model.AddEventParameterToAuditRec(&rec, "a_string_slice", []string{"x", "y"})
	model.AddEventParameterToAuditRec(&rec, "a_string_map", map[string]string{"k": "v"})

	// Overwriting an existing key replaces it.
	model.AddEventParameterToAuditRec(&rec, "a_string", "replaced")

	arrayRec := model.AuditRecord{}
	b1 := model.Bot{UserId: "aaaaaaaaaaaaaaaaaaaaaaaaaa", Username: "one"}
	b2 := model.Bot{UserId: "bbbbbbbbbbbbbbbbbbbbbbbbbb", Username: "two"}
	model.AddEventParameterAuditableArrayToAuditRec(&arrayRec, "bots", []*model.Bot{&b1, &b2})

	emptyArrayRec := model.AuditRecord{}
	model.AddEventParameterAuditableArrayToAuditRec(&emptyArrayRec, "bots", []*model.Bot{})

	return []map[string]any{
		{"name": "mixed_types", "parameters": mustJSON(rec.EventData.Parameters)},
		{"name": "auditable_array", "parameters": mustJSON(arrayRec.EventData.Parameters)},
		// make(..., 0, 0) then append nothing: an empty slice, so `[]` rather than `null`.
		{"name": "auditable_array_empty", "parameters": mustJSON(emptyArrayRec.EventData.Parameters)},
	}
}

func auditRecordStateAll() []map[string]any {
	bot := model.Bot{
		UserId:      "y9i4er48tt8bukijy7i3u5y9ar",
		Username:    "botusername",
		DisplayName: "Bot",
		OwnerId:     "aaaaaaaaaaaaaaaaaaaaaaaaaa",
		CreateAt:    100,
		UpdateAt:    200,
	}

	rec := model.AuditRecord{}
	rec.AddEventPriorState(&bot)
	rec.AddEventResultState(&bot)
	rec.AddEventObjectType("bot")

	return []map[string]any{{
		"name":            "bot_states",
		"prior_state":     mustJSON(rec.EventData.PriorState),
		"resulting_state": mustJSON(rec.EventData.ResultState),
		"object_type":     rec.EventData.ObjectType,
	}}
}

func auditRecordErrorAll() []map[string]any {
	out := make([]map[string]any, 0)

	rec := model.AuditRecord{}
	rec.AddErrorCode(418)
	rec.AddErrorDesc("teapot")
	out = append(out, map[string]any{
		"name":        "explicit",
		"code":        rec.Error.Code,
		"description": rec.Error.Description,
	})

	// AddAppError uses err.Error(), not err.Message — so the description is the full formatted
	// string including the Where and the DetailedError, not the human message alone.
	appErr := model.NewAppError("SomeWhere", "some.error.id", nil, "detailed bit", 409)
	rec2 := model.AuditRecord{}
	rec2.AddAppError(appErr)
	out = append(out, map[string]any{
		"name":            "from_app_error",
		"code":            rec2.Error.Code,
		"description":     rec2.Error.Description,
		"app_error_error": appErr.Error(),
		"app_error_msg":   appErr.Message,
	})

	return out
}
