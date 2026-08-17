package main

// Behavioural oracle for model/audit.go and model/audits.go, written to
// fixtures/behaviour_audit.json.
//
//	type Audit struct { Id, UserId, Action, ExtraInfo, IpAddress, SessionId string; CreateAt int64 }
//	type Audits []Audit
//
//	func (o Audits) Etag() string {
//	    if len(o) > 0 { return Etag(o[0].CreateAt) }   // the first is always the most current
//	    return ""
//	}
//
// `Audit` is a plain tagged struct with nothing to measure beyond its keys. `Audits` is where the
// content is, and it is **unlike every other list Etag in the tree**:
//
//   - **An empty list returns `""`, not a versioned etag.** `ChannelList.Etag` on an empty list
//     returns `<version>.0.0.0.0`; this returns the empty string, which is not a valid etag at
//     all. A caller sending it as an `ETag:` header sends an empty header.
//   - **It reads element [0] rather than scanning.** Every other list etag walks the whole slice
//     for a maximum ([D-010]'s note 2 in MIGRATION.md spells out how `ChannelList` does it). This
//     one trusts the caller's ordering — the comment says "the first in the list is always the
//     most current" — so an unsorted list produces an etag for whichever row happens to be first,
//     and a list sorted the other way is silently wrong.
//   - **One component, not four.** `Etag(o[0].CreateAt)` passes a single value, so the result is
//     `<version>.<create_at>` where the channel lists produce five parts.
//
// The corpus drives an unsorted list explicitly, because "reads [0]" and "returns the newest" are
// the same answer only for sorted input and the Go comment asserts the sort rather than doing it.
//
// **`Audits` is `[]Audit`, not `[]*Audit`** — the first list in the tree whose element is a value.
// So there is no [D-033] here, and a `null` element is a decode error in Go too. Worth recording,
// because every other slice in this crate has needed that entry cited.
//
// Determinism: fixed values only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeAuditBehaviourFixture(outDir string) error {
	out := map[string]any{
		"audit_keys":   expectedKeys(reflect.TypeOf(model.Audit{})),
		"audit_wire":   auditWireAll(),
		"audits_wire":  auditsWireAll(),
		"etag":         auditsEtagAll(),
		"null_element": auditsNullElement(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_audit.json"), append(blob, '\n'), 0o644)
}

// --- Audit ---------------------------------------------------------------------------------------

func auditRow(id string, createAt int64) model.Audit {
	return model.Audit{
		Id:        id,
		CreateAt:  createAt,
		UserId:    "6bdz674pgq767e4jx75w4pf57a",
		Action:    "/api/v4/users/login",
		ExtraInfo: "success",
		IpAddress: "10.0.0.1",
		SessionId: "qr6kf7ztp7yifxt4wm5xn51bke",
	}
}

func auditWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Audit
	}{
		{"zero", model.Audit{}},
		{"typical", auditRow("a1", 1700000000000)},
		{"empty_strings", model.Audit{CreateAt: 1}},
		{"negative_create_at", model.Audit{Id: "a", CreateAt: -1}},
		{"int64_bounds", model.Audit{Id: "a", CreateAt: math.MaxInt64}},
		// ExtraInfo is free text written by the server and can hold anything.
		{"escaped_extra_info", model.Audit{Id: "a", ExtraInfo: "<a>&b c"}},
		// IPv6, since the field is a plain string with no validation.
		{"ipv6", model.Audit{Id: "a", IpAddress: "2001:db8::1"}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

// --- Audits --------------------------------------------------------------------------------------

func auditsWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Audits
	}{
		{"nil", nil},
		{"empty", model.Audits{}},
		{"one", model.Audits{auditRow("a1", 100)}},
		{"several", model.Audits{auditRow("a1", 300), auditRow("a2", 200), auditRow("a3", 100)}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["json"] = mustMarshal(c.in)
			row["nil"] = c.in == nil
			row["len"] = len(c.in)
		})
		res = append(res, row)
	}
	return res
}

// --- Etag ------------------------------------------------------------------------------------------

// auditsEtagAll is the file. Three things are being measured and only the first is obvious:
// the empty-list answer, the fact that element [0] is read rather than the maximum found, and the
// component count.
func auditsEtagAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.Audits
	}{
		// The empty-list answer, which is NOT a versioned etag.
		{"nil", nil},
		{"empty", model.Audits{}},

		{"one", model.Audits{auditRow("a1", 1700000000000)}},

		// Descending by create_at, which is what the comment assumes the caller provides. Here
		// "first" and "newest" coincide.
		{"sorted_descending", model.Audits{
			auditRow("a1", 300), auditRow("a2", 200), auditRow("a3", 100),
		}},
		// Ascending: "first" is now the OLDEST row, and the etag follows the position rather than
		// the timestamp. This is the case that shows the function trusts its input.
		{"sorted_ascending", model.Audits{
			auditRow("a1", 100), auditRow("a2", 200), auditRow("a3", 300),
		}},
		// Unsorted: neither newest nor oldest is first.
		{"unsorted", model.Audits{
			auditRow("a1", 200), auditRow("a2", 300), auditRow("a3", 100),
		}},

		// Edge values in position zero.
		{"zero_create_at", model.Audits{auditRow("a1", 0), auditRow("a2", 999)}},
		{"negative_create_at", model.Audits{auditRow("a1", -1)}},
		{"max_int64", model.Audits{auditRow("a1", math.MaxInt64)}},
		{"min_int64", model.Audits{auditRow("a1", math.MinInt64)}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			etag := c.in.Etag()
			row["etag"] = etag
			row["len"] = len(c.in)
			row["is_empty_string"] = etag == ""
			if len(c.in) > 0 {
				row["first_create_at"] = c.in[0].CreateAt
				// The maximum, so the Rust side can assert that the etag does NOT track it.
				max := c.in[0].CreateAt
				for _, a := range c.in {
					if a.CreateAt > max {
						max = a.CreateAt
					}
				}
				row["max_create_at"] = max
				row["first_is_max"] = c.in[0].CreateAt == max
			}
		})
		res = append(res, row)
	}
	return res
}

// --- the element type ---------------------------------------------------------------------------------

// auditsNullElement records that `Audits` is `[]Audit` and not `[]*Audit`, so Go rejects a null
// element rather than storing one. It is the first slice in the tree where Go and the Rust port
// **agree** on this, and the fixture says so explicitly rather than the absence of a [D-033] row
// being taken as evidence.
func auditsNullElement() map[string]any {
	row := map[string]any{"in": `[null]`}
	probe(row, func() {
		var out model.Audits
		err := json.Unmarshal([]byte(`[null]`), &out)
		row["ok"] = err == nil
		if err != nil {
			row["err"] = err.Error()
		} else {
			row["err"] = nil
			row["json_after"] = mustMarshal(out)
		}
		row["element_is_pointer"] = reflect.TypeOf(model.Audits{}).Elem().Kind() == reflect.Pointer
	})
	return row
}
