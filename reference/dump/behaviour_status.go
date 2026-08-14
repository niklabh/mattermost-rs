package main

// Behavioural oracle for model/status.go, written to fixtures/behaviour_status.json.
//
// status.go is small, but three things about it are worth Go's own answer rather than a reading:
//
//  1. **ActiveChannel is stripped by every marshaller in the file.** It carries a json tag *and*
//     `omitempty`, and both ToJSON and StatusListToJSON blank it on a copy before marshalling —
//     so the key is present in the struct, absent from the wire, and the receiver is untouched.
//     A port that simply serialises the struct leaks a field Go never sends.
//
//  2. **StatusListToJSON never emits null.** It allocates `make([]Status, len(u))`, which is an
//     empty non-nil slice for a nil input, so the answer is `[]` where a naive port of a nil Go
//     slice would write `null`.
//
//  3. **StatusMapToInterfaceMap keys the result by s.UserId, not by the map key it read**, and
//     drops offline entries entirely. The two keys are the same at every call site, which is
//     exactly why a port can silently use the wrong one.
//
// The constants are recorded too. StatusCacheSize is `SessionCacheSize` from session.go, so it
// is a cross-file borrow of the kind D-005 tracks; DNDExpiryInterval is a time.Duration, i.e.
// nanoseconds, which is the only nanosecond quantity in the model package.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeStatusBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":                   statusConstants(),
		"to_json":                     statusToJSONAll(),
		"status_list_to_json":         statusListToJSONAll(),
		"status_map_to_interface_map": statusMapToInterfaceMapAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_status.json"), append(blob, '\n'), 0o644)
}

func statusConstants() map[string]any {
	return map[string]any{
		"out_of_office": model.StatusOutOfOffice,
		"offline":       model.StatusOffline,
		"away":          model.StatusAway,
		"dnd":           model.StatusDnd,
		"online":        model.StatusOnline,
		// Borrowed from session.go; recorded so the borrow cannot drift silently.
		"cache_size":      model.StatusCacheSize,
		"channel_timeout": model.StatusChannelTimeout,
		"min_update_time": model.StatusMinUpdateTime,
		// A time.Duration is an int64 count of nanoseconds.
		"dnd_expiry_interval_nanos": int64(model.DNDExpiryInterval),
	}
}

// --- (*Status).ToJSON ------------------------------------------------------------

type statusJSONCase struct {
	Name string `json:"name"`
	// The status as plain `json.Marshal` renders it, so the difference ToJSON makes is visible
	// in the fixture rather than only in the prose above.
	Plain json.RawMessage `json:"plain"`
	Out   json.RawMessage `json:"out"`
	// The same bytes as a string, so the Rust side can assert byte-for-byte rather than
	// comparing parsed Value graphs — key *order* and escaping are part of the contract.
	OutBytes string `json:"out_bytes"`
	// ToJSON copies before blanking; this is the receiver's ActiveChannel afterwards.
	ActiveChannelAfter string `json:"active_channel_after"`
}

func statusCorpus() []struct {
	name string
	s    model.Status
} {
	return []struct {
		name string
		s    model.Status
	}{
		{"zero", model.Status{}},
		{"complete", model.Status{
			UserId:         idA,
			Status:         model.StatusOnline,
			Manual:         true,
			LastActivityAt: 1700000000000,
			ActiveChannel:  idB,
			DNDEndTime:     1700000060,
			PrevStatus:     model.StatusAway,
		}},
		{"no_active_channel", model.Status{
			UserId:         idA,
			Status:         model.StatusDnd,
			LastActivityAt: 1700000000000,
			DNDEndTime:     1700000060,
		}},
		{"offline", model.Status{UserId: idA, Status: model.StatusOffline}},
		{"prev_status_only", model.Status{UserId: idA, PrevStatus: model.StatusOnline}},
		// DNDEndTime is seconds, not milliseconds — the one timestamp in the package that is.
		{"dnd_seconds", model.Status{UserId: idA, Status: model.StatusDnd, DNDEndTime: 1}},
	}
}

func statusToJSONAll() []statusJSONCase {
	var res []statusJSONCase
	for _, c := range statusCorpus() {
		s := c.s
		plain, err := json.Marshal(s)
		if err != nil {
			panic(err)
		}
		out, err := s.ToJSON()
		if err != nil {
			panic(err)
		}
		res = append(res, statusJSONCase{
			Name:               c.name,
			Plain:              plain,
			Out:                out,
			OutBytes:           string(out),
			ActiveChannelAfter: s.ActiveChannel,
		})
	}
	return res
}

// --- StatusListToJSON --------------------------------------------------------------

type statusListCase struct {
	Name string `json:"name"`
	// nil is distinct from empty here and the corpus needs to say which it meant.
	Nil      bool            `json:"nil"`
	In       json.RawMessage `json:"in"`
	Out      json.RawMessage `json:"out"`
	OutBytes string          `json:"out_bytes"`
}

func statusListToJSONAll() []statusListCase {
	all := statusCorpus()
	lists := []struct {
		name  string
		isNil bool
		in    []*model.Status
	}{
		{"nil", true, nil},
		{"empty", false, []*model.Status{}},
		{"single", false, []*model.Status{&all[1].s}},
		{"several", false, []*model.Status{&all[1].s, &all[2].s, &all[3].s}},
	}

	var res []statusListCase
	for _, l := range lists {
		in, err := json.Marshal(l.in)
		if err != nil {
			panic(err)
		}
		out, err := model.StatusListToJSON(l.in)
		if err != nil {
			panic(err)
		}
		res = append(res, statusListCase{Name: l.name, Nil: l.isNil, In: in, Out: out, OutBytes: string(out)})
	}
	return res
}

// --- StatusMapToInterfaceMap ---------------------------------------------------------

type statusMapCase struct {
	Name string `json:"name"`
	// Keyed by the map key, whose relationship to Status.UserId is the point.
	In  map[string]json.RawMessage `json:"in"`
	Out map[string]any             `json:"out"`
}

func statusMapToInterfaceMapAll() []statusMapCase {
	maps := []struct {
		name string
		in   map[string]*model.Status
	}{
		{"empty", map[string]*model.Status{}},
		{"nil", nil},
		{"all_offline", map[string]*model.Status{
			idA: {UserId: idA, Status: model.StatusOffline},
			idB: {UserId: idB, Status: model.StatusOffline},
		}},
		{"mixed", map[string]*model.Status{
			idA: {UserId: idA, Status: model.StatusOnline},
			idB: {UserId: idB, Status: model.StatusOffline},
			idC: {UserId: idC, Status: model.StatusDnd},
		}},
		// The map key and the UserId disagree: the output must use the UserId.
		{"key_differs_from_user_id", map[string]*model.Status{
			"not-a-user-id": {UserId: idA, Status: model.StatusAway},
		}},
		// An empty status string is not StatusOffline, so it survives the filter.
		{"empty_status_string", map[string]*model.Status{
			idA: {UserId: idA, Status: ""},
		}},
	}

	var res []statusMapCase
	for _, m := range maps {
		in := map[string]json.RawMessage{}
		for k, v := range m.in {
			blob, err := json.Marshal(v)
			if err != nil {
				panic(err)
			}
			in[k] = blob
		}
		res = append(res, statusMapCase{
			Name: m.name,
			In:   in,
			Out:  model.StatusMapToInterfaceMap(m.in),
		})
	}
	return res
}
