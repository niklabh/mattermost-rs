package main

// Behavioural oracle for the two constraint grammars and the config-path lookup that
// `noticeMatchesConditions` (channels/app/product_notices.go) evaluates a product notice with,
// written to fixtures/behaviour_notice_conditions.json.
//
//   - `github.com/Masterminds/semver/v3`: `NewVersion` (the *coercing* parser — `CoerceNewVersion`
//     is true by default, so "7.9" and "v11" parse where the strict parser refuses them) and
//     `NewConstraint(...).Check(...)` over every operator, the `x`/`*` wildcards, hyphen ranges,
//     comma and space conjunction, `||` disjunction and the prerelease rule.
//   - `github.com/reflog/dateconstraints`: `NewConstraint(...).Check(&t)` over the RFC 3339 forms
//     its regex admits — a bare year, a date, a date-time with and without seconds, fraction and
//     zone — and the `Check` against a midnight-truncated UTC instant, which is what the app
//     passes.
//   - `config.GetValueByPath` and the typed equality `validateConfigEntry` applies to what it
//     finds: a JSON-decoded expectation (`float64`, `string`, `bool`, `nil`) against a `*string`,
//     `*bool`, `*int` or `*int64` setting. `1 == 1.0` is **false** there, because the setting is
//     an `int` and the expectation a `float64`.
//
// Determinism: fixed values only. See [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"time"

	masterminds "github.com/Masterminds/semver/v3"
	date_constraints "github.com/reflog/dateconstraints"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/config"
)

func writeNoticeConditionsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"semver_parse": ncSemverParseAll(),
		"semver_check": ncSemverCheckAll(),
		"date_check":   ncDateCheckAll(),
		"config_entry": ncConfigEntryAll(),
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_notice_conditions.json"), append(blob, '\n'), 0o644)
}

// The versions the parser is driven through: the feed's real spellings, the coercions the
// loose regex admits, and the refusals.
var ncVersions = []string{
	"11.11.0", "11.11", "11", "v11.11.0", "7.9", "7.10", "8.0.1", "11.7.9", "10.11.22", "7.0",
	"0.0.0", "1.2.3-rc1", "1.2.3-rc.1", "1.2.3-0", "1.2.3+build.5", "1.2.3-beta+exp.sha.5114f85",
	"01.2.3", "1.02.3", "1.2.3-01", "1.2.3-", "1.2.3-a..b", "1.2.3+", "1.2.3.4", "1.2.3 ",
	" 1.2.3", "a.b.c", "", "notsemver", "5.0.0", "5.20", "1.0.0-alpha", "1.0.0-alpha.1",
	"1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0",
	"18446744073709551615", "18446744073709551616", "2.0.0-x", "2.0.0", "1.9.9",
}

func ncSemverParseAll() []map[string]any {
	rows := make([]map[string]any, 0, len(ncVersions))
	for _, in := range ncVersions {
		row := map[string]any{"in": in}
		v, err := masterminds.NewVersion(in)
		if err != nil {
			row["ok"] = false
		} else {
			row["ok"] = true
			row["major"] = v.Major()
			row["minor"] = v.Minor()
			row["patch"] = v.Patch()
			row["pre"] = v.Prerelease()
			row["metadata"] = v.Metadata()
		}
		rows = append(rows, row)
	}
	return rows
}

// The constraints: the feed's, then each operator with and without wildcards, ranges,
// conjunction, disjunction, prereleases, and the refusals.
var ncConstraints = []string{
	"<=7.8.9", ">= 7.9 <= 7.10", ">= 8.0.0 <=8.0.1", "8.1.0", ">= 11.7.0 <=11.7.9", "<11.10",
	">= 10.11.0 <=10.11.22", ">=7.0", "<11.0", "<7.1", "<12.0",
	"", "*", "x", "1.x", "1.2.x", "1.*", "1", "1.2", "=1.2.3", "!=1.2.3", "!=1.2", "!=1.x",
	">1.2.3", ">1.2", ">1", "<1.2.3", "<1.2", ">=1.2.3", "=>1.2.3", "<=1.2.3", "=<1.2.3",
	"<=1.2", "<=1", "~1.2.3", "~1.2", "~1", "~>1.2.3", "^1.2.3", "^1.2", "^1", "^0.2.3", "^0.0.3",
	"^0.0", "^0", "1.2.3 - 2.3.4", "1.2 - 2.3", ">=1.2.3, <2.0.0", ">=1.2.3 <2.0.0",
	"<1.0.0 || >=2.0.0", ">=1.2.3-rc1", ">1.0.0-alpha", "~1.0.0-beta", "1.2.3-rc.1",
	"< 1.2.3", ">=  1.2.3", "v1.2.3", ">=v1.2", "1.2.3.4", ">>1.2.3", "><1.2.3", "1.2.3 1.2.4",
	"a.b.c", ">=1.2.3,", ",>=1.2.3", "||", ">= 7.9 - 7.10",
}

// The versions each constraint is checked against.
var ncCheckVersions = []string{
	"11.11.0", "7.8.9", "7.9", "7.9.0", "7.10", "7.10.1", "8.0.0", "8.0.1", "8.0.2", "8.1.0",
	"11.7.0", "11.7.9", "11.7.10", "11.10", "11.9.9", "10.11.0", "10.11.22", "10.11.23", "7.0",
	"6.9.9", "11.0", "10.9", "7.1", "12.0", "1.2.3", "1.2.4", "1.3.0", "2.0.0", "1.9.9", "0.9.9",
	"1.2.2", "1.2.3-rc1", "1.2.3-rc2", "1.0.0-alpha", "1.0.0-beta", "2.3.4", "2.3.5", "1.2.3+b",
	"0.2.3", "0.2.4", "0.3.0", "0.0.3", "0.0.4", "0.1.0", "1.0.0", "3.0.0",
}

func ncSemverCheckAll() []map[string]any {
	rows := make([]map[string]any, 0, len(ncConstraints)*(1+len(ncCheckVersions)))
	for _, c := range ncConstraints {
		cs, err := masterminds.NewConstraint(c)
		if err != nil {
			rows = append(rows, map[string]any{"constraint": c, "ok": false})
			continue
		}
		for _, in := range ncCheckVersions {
			v, err := masterminds.NewVersion(in)
			if err != nil {
				continue
			}
			rows = append(rows, map[string]any{
				"constraint": c, "ok": true, "version": in, "check": cs.Check(v),
			})
		}
	}
	return rows
}

var ncDateConstraints = []string{
	">= 2026-08-26T00:00:00Z", ">= 2022-06-16T00:00:00Z", ">= 2026-08-14T00:00:00Z",
	"2026-09-15T00:00:00Z", "= 2026-09-15T00:00:00Z", "!= 2026-09-15T00:00:00Z",
	"> 2026-09-15T00:00:00Z", "< 2026-09-15T00:00:00Z", "<= 2026-09-15T00:00:00Z",
	"=> 2026-09-15T00:00:00Z", "=< 2026-09-15T00:00:00Z",
	"> 2026-09-01T00:00:00Z <= 2026-09-30T00:00:00Z", "> 2026-09-01T00:00:00Z, <= 2026-09-30T00:00:00Z",
	"2026-09-01T00:00:00Z - 2026-09-30T00:00:00Z", "< 2020-01-01T00:00:00Z || > 2026-09-01T00:00:00Z",
	">= 2026", ">= 2026-09", ">= 2026-09-15", ">= 2026-09-15T00:00", ">= 2026-09-15T00:00:00.5Z",
	">= 2026-09-15T05:30:00+05:30", ">= 2026-09-15T00:00:00", "", "bogus", ">= 2026-13-01T00:00:00Z",
	">= 2026-09-15T00:00:00Z extra",
}

var ncDates = []string{
	"2026-09-15T00:00:00Z", "2026-09-14T00:00:00Z", "2026-09-16T00:00:00Z", "2026-08-26T00:00:00Z",
	"2022-06-16T00:00:00Z", "2020-01-01T00:00:00Z", "2026-09-01T00:00:00Z", "2026-09-30T00:00:00Z",
	"2026-10-01T00:00:00Z", "2019-12-31T00:00:00Z",
}

func ncDateCheckAll() []map[string]any {
	rows := make([]map[string]any, 0, len(ncDateConstraints)*(1+len(ncDates)))
	for _, c := range ncDateConstraints {
		cs, err := date_constraints.NewConstraint(c)
		if err != nil {
			rows = append(rows, map[string]any{"constraint": c, "ok": false})
			continue
		}
		for _, in := range ncDates {
			t, err := time.Parse(time.RFC3339, in)
			if err != nil {
				panic(err)
			}
			rows = append(rows, map[string]any{
				"constraint": c, "ok": true, "date": in, "check": cs.Check(&t),
			})
		}
	}
	return rows
}

// validateConfigEntry (product_notices.go:201), verbatim, over a defaulted config with a few
// settings pinned to known values.
func ncValidateConfigEntry(conf *model.Config, path string, expectedValue any) (bool, bool) {
	value, found := config.GetValueByPath(strings.Split(path, "."), *conf)
	if !found {
		return false, false
	}
	return ncTypedEqual(value, expectedValue), true
}

func ncTypedEqual(value any, expectedValue any) bool {
	// reflect.ValueOf(value).IsNil() / Elem() / Interface() == expectedValue, as the app does.
	switch v := value.(type) {
	case *string:
		if v == nil {
			return expectedValue == nil
		}
		return any(*v) == expectedValue
	case *bool:
		if v == nil {
			return expectedValue == nil
		}
		return any(*v) == expectedValue
	case *int:
		if v == nil {
			return expectedValue == nil
		}
		return any(*v) == expectedValue
	case *int64:
		if v == nil {
			return expectedValue == nil
		}
		return any(*v) == expectedValue
	}
	return false
}

func ncConfigEntryAll() []map[string]any {
	conf := &model.Config{}
	conf.SetDefaults()
	conf.ServiceSettings.CollapsedThreads = model.NewPointer("always_on")
	conf.ServiceSettings.EnableLinkPreviews = model.NewPointer(true)
	conf.ServiceSettings.PostEditTimeLimit = model.NewPointer(-1)
	conf.FileSettings.MaxFileSize = model.NewPointer(int64(1048576))
	conf.ImageProxySettings.ImageProxyType = model.NewPointer("atmos/camo")
	conf.LdapSettings.LoginIdAttribute = nil

	// Each expectation as JSON, decoded the way a notice's `serverConfig` map arrives.
	cases := []struct {
		path string
		json string
	}{
		{"ServiceSettings.CollapsedThreads", `"always_on"`},
		{"ServiceSettings.CollapsedThreads", `"default_on"`},
		{"ServiceSettings.CollapsedThreads", `true`},
		{"ServiceSettings.EnableLinkPreviews", `true`},
		{"ServiceSettings.EnableLinkPreviews", `false`},
		{"ServiceSettings.EnableLinkPreviews", `"true"`},
		{"ServiceSettings.PostEditTimeLimit", `-1`},
		{"ServiceSettings.PostEditTimeLimit", `"-1"`},
		{"FileSettings.MaxFileSize", `1048576`},
		{"ImageProxySettings.ImageProxyType", `"atmos/camo"`},
		{"BleveSettings.EnableSearching", `true`},
		{"BleveSettings.EnableSearching", `false`},
		{"LdapSettings.LoginIdAttribute", `null`},
		{"LdapSettings.LoginIdAttribute", `""`},
		{"ServiceSettings.NoSuchSetting", `true`},
		{"NoSuchSection.Setting", `true`},
		{"ServiceSettings", `true`},
		{"ServiceSettings.CollapsedThreads.Deeper", `"x"`},
	}
	rows := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		var expected any
		if err := json.Unmarshal([]byte(c.json), &expected); err != nil {
			panic(err)
		}
		match, found := ncValidateConfigEntry(conf, c.path, expected)
		rows = append(rows, map[string]any{
			"path": c.path, "expected": json.RawMessage(c.json), "found": found, "match": match,
		})
	}
	return rows
}
