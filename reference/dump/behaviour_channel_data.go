package main

// Behavioural oracle for model/channel_data.go, written to fixtures/behaviour_channel_data.json.
//
// Eighteen lines: two nillable pointer fields and one `Etag`.
//
//	type ChannelData struct {
//	    Channel *Channel       `json:"channel"`
//	    Member  *ChannelMember `json:"member"`
//	}
//
//	func (o *ChannelData) Etag() string {
//	    var mt int64
//	    if o.Member != nil { mt = o.Member.LastUpdateAt }
//	    return Etag(o.Channel.Id, o.Channel.UpdateAt, o.Channel.LastPostAt, mt)
//	}
//
// **The method guards one pointer and dereferences the other, three lines apart.** `Member` gets
// a nil check; `Channel` is read unguarded on the very next line. So a nil member yields an etag
// whose fourth component is `0`, and a nil channel **crashes** — and a nil channel is not exotic,
// because neither field has `omitempty`, both are `null` on the wire for a zero value, and
// `ChannelData{}` from any code path has both nil.
//
// That asymmetry is the file, and it is recorded under `recover` rather than reasoned about: the
// last three sessions each found a guarded/unguarded pair that was not the one the source
// suggested at a glance, so the crash is measured, not inferred from reading.
//
// The other thing worth pinning is which *fields* reach the etag. `Etag` takes four values and
// three of them come from the channel — `Id`, `UpdateAt`, `LastPostAt` — while the member
// contributes only `LastUpdateAt`. So two `ChannelData` values differing in the member's roles,
// its `LastViewedAt` or its `MsgCount` share an etag, and a client will not refetch. The corpus
// varies each of those independently to make that a measurement.
//
// **Every case is built as a Go value and marshalled, rather than written as a JSON literal.**
// That is deliberate: a hand-written partial document like `{"channel":{}}` is a fine probe of Go
// and cannot be decoded by the Rust port at all, because `Channel` and `ChannelMember` are two of
// the 61 containers still owed a `#[serde(default)]` under [D-043]. Marshalling from a struct
// yields the document the Go *server* emits — complete, every key present — which is the one the
// wire format actually has to agree on, and it keeps this file from silently becoming a D-043
// test.
//
// Determinism: fixed values only. No NewId, no time.Now — see [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeChannelDataBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":       channelDataWireAll(),
		"etag":       channelDataEtagAll(),
		"etag_parts": channelDataEtagPartsAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_data.json"), append(blob, '\n'), 0o644)
}

// --- the corpus ---------------------------------------------------------------------------------

func cdChannel() *model.Channel {
	return &model.Channel{
		Id:            "qr6kf7ztp7yifxt4wm5xn51bke",
		CreateAt:      100,
		UpdateAt:      200,
		TeamId:        "6bdz674pgq767e4jx75w4pf57a",
		Type:          model.ChannelTypeOpen,
		DisplayName:   "Town Square",
		Name:          "town-square",
		Header:        "h",
		Purpose:       "p",
		LastPostAt:    300,
		TotalMsgCount: 7,
	}
}

func cdMember() *model.ChannelMember {
	return &model.ChannelMember{
		ChannelId:    "qr6kf7ztp7yifxt4wm5xn51bke",
		UserId:       "6bdz674pgq767e4jx75w4pf57a",
		Roles:        "channel_user",
		LastViewedAt: 400,
		MsgCount:     5,
		MentionCount: 1,
		NotifyProps:  model.StringMap{"desktop": "default", "mark_unread": "all"},
		LastUpdateAt: 500,
		SchemeUser:   true,
	}
}

// withChannel and withMember apply a mutation to a fresh baseline, so no case can leak state into
// the next one.
func withChannel(f func(*model.Channel)) *model.Channel {
	c := cdChannel()
	f(c)
	return c
}

func withMember(f func(*model.ChannelMember)) *model.ChannelMember {
	m := cdMember()
	f(m)
	return m
}

// dumpChannelData records the value plus the two facts worth reading out explicitly: which of the
// two pointers is nil. Both marshal to `null`, so the JSON shows it too — but naming it lets the
// Rust side assert the Option state rather than the rendering.
func dumpChannelData(cd *model.ChannelData) map[string]any {
	if cd == nil {
		return map[string]any{"nil": true}
	}
	return map[string]any{
		"json":        mustMarshal(cd),
		"channel_nil": cd.Channel == nil,
		"member_nil":  cd.Member == nil,
	}
}

// --- the wire format ------------------------------------------------------------------------------

// channelDataWireAll drives the four combinations of the two nillable pointers. Neither field has
// omitempty, so all four are on the wire and `{}` is not one of the outputs.
func channelDataWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   *model.ChannelData
	}{
		{"zero", &model.ChannelData{}},
		{"channel_only", &model.ChannelData{Channel: cdChannel()}},
		{"member_only", &model.ChannelData{Member: cdMember()}},
		{"both", &model.ChannelData{Channel: cdChannel(), Member: cdMember()}},
		// Allocated but zero-valued: the pointers are non-nil and every field is its zero value,
		// which is a different document from `null` and a different one again from a real
		// channel.
		{"zero_objects", &model.ChannelData{Channel: &model.Channel{}, Member: &model.ChannelMember{}}},
		{"zero_channel_real_member", &model.ChannelData{Channel: &model.Channel{}, Member: cdMember()}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			// `in` is Go's own encoding of the value, so the Rust side decodes exactly what a Go
			// server would have sent rather than a hand-written approximation of it.
			row["in"] = mustMarshal(c.in)
			row["out"] = dumpChannelData(c.in)
		})
		res = append(res, row)
	}
	return res
}

// --- Etag -------------------------------------------------------------------------------------

// channelDataEtagAll is the asymmetry. Every row is probed under recover, so a nil channel is
// recorded as `panicked: true` rather than as an absent answer.
func channelDataEtagAll() []map[string]any {
	corpus := []struct {
		name string
		in   *model.ChannelData
	}{
		// The crash: Channel is dereferenced with no guard.
		{"both_nil", &model.ChannelData{}},
		{"member_only", &model.ChannelData{Member: cdMember()}},
		{"nil_channel_zero_member", &model.ChannelData{Member: &model.ChannelMember{}}},
		// The guard: a nil member contributes a literal 0.
		{"channel_only", &model.ChannelData{Channel: cdChannel()}},
		// Both present.
		{"both", &model.ChannelData{Channel: cdChannel(), Member: cdMember()}},
		// A zero-valued member is not the same as a nil one on the way in, and is the same on the
		// way out — both contribute 0. The one case where the guard makes no difference.
		{"zero_member", &model.ChannelData{Channel: cdChannel(), Member: &model.ChannelMember{}}},
		{"zero_channel", &model.ChannelData{Channel: &model.Channel{}, Member: cdMember()}},
		{"both_zero", &model.ChannelData{Channel: &model.Channel{}, Member: &model.ChannelMember{}}},
		// Negative timestamps: Etag interpolates with %v and escapes nothing, so the sign simply
		// appears.
		{"negative_timestamps", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.UpdateAt, c.LastPostAt = -1, -2 }),
			Member:  withMember(func(m *model.ChannelMember) { m.LastUpdateAt = -3 }),
		}},
		// An id containing a dot silently changes the component count — Etag joins with "." and
		// escapes nothing.
		{"id_with_a_dot", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.Id = "a.b" }),
		}},
		{"empty_id", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.Id = "" }),
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["in"] = mustMarshal(c.in)
			row["channel_nil"] = c.in.Channel == nil
			row["member_nil"] = c.in.Member == nil
			// If this panics, `probe` records it and `etag` stays absent.
			row["etag"] = c.in.Etag()
		})
		res = append(res, row)
	}
	return res
}

// --- which fields reach the etag ------------------------------------------------------------------

// channelDataEtagPartsAll varies one field at a time against a fixed baseline and records whether
// the etag moved. Three channel fields feed it and exactly one member field does, so every other
// member field is invisible to cache invalidation — two members differing only in their roles or
// their unread counts share an etag and a client will not refetch.
func channelDataEtagPartsAll() []map[string]any {
	baseline := &model.ChannelData{Channel: cdChannel(), Member: cdMember()}
	base := baseline.Etag()

	mutations := []struct {
		name string
		in   *model.ChannelData
	}{
		{"baseline", baseline},

		// The three that feed it, from the channel.
		{"channel_id", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.Id = "CHANGED" }), Member: cdMember()}},
		{"channel_update_at", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.UpdateAt = 999 }), Member: cdMember()}},
		{"channel_last_post_at", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.LastPostAt = 999 }), Member: cdMember()}},

		// The one that feeds it, from the member.
		{"member_last_update_at", &model.ChannelData{
			Channel: cdChannel(), Member: withMember(func(m *model.ChannelMember) { m.LastUpdateAt = 999 })}},

		// ...and the member fields that do not. Each of these is a real change a client would
		// want to see and will not be told about.
		{"member_roles", &model.ChannelData{
			Channel: cdChannel(), Member: withMember(func(m *model.ChannelMember) { m.Roles = "channel_admin" })}},
		{"member_last_viewed_at", &model.ChannelData{
			Channel: cdChannel(), Member: withMember(func(m *model.ChannelMember) { m.LastViewedAt = 9999 })}},
		{"member_msg_count", &model.ChannelData{
			Channel: cdChannel(), Member: withMember(func(m *model.ChannelMember) { m.MsgCount = 9999 })}},
		{"member_mention_count", &model.ChannelData{
			Channel: cdChannel(), Member: withMember(func(m *model.ChannelMember) { m.MentionCount = 9999 })}},
		{"member_notify_props", &model.ChannelData{
			Channel: cdChannel(), Member: withMember(func(m *model.ChannelMember) {
				m.NotifyProps = model.StringMap{"desktop": "none"}
			})}},

		// And the channel fields that do not.
		{"channel_display_name", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.DisplayName = "CHANGED" }), Member: cdMember()}},
		{"channel_total_msg_count", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.TotalMsgCount = 9999 }), Member: cdMember()}},
		{"channel_delete_at", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.DeleteAt = 9999 }), Member: cdMember()}},
		{"channel_create_at", &model.ChannelData{
			Channel: withChannel(func(c *model.Channel) { c.CreateAt = 9999 }), Member: cdMember()}},
	}

	var res []map[string]any
	for _, m := range mutations {
		row := map[string]any{"name": m.name}
		probe(row, func() {
			row["in"] = mustMarshal(m.in)
			etag := m.in.Etag()
			row["etag"] = etag
			row["differs_from_baseline"] = etag != base
		})
		res = append(res, row)
	}
	return res
}
