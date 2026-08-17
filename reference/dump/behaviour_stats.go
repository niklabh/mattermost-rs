package main

// Behavioural oracle for model/team_stats.go, model/users_stats.go and model/cluster_stats.go,
// written to fixtures/behaviour_stats.json.
//
// Twenty-six lines across three files: eight tagged fields, no methods, no pointers, no
// `omitempty`. One oracle covers all three because there is exactly one question between them and
// it is the same question three times.
//
// # `ClusterStats` uses bare `int`, and the other two use `int64`
//
//	TeamStats.TotalMemberCount        int64
//	UsersStats.TotalUsersCount        int64
//	ClusterStats.TotalWebsocketConnections int   <- not int64
//
// Go's `int` is **platform-width**: 64-bit on every target Mattermost ships and 32-bit on a
// 32-bit build. The crate has mapped every Go integer to `i64` so far, and for `int` that is only
// right if the wire actually accepts the full 64-bit range — which is a property of the machine
// the server runs on, not of the type declaration.
//
// So `int_size` records `strconv.IntSize` and the bounds corpus drives `math.MaxInt64` and
// `math.MinInt64` through both an `int` field and an `int64` field. On the generating host they
// agree; on a 32-bit build they would not, and the recorded `int_size` is what makes that visible
// rather than silent. See [D-074].
//
// Everything else is the ordinary tagged-struct shape: no `omitempty`, so each zero value is a
// full object, and the only expected divergences are the standing [D-057] (`null` into a scalar)
// and [D-040] (case-insensitive keys) instances.
//
// Determinism: fixed values only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"strconv"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeStatsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"int_size":      strconv.IntSize,
		"team_wire":     statsTeamWireAll(),
		"users_wire":    statsUsersWireAll(),
		"cluster_wire":  statsClusterWireAll(),
		"int_bounds":    statsIntBoundsAll(),
		"scalar_decode": statsScalarDecodeAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_stats.json"), append(blob, '\n'), 0o644)
}

// --- the three wire formats ------------------------------------------------------------------------

func statsTeamWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.TeamStats
	}{
		{"zero", model.TeamStats{}},
		{"typical", model.TeamStats{
			TeamId:            "6bdz674pgq767e4jx75w4pf57a",
			TotalMemberCount:  120,
			ActiveMemberCount: 97,
		}},
		{"empty_team_id", model.TeamStats{TotalMemberCount: 1}},
		// Nothing validates these, so an active count above the total is representable.
		{"active_exceeds_total", model.TeamStats{
			TeamId:            "6bdz674pgq767e4jx75w4pf57a",
			TotalMemberCount:  1,
			ActiveMemberCount: 99,
		}},
		{"negative", model.TeamStats{TeamId: "t", TotalMemberCount: -1, ActiveMemberCount: -2}},
		{"escaped_id", model.TeamStats{TeamId: "<a>&b"}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

func statsUsersWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.UsersStats
	}{
		{"zero", model.UsersStats{}},
		{"typical", model.UsersStats{TotalUsersCount: 4321}},
		{"negative", model.UsersStats{TotalUsersCount: -1}},
		{"max_int64", model.UsersStats{TotalUsersCount: math.MaxInt64}},
		{"min_int64", model.UsersStats{TotalUsersCount: math.MinInt64}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

func statsClusterWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.ClusterStats
	}{
		{"zero", model.ClusterStats{}},
		{"typical", model.ClusterStats{
			Id:                        "node-1",
			TotalWebsocketConnections: 512,
			TotalReadDbConnections:    16,
			TotalMasterDbConnections:  8,
		}},
		{"empty_id", model.ClusterStats{TotalWebsocketConnections: 1}},
		{"negative", model.ClusterStats{Id: "n", TotalWebsocketConnections: -1}},
		// The whole point of the file: an `int` field carrying a value only 64 bits can hold.
		{"max_int", model.ClusterStats{Id: "n", TotalWebsocketConnections: math.MaxInt}},
		{"min_int", model.ClusterStats{Id: "n", TotalWebsocketConnections: math.MinInt}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

// --- int versus int64 ------------------------------------------------------------------------------

// statsIntBoundsAll drives the same numeric literals through an `int` field (ClusterStats) and an
// `int64` field (UsersStats) and records whether each accepted it. On a 64-bit host the two
// columns are identical, which is the result that justifies mapping Go's `int` to Rust's `i64`;
// on a 32-bit build they would diverge at 2^31, and `int_size` is what says which host produced
// the fixture.
func statsIntBoundsAll() []map[string]any {
	values := []struct{ name, raw string }{
		{"zero", `0`},
		{"one", `1`},
		{"negative_one", `-1`},
		{"max_int32", `2147483647`},
		{"max_int32_plus_one", `2147483648`},
		{"min_int32", `-2147483648`},
		{"min_int32_minus_one", `-2147483649`},
		{"max_int64", `9223372036854775807`},
		{"min_int64", `-9223372036854775808`},
		{"max_int64_plus_one", `9223372036854775808`},
		{"min_int64_minus_one", `-9223372036854775809`},
	}

	var res []map[string]any
	for _, v := range values {
		row := map[string]any{"name": v.name, "raw": v.raw}
		probe(row, func() {
			// The `int` field.
			var cluster model.ClusterStats
			intErr := json.Unmarshal([]byte(`{"total_websocket_connections":`+v.raw+`}`), &cluster)
			row["int_ok"] = intErr == nil
			row["int_value"] = int64(cluster.TotalWebsocketConnections)
			if intErr != nil {
				row["int_err"] = intErr.Error()
			} else {
				row["int_err"] = nil
			}

			// The `int64` field.
			var users model.UsersStats
			int64Err := json.Unmarshal([]byte(`{"total_users_count":`+v.raw+`}`), &users)
			row["int64_ok"] = int64Err == nil
			row["int64_value"] = users.TotalUsersCount
			if int64Err != nil {
				row["int64_err"] = int64Err.Error()
			} else {
				row["int64_err"] = nil
			}

			// The answer the Rust side actually needs: do the two agree?
			row["agree"] = (intErr == nil) == (int64Err == nil) &&
				int64(cluster.TotalWebsocketConnections) == users.TotalUsersCount
		})
		res = append(res, row)
	}
	return res
}

// --- the scalar decode -----------------------------------------------------------------------------

// statsScalarDecodeAll is the ordinary shape check: which non-integer values Go accepts into these
// fields. Driven through TeamStats because it has both a string and two integers, so a document
// can be malformed in either position.
func statsScalarDecodeAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"full", `{"team_id":"t1","total_member_count":5,"active_member_count":3}`},
		{"partial", `{"team_id":"t1"}`},
		{"empty", `{}`},
		{"unknown_key", `{"nope":1}`},
		// Go matches field names case-insensitively; we do not ([D-040]).
		{"uppercase_key", `{"TEAM_ID":"t1"}`},
		{"mixed_case_key", `{"Total_Member_Count":5}`},
		// Go accepts null into a scalar and leaves the zero value; we reject ([D-057]).
		{"null_string", `{"team_id":null}`},
		{"null_int", `{"total_member_count":null}`},
		// Integer rules, the same ones file.go measured for a struct field.
		{"fractional", `{"total_member_count":1.5}`},
		{"fractional_but_whole", `{"total_member_count":1.0}`},
		{"exponent", `{"total_member_count":1e3}`},
		{"quoted_number", `{"total_member_count":"5"}`},
		{"bool_into_int", `{"total_member_count":true}`},
		{"number_into_string", `{"team_id":5}`},
		{"duplicate_key", `{"team_id":"first","team_id":"second"}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "in": d.doc}
		probe(row, func() {
			var out model.TeamStats
			err := json.Unmarshal([]byte(d.doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["team_id"] = out.TeamId
			row["total_member_count"] = out.TotalMemberCount
			row["active_member_count"] = out.ActiveMemberCount
			row["json_after"] = mustMarshal(out)
		})
		res = append(res, row)
	}
	return res
}
