package main

// Behavioural oracle for model/channel_list.go and model.Etag, written to
// fixtures/behaviour_channel_list.json.
//
// Etag is cache-validation surface: a Rust server that computes a different string than the Go
// server sitting behind the same proxy causes stale or thrashing client caches. It is also the
// one place a *var* from version.go leaks into every model type, so `current_version` is
// recorded here purely as a drift detector — when the pinned SHA moves to a release with a new
// versions[0], the Rust constant stops matching and a test fails instead of the etag silently
// changing.
//
// The list Etags look like they take the newest LastPostAt, but they take the max of LastPostAt
// *and* UpdateAt against a single running `t`, so the two comparisons interfere. The corpus
// below is built to expose that.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

type etagPartsCase struct {
	Name  string `json:"name"`
	Parts []any  `json:"parts"`
	Out   string `json:"out"`
}

type listEtagCase struct {
	Name  string          `json:"name"`
	Items json.RawMessage `json:"items"`
	Out   string          `json:"out"`
}

func writeChannelListBehaviourFixture(outDir string) error {
	out := map[string]any{
		"current_version":                  model.CurrentVersion,
		"etag_parts":                       etagPartsAll(),
		"channel_list_etag":                channelListEtagAll(),
		"channel_list_with_team_data_etag": channelListWithTeamDataEtagAll(),
		"team_etag":                        teamEtagAll(),
		"user_etag":                        userEtagAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_list.json"), append(blob, '\n'), 0o644)
}

// --- Etag(parts ...any) --------------------------------------------------------

// etagPartsAll pins fmt's %v for every argument type a real call site passes. Only strings,
// integers and bools are reachable today; the empty case pins that Etag with no parts is the
// bare version with no separator.
func etagPartsAll() []etagPartsCase {
	cases := []struct {
		name  string
		parts []any
	}{
		{"empty", nil},
		{"one_string", []any{"abc"}},
		{"empty_string", []any{""}},
		{"zero", []any{int64(0)}},
		{"negative", []any{int64(-1)}},
		{"large", []any{int64(1700000000000)}},
		{"bools", []any{true, false}},
		{"team_shape", []any{idA, int64(1705492114000)}},
		{"list_shape", []any{idA, int64(1705492114000), int64(0), 3}},
		{"user_shape", []any{idA, int64(1705492114000), idB, int64(1700000000000), true, false, int64(0)}},
		{"dots_in_a_part", []any{"a.b.c", int64(1)}},
	}

	res := make([]etagPartsCase, 0, len(cases))
	for _, c := range cases {
		parts := c.parts
		if parts == nil {
			parts = []any{}
		}
		res = append(res, etagPartsCase{Name: c.name, Parts: parts, Out: model.Etag(c.parts...)})
	}
	return res
}

// --- ChannelList.Etag ----------------------------------------------------------

func ch(id string, lastPostAt, updateAt int64) *model.Channel {
	return &model.Channel{Id: id, LastPostAt: lastPostAt, UpdateAt: updateAt}
}

func channelListEtagAll() []listEtagCase {
	cases := []struct {
		name string
		list model.ChannelList
	}{
		{"empty", model.ChannelList{}},
		{"nil", nil},
		{"one_zeroed", model.ChannelList{ch(idA, 0, 0)}},
		{"one", model.ChannelList{ch(idA, 100, 50)}},
		// update_at wins over last_post_at on the same channel.
		{"update_at_wins", model.ChannelList{ch(idA, 50, 100)}},
		// Two channels, the newer one second.
		{"newer_second", model.ChannelList{ch(idA, 100, 100), ch(idB, 200, 200)}},
		{"newer_first", model.ChannelList{ch(idB, 200, 200), ch(idA, 100, 100)}},
		// The interference case: B's last_post_at loses to A's t, but B's update_at wins,
		// so the id flips to B while t comes from B's update_at.
		{"split_across_channels", model.ChannelList{ch(idA, 300, 0), ch(idB, 100, 400)}},
		// Equal values never win — the comparison is strictly greater-than.
		{"ties_keep_the_first", model.ChannelList{ch(idA, 100, 100), ch(idB, 100, 100)}},
		// Negative timestamps never beat the initial t of 0, so id stays "0".
		{"all_negative", model.ChannelList{ch(idA, -5, -5)}},
		{"three", model.ChannelList{ch(idA, 10, 20), ch(idB, 30, 5), ch(idC, 7, 8)}},
	}

	res := make([]listEtagCase, 0, len(cases))
	for _, c := range cases {
		list := c.list
		blob, err := json.Marshal(list)
		if err != nil {
			panic(err)
		}
		res = append(res, listEtagCase{Name: c.name, Items: blob, Out: list.Etag()})
	}
	return res
}

func cwtd(id string, lastPostAt, updateAt, teamUpdateAt int64) *model.ChannelWithTeamData {
	return &model.ChannelWithTeamData{
		Channel:      *ch(id, lastPostAt, updateAt),
		TeamUpdateAt: teamUpdateAt,
	}
}

func channelListWithTeamDataEtagAll() []listEtagCase {
	cases := []struct {
		name string
		list model.ChannelListWithTeamData
	}{
		{"empty", model.ChannelListWithTeamData{}},
		{"nil", nil},
		{"one", model.ChannelListWithTeamData{cwtd(idA, 100, 50, 10)}},
		// The third comparison: team_update_at can beat both channel fields.
		{"team_update_at_wins", model.ChannelListWithTeamData{cwtd(idA, 100, 50, 900)}},
		{"team_update_at_loses", model.ChannelListWithTeamData{cwtd(idA, 900, 50, 100)}},
		{"split_across_channels", model.ChannelListWithTeamData{
			cwtd(idA, 300, 0, 0), cwtd(idB, 0, 0, 400),
		}},
		{"ties_keep_the_first", model.ChannelListWithTeamData{
			cwtd(idA, 100, 100, 100), cwtd(idB, 100, 100, 100),
		}},
		{"three", model.ChannelListWithTeamData{
			cwtd(idA, 10, 20, 30), cwtd(idB, 40, 5, 6), cwtd(idC, 7, 8, 9),
		}},
	}

	res := make([]listEtagCase, 0, len(cases))
	for _, c := range cases {
		list := c.list
		blob, err := json.Marshal(list)
		if err != nil {
			panic(err)
		}
		res = append(res, listEtagCase{Name: c.name, Items: blob, Out: list.Etag()})
	}
	return res
}

// --- the two Etags that were blocked on CurrentVersion -------------------------

func teamEtagAll() []listEtagCase {
	cases := []struct {
		name string
		team *model.Team
	}{
		{"zero", &model.Team{}},
		{"typical", &model.Team{Id: idA, UpdateAt: 1705492114000}},
		{"negative_update_at", &model.Team{Id: idA, UpdateAt: -1}},
	}

	res := make([]listEtagCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(c.team)
		if err != nil {
			panic(err)
		}
		res = append(res, listEtagCase{Name: c.name, Items: blob, Out: c.team.Etag()})
	}
	return res
}

type userEtagCase struct {
	Name         string          `json:"name"`
	User         json.RawMessage `json:"user"`
	ShowFullName bool            `json:"show_full_name"`
	ShowEmail    bool            `json:"show_email"`
	Out          string          `json:"out"`
}

func userEtagAll() []userEtagCase {
	users := []struct {
		name string
		user *model.User
	}{
		{"zero", &model.User{}},
		{"typical", &model.User{
			Id:                     idA,
			UpdateAt:               1705492114000,
			TermsOfServiceId:       idB,
			TermsOfServiceCreateAt: 1700000000000,
			BotLastIconUpdate:      1701000000000,
		}},
		{"no_tos", &model.User{Id: idA, UpdateAt: 1705492114000}},
	}

	var res []userEtagCase
	for _, u := range users {
		for _, showFullName := range []bool{true, false} {
			for _, showEmail := range []bool{true, false} {
				blob, err := json.Marshal(u.user)
				if err != nil {
					panic(err)
				}
				res = append(res, userEtagCase{
					Name:         u.name,
					User:         blob,
					ShowFullName: showFullName,
					ShowEmail:    showEmail,
					Out:          u.user.Etag(showFullName, showEmail),
				})
			}
		}
	}
	return res
}
