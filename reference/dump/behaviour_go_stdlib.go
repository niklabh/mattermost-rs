package main

// Behavioural oracle for the **standard-library and third-party algorithms the 2026-08-24 sweep
// had to reimplement**, written to fixtures/behaviour_go_stdlib.json.
//
// Every entry here exists because a ported model file calls a Go function that has no Rust
// equivalent, so the port contains a hand-written reimplementation. Those are the highest-risk
// lines in the sweep: they compile, they look right, and nothing else in the tree would notice if
// they were wrong. This file replaces "I read the algorithm and wrote it out" with Go's answer.
//
//   - `path.Clean` / `path.Join` — `AutocompleteData.UpdateRelativeURLsForPluginCommands`
//     (command_autocomplete.go:211) joins a base path with a relative FetchURL.
//   - `time.Duration.String` — `ScheduledTask.String` (scheduled_task.go:94).
//   - `time.Time.String` — `UserReport.ToReport` (report.go:95) renders every CSV timestamp with
//     it. **Zone-dependent**, so the corpus is generated under the pinned TZ and the Rust side
//     rebuilds each instant in that same zone.
//   - `encoding/xml` attribute encoding — `StringMap`/`StringInterface`'s `MarshalXML`
//     (xml_helpers.go). Recorded as the **whole element**, not just the escaping, so the corpus
//     tests the port's encoder end to end.
//   - `fmt.Sprintf("%s", []string)` — `OutgoingWebhook.IsValid` (outgoing_webhook.go:127)
//     measures its two 1024-byte caps against this rendering, brackets and separators included.
//   - `golang.org/x/mod/semver.IsValid` — `AccessControlPolicy`'s five version validators.
//   - `github.com/Masterminds/semver/v3.StrictNewVersion` and ordering — `Manifest.IsValid` and
//     `MeetMinServerVersion`. **A different parser from the one above**, and they disagree about
//     the leading `v`; both are recorded so the port cannot conflate them.
//
// Determinism: fixed corpora only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"encoding/xml"
	"fmt"
	"os"
	"path"
	"path/filepath"
	"time"

	masterminds "github.com/Masterminds/semver/v3"
	"github.com/mattermost/mattermost/server/public/model"
	modsemver "golang.org/x/mod/semver"
)

func writeGoStdlibBehaviourFixture(outDir string) error {
	out := map[string]any{
		"path_clean":         pathCleanAll(),
		"path_join":          pathJoinAll(),
		"duration_string":    durationStringAll(),
		"time_string":        timeStringAll(),
		"string_map_xml":     stringMapXMLAll(),
		"string_iface_xml":   stringInterfaceXMLAll(),
		"slice_percent_s":    slicePercentSAll(),
		"mod_semver_isvalid": modSemverIsValidAll(),
		"strict_semver":      strictSemverAll(),
		"strict_semver_cmp":  strictSemverCompareAll(),
		"time_zone":          time.Local.String(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_go_stdlib.json"), append(blob, '\n'), 0o644)
}

// --- path.Clean / path.Join --------------------------------------------------------------------

// pathCorpus covers the shapes `Clean`'s lazybuf actually branches on: rooted and relative, `.`
// and `..` at every position, runs of separators, a trailing separator, and the two cases whose
// answers are counter-intuitive — a relative path that backs out past its own root keeps the
// `..`, while a rooted one silently drops it.
var pathCorpus = []string{
	"", ".", "..", "/", "//", "///",
	"a", "a/", "/a", "/a/", "a//b", "a/./b", "a/../b", "a/b/..",
	"..//a", "../a", "a/..", "/..", "/../a", "/a/../..", "a/../../b",
	"./a", "././a", "a/./././b", "/a//b///c",
	"static/plugin", "/static/plugin/", "/static//plugin/../assets",
	"a/b/c/../../d", "../../a", "/../../a",
}

func pathCleanAll() []map[string]any {
	rows := make([]map[string]any, 0, len(pathCorpus))
	for _, in := range pathCorpus {
		rows = append(rows, map[string]any{"in": in, "out": path.Clean(in)})
	}
	return rows
}

// joinCorpus is the two-element form `UpdateRelativeURLsForPluginCommands` uses: the base URL's
// path and a plugin-supplied FetchURL. The empty-element cases matter — `Join` drops empties
// *before* cleaning, so `Join("", "x")` is not `Clean("/x")`.
var joinCorpus = [][2]string{
	{"", ""}, {"", "a"}, {"a", ""}, {"/", "a"}, {"a", "b"},
	{"/base", "rel"}, {"/base/", "/rel"}, {"/base", "../rel"},
	{"/base/deep", "../../rel"}, {"base", "./rel"}, {"/", "/"},
	{"/plugins/com.example", "autocomplete"},
	{"/plugins/com.example/", "/autocomplete/"},
	{"", "../escape"}, {"/", "../escape"},
}

func pathJoinAll() []map[string]any {
	rows := make([]map[string]any, 0, len(joinCorpus))
	for _, pair := range joinCorpus {
		rows = append(rows, map[string]any{
			"a":   pair[0],
			"b":   pair[1],
			"out": path.Join(pair[0], pair[1]),
		})
	}
	return rows
}

// --- time.Duration.String ----------------------------------------------------------------------

// durationCorpus spans every unit `String` switches on and the fraction-trimming boundaries: zero,
// sub-microsecond, the three sub-second units, exact seconds, minutes and hours, a negative, and
// the two extremes.
var durationCorpus = []int64{
	0, 1, 999, 1000, 1500, 999999, 1000000, 1500000, 999999999,
	1000000000, 1500000000, 1000000001, 59000000000, 60000000000, 61500000000,
	3600000000000, 3661000000000, 86400000000000,
	-1, -1500000000, -3661000000000,
	int64(^uint64(0) >> 1), -int64(^uint64(0)>>1) - 1,
	// The two a scheduled task actually carries.
	int64(5 * time.Minute), int64(24 * time.Hour),
}

func durationStringAll() []map[string]any {
	rows := make([]map[string]any, 0, len(durationCorpus))
	for _, ns := range durationCorpus {
		rows = append(rows, map[string]any{
			"nanos": ns,
			"out":   time.Duration(ns).String(),
		})
	}
	return rows
}

// --- time.Time.String --------------------------------------------------------------------------

// timeStringCorpus is epoch milliseconds, the unit `UserReport.ToReport` starts from. The
// fractional cases are the interesting ones: `String` uses the `.999999999` verb, so trailing
// zeros are trimmed and a whole second renders with no decimal point at all.
var timeStringCorpus = []int64{
	0, 1, 10, 100, 1000, 1001, 1010, 1100, 1500, 1999,
	baseTimeMs, baseTimeMs + 123, baseTimeMs + 120, baseTimeMs + 100,
	-1, -1000, -1500,
	1_000_000_000_000, 253_402_300_799_000,
}

func timeStringAll() []map[string]any {
	rows := make([]map[string]any, 0, len(timeStringCorpus))
	for _, ms := range timeStringCorpus {
		rows = append(rows, map[string]any{
			"millis": ms,
			"out":    time.UnixMilli(ms).String(),
			// RFC3339 with seconds precision is what compliance_post.go's Row uses for the same
			// instant, so both renderings of one input sit side by side.
			"rfc3339": time.Unix(0, ms*int64(1000*1000)).Format(time.RFC3339),
		})
	}
	return rows
}

// --- StringMap / StringInterface XML -----------------------------------------------------------

// xmlProbeMap wraps a StringMap so the recorded output is what a real export writes: a named
// parent element containing the `<Entry>` sequence MarshalXML produces.
type xmlProbeMap struct {
	XMLName xml.Name        `xml:"Probe"`
	Props   model.StringMap `xml:"Props"`
}

type xmlProbeInterface struct {
	XMLName xml.Name              `xml:"Probe"`
	Props   model.StringInterface `xml:"Props"`
}

// stringMapXMLCorpus covers nil, empty, ordering (deliberately inserted out of order, because the
// sort is the only thing making the output deterministic), and every character `encoding/xml`
// escapes in an attribute — including the three whitespace ones that become numeric references.
var stringMapXMLCorpus = []model.StringMap{
	nil,
	{},
	{"a": "1"},
	{"z": "last", "a": "first", "m": "middle"},
	{"amp": "a&b", "lt": "a<b", "gt": "a>b"},
	{"quote": `a"b`, "apos": "a'b"},
	{"tab": "a\tb", "newline": "a\nb", "cr": "a\rb"},
	{"empty": "", "": "empty-key"},
	{"unicode": "héllo    "},
}

func stringMapXMLAll() []map[string]any {
	rows := make([]map[string]any, 0, len(stringMapXMLCorpus))
	for _, m := range stringMapXMLCorpus {
		blob, err := xml.Marshal(xmlProbeMap{Props: m})
		row := map[string]any{"in": m}
		if err != nil {
			row["error"] = err.Error()
		} else {
			row["out"] = string(blob)
		}
		rows = append(rows, row)
	}
	return rows
}

// stringInterfaceXMLCorpus adds the type-switch cases: a string passes through untagged, nil and
// every non-string value are JSON-encoded and tagged `type="json"`.
var stringInterfaceXMLCorpus = []model.StringInterface{
	nil,
	{},
	{"s": "plain"},
	{"n": nil},
	{"b": true, "i": float64(3), "f": 1.5},
	{"arr": []any{"a", float64(1)}, "obj": map[string]any{"k": "v"}},
	{"str_with_quote": `a"b`, "json_with_quote": map[string]any{"k": `a"b`}},
	{"numeric_string": "42"},
}

func stringInterfaceXMLAll() []map[string]any {
	rows := make([]map[string]any, 0, len(stringInterfaceXMLCorpus))
	for _, m := range stringInterfaceXMLCorpus {
		blob, err := xml.Marshal(xmlProbeInterface{Props: m})
		row := map[string]any{"in": m}
		if err != nil {
			row["error"] = err.Error()
		} else {
			row["out"] = string(blob)
		}
		rows = append(rows, row)
	}
	return rows
}

// --- fmt.Sprintf("%s", []string) ---------------------------------------------------------------

// slicePercentSCorpus includes the cases where the rendered length differs most from the sum of
// the elements: nil and empty both render as `[]`, and every extra element adds a separator.
var slicePercentSCorpus = [][]string{
	nil,
	{},
	{""},
	{"a"},
	{"a", "b"},
	{"", ""},
	{"one", "two", "three"},
	{"https://example.com/hook", "https://example.com/other"},
	{"a b", "c\td"},
	{"[bracketed]"},
}

func slicePercentSAll() []map[string]any {
	rows := make([]map[string]any, 0, len(slicePercentSCorpus))
	for _, s := range slicePercentSCorpus {
		rendered := fmt.Sprintf("%s", s)
		rows = append(rows, map[string]any{
			"in":  s,
			"out": rendered,
			// The number `OutgoingWebhook.IsValid` compares against 1024.
			"len": len(rendered),
		})
	}
	return rows
}

// --- golang.org/x/mod/semver -------------------------------------------------------------------

// semverCorpus is shared by both parsers on purpose: the pairs of rows where one accepts and the
// other rejects are the whole point of recording them together.
var semverCorpus = []string{
	"", "v", "0", "1", "0.1", "v0", "v0.1", "v0.1.0", "v1.2.3",
	"0.1.0", "1.2.3", "V1.2.3", "v1.2.3.4", "v01.2.3", "v1.02.3",
	"v1.2.3-alpha", "v1.2.3-alpha.1", "v1.2.3-0", "v1.2.3-01", "v1.2.3-",
	"v1.2.3+build", "v1.2.3+build.1", "v1.2.3-alpha+build", "v1.2.3+",
	"v1.2.3 ", " v1.2.3", "v1.2.-3", "v-1.2.3", "vx.y.z",
	// The five values access_policy.go actually stores.
	"v0.1", "v0.2", "v0.3", "v0.4", "v0.5",
	// What a plugin manifest carries.
	"0.1.0", "5.6.0", "5.6", "10.0.0-rc.1",
	// Un-prefixed variants. Without these almost every strict row is false for the *one* reason
	// that it starts with `v`, and the leading-zero and prerelease rules go untested — the
	// "right and wrong answers coincide" shape CLAUDE.md warns about.
	"1.2.3.4", "01.2.3", "1.02.3", "1.2.03", "1.2.3-alpha", "1.2.3-alpha.1",
	"1.2.3-0", "1.2.3-01", "1.2.3-0a", "1.2.3+build", "1.2.3+01", "1.2.3+build.01",
	"1.2.3-", "1.2.3+", "1.2.3-alpha..1", "1.2.3-AL.pha", "1.2.3-alpha_1",
	" 1.2.3", "1.2.3 ", "1.2", "1", "",
}

func modSemverIsValidAll() []map[string]any {
	rows := make([]map[string]any, 0, len(semverCorpus))
	for _, in := range semverCorpus {
		rows = append(rows, map[string]any{
			"in":  in,
			"out": modsemver.IsValid(in),
		})
	}
	return rows
}

// --- Masterminds/semver ------------------------------------------------------------------------

func strictSemverAll() []map[string]any {
	rows := make([]map[string]any, 0, len(semverCorpus)*2)
	for _, in := range semverCorpus {
		strict, strictErr := masterminds.StrictNewVersion(in)
		lenient, lenientErr := masterminds.NewVersion(in)

		row := map[string]any{"in": in, "strict_ok": strictErr == nil, "lenient_ok": lenientErr == nil}
		// The rejection *reason*, not just the fact: `Manifest.IsValid` wraps these into the
		// message a plugin developer sees, and Masterminds distinguishes six of them.
		if strictErr != nil {
			row["strict_err"] = strictErr.Error()
		}
		if lenientErr != nil {
			row["lenient_err"] = lenientErr.Error()
		}
		if strictErr == nil {
			row["strict_major"] = strict.Major()
			row["strict_minor"] = strict.Minor()
			row["strict_patch"] = strict.Patch()
			row["strict_prerelease"] = strict.Prerelease()
			row["strict_metadata"] = strict.Metadata()
			row["strict_string"] = strict.String()
		}
		if lenientErr == nil {
			row["lenient_major"] = lenient.Major()
			row["lenient_minor"] = lenient.Minor()
			row["lenient_patch"] = lenient.Patch()
			row["lenient_string"] = lenient.String()
		}
		rows = append(rows, row)
	}
	return rows
}

// semverPairs are the orderings `Manifest.MeetMinServerVersion` turns on, including the two SemVer
// §11 rules a naive comparison gets wrong: a prerelease sorts *below* its release, and build
// metadata is ignored entirely.
var semverPairs = [][2]string{
	{"v1.0.0", "v1.0.0"}, {"v1.0.0", "v1.0.1"}, {"v1.0.1", "v1.0.0"},
	{"v1.0.0", "v1.1.0"}, {"v1.0.0", "v2.0.0"}, {"v0.9.9", "v1.0.0"},
	{"v1.0.0-alpha", "v1.0.0"}, {"v1.0.0", "v1.0.0-alpha"},
	{"v1.0.0-alpha", "v1.0.0-beta"}, {"v1.0.0-alpha.1", "v1.0.0-alpha.2"},
	{"v1.0.0-alpha.1", "v1.0.0-alpha.beta"}, {"v1.0.0-1", "v1.0.0-alpha"},
	{"v1.0.0+build1", "v1.0.0+build2"}, {"v1.0.0+build", "v1.0.0"},
	{"v5.6.0", "v5.6.1"}, {"v5.10.0", "v5.9.0"},
}

func strictSemverCompareAll() []map[string]any {
	rows := make([]map[string]any, 0, len(semverPairs))
	for _, pair := range semverPairs {
		a, errA := masterminds.StrictNewVersion(pair[0][1:])
		b, errB := masterminds.StrictNewVersion(pair[1][1:])
		row := map[string]any{"a": pair[0], "b": pair[1]}
		if errA != nil || errB != nil {
			row["error"] = "unparseable"
			rows = append(rows, row)
			continue
		}
		row["compare"] = a.Compare(b)
		row["a_less_than_b"] = a.LessThan(b)
		rows = append(rows, row)
	}
	return rows
}
