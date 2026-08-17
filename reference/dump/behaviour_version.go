package main

// Behavioural oracle for model/version.go, written to fixtures/behaviour_version.json.
//
// version.go has no wire types, so there is no serialization fixture to pin it. Everything it
// exports is either a table or a function over that table, and both are easy to get subtly
// wrong:
//
//  1. `versions` and `versionsWithoutHotFixes` are **unexported**, so a Rust port has to
//     transcribe the release list by hand. A transcription error is invisible — the code still
//     compiles and every function still returns *something*. So this oracle extracts the literal
//     from the Go source with go/parser and records it verbatim, and separately records the
//     derived hotfix-free list by walking GetPreviousVersion from CurrentVersion. The two
//     together pin both the transcription and the dedup.
//
//  2. `SplitVersion` swallows every strconv.ParseInt error, and Go's ParseInt does not fail the
//     same way Rust's str::parse::<i64> does. On a range error Go returns the **clamped** bound
//     (MaxInt64/MinInt64) alongside the error, and since version.go discards the error that
//     clamped value is what SplitVersion returns. Rust's parse() returns Err for the same input,
//     so a naive `unwrap_or(0)` port silently answers 0 where Go answers 9223372036854775807.
//     The corpus below drives that difference on purpose.
//
// versionsWithoutHotFixes cannot be read directly, but it can be *observed*: GetPreviousVersion
// walks it, so chaining it from CurrentVersion reproduces the list from index 0 down to the last
// entry (which returns ""). That is Go's own answer rather than a re-implementation of init().

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"strconv"

	"github.com/mattermost/mattermost/server/public/model"
)

// versionGoPath is relative to reference/dump, which is where the generator is run from.
const versionGoPath = "../mattermost/server/public/model/version.go"

type splitVersionCase struct {
	In    string `json:"in"`
	Major int64  `json:"major"`
	Minor int64  `json:"minor"`
	Patch int64  `json:"patch"`
}

func writeVersionBehaviourFixture(outDir string) error {
	versions, err := parseVersionsLiteral(versionGoPath)
	if err != nil {
		return err
	}
	if len(versions) == 0 {
		return fmt.Errorf("parsed an empty versions list from %s", versionGoPath)
	}
	if versions[0] != model.CurrentVersion {
		return fmt.Errorf("versions[0]=%q but model.CurrentVersion=%q", versions[0], model.CurrentVersion)
	}

	out := map[string]any{
		"versions":                       versions,
		"current_version":                model.CurrentVersion,
		"versions_without_hotfixes":      versionsWithoutHotFixesObserved(),
		"split_version":                  splitVersionAll(),
		"get_previous_version":           getPreviousVersionAll(),
		"is_current_version":             isCurrentVersionAll(),
		"is_previous_versions_supported": isPreviousVersionsSupportedAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_version.json"), append(blob, '\n'), 0o644)
}

// --- the unexported tables -----------------------------------------------------

// parseVersionsLiteral reads `var versions = []string{...}` straight out of version.go. Doing
// this instead of transcribing the list means an upstream release bump fails the Rust test
// rather than leaving the port pinned to a stale table.
func parseVersionsLiteral(path string) ([]string, error) {
	fset := token.NewFileSet()
	file, err := parser.ParseFile(fset, path, nil, 0)
	if err != nil {
		return nil, fmt.Errorf("parsing %s: %w", path, err)
	}

	for _, decl := range file.Decls {
		gen, ok := decl.(*ast.GenDecl)
		if !ok || gen.Tok != token.VAR {
			continue
		}
		for _, spec := range gen.Specs {
			value, ok := spec.(*ast.ValueSpec)
			if !ok || len(value.Names) != 1 || value.Names[0].Name != "versions" {
				continue
			}
			if len(value.Values) != 1 {
				return nil, fmt.Errorf("versions has %d initialisers, want 1", len(value.Values))
			}
			lit, ok := value.Values[0].(*ast.CompositeLit)
			if !ok {
				return nil, fmt.Errorf("versions is not a composite literal")
			}
			out := make([]string, 0, len(lit.Elts))
			for _, elt := range lit.Elts {
				basic, ok := elt.(*ast.BasicLit)
				if !ok || basic.Kind != token.STRING {
					return nil, fmt.Errorf("versions element is not a string literal: %#v", elt)
				}
				unquoted, err := strconv.Unquote(basic.Value)
				if err != nil {
					return nil, fmt.Errorf("unquoting %s: %w", basic.Value, err)
				}
				out = append(out, unquoted)
			}
			return out, nil
		}
	}
	return nil, fmt.Errorf("no `var versions` declaration found in %s", path)
}

// versionsWithoutHotFixesObserved rebuilds the unexported list by walking the only function that
// reads it. GetPreviousVersion returns "" for the final entry, which terminates the walk.
func versionsWithoutHotFixesObserved() []string {
	major, minor, _ := model.SplitVersion(model.CurrentVersion)
	current := fmt.Sprintf("%v.%v.0", major, minor)

	out := []string{current}
	for {
		next := model.GetPreviousVersion(current)
		if next == "" {
			return out
		}
		out = append(out, next)
		current = next
	}
}

// --- SplitVersion ---------------------------------------------------------------

// splitVersionCorpus targets the three things SplitVersion actually does: split on ".", take at
// most the first three parts, and ParseInt each with the error thrown away. The range cases at
// the end are the ones Rust gets wrong by default.
var splitVersionCorpus = []string{
	"", ".", "..", "...", "0", "1", "1.2", "1.2.3", "1.2.3.4", "1.2.3.4.5",
	"11.11.0", "10.12.0", "0.5.0", "1.2.1", "4.8.1", "4.7.2", "9.11.0",
	"01.02.03", "1.02.0", "0001.0.0",
	"+1.2.3", "-1.2.3", "1.-2.3", "1.2.-3", "-0.-0.-0",
	"v1.2.3", "1.2.3-rc1", "1.2.x", "x.y.z", "garbage",
	" 1.2.3", "1.2.3 ", "1. 2.3", "1.2. 3",
	"1_000.2.3", "0x10.0.0", "0b1.0.0", "1e3.0.0", "١.٢.٣",
	".1.2", "1..2", "1.2.",
	"9223372036854775807.0.0", "9223372036854775808.0.0",
	"99999999999999999999.0.0", "0.99999999999999999999.0", "0.0.99999999999999999999",
	"-9223372036854775808.0.0", "-9223372036854775809.0.0", "-99999999999999999999.0.0",
	"99999999999999999999abc.0.0", "9999999999999999999999999999999999999999.0.0",
}

func splitVersionAll() []splitVersionCase {
	res := make([]splitVersionCase, 0, len(splitVersionCorpus))
	for _, in := range splitVersionCorpus {
		major, minor, patch := model.SplitVersion(in)
		res = append(res, splitVersionCase{In: in, Major: major, Minor: minor, Patch: patch})
	}
	return res
}

// --- the three table lookups -----------------------------------------------------

// versionQueryCorpus is shared by the three functions that reduce their input to "major.minor.0"
// and look it up. It mixes real releases, hotfix patches (which must map onto their .0), the
// boundaries of the four-release support window, the oldest and newest entries, and unparseable
// input (which collapses to "0.0.0" and must miss).
var versionQueryCorpus = []string{
	"11.11.0", "11.11.5", "11.11", "11.11.0.1",
	"11.10.0", "11.9.0", "11.8.0", "11.7.0", "11.6.0",
	"11.0.0", "10.12.0", "10.0.0", "9.11.0",
	"4.8.1", "4.8.0", "4.7.2", "4.7.1", "4.7.0",
	"1.2.1", "1.2.0", "1.1.0",
	"0.5.0", "0.5.1", "0.6.0", "0.7.1",
	"", "garbage", "0.0.0", "99.99.0", "11.99.0", "99.11.0",
	"-1.-1.0", "9223372036854775807.0.0", "99999999999999999999.11.0",
}

func getPreviousVersionAll() map[string]string {
	res := make(map[string]string, len(versionQueryCorpus))
	for _, in := range versionQueryCorpus {
		res[in] = model.GetPreviousVersion(in)
	}
	return res
}

func isCurrentVersionAll() map[string]bool {
	res := make(map[string]bool, len(versionQueryCorpus))
	for _, in := range versionQueryCorpus {
		res[in] = model.IsCurrentVersion(in)
	}
	return res
}

func isPreviousVersionsSupportedAll() map[string]bool {
	res := make(map[string]bool, len(versionQueryCorpus))
	for _, in := range versionQueryCorpus {
		res[in] = model.IsPreviousVersionsSupported(in)
	}
	return res
}
