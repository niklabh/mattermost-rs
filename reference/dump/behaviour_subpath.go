package main

// Behavioural oracle for `utils.GetSubpathFromConfig` (channels/utils/subpath.go:242), written to
// fixtures/behaviour_subpath.json.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-app`'s tests and never
// `mm-model`'s — the same rule behaviour_password.go states ([D-031]).
//
// # Why this needs an oracle at all when its two ingredients already have one
//
// `go_url::go_parse` and `go_path::clean` are both ported and both already asserted against Go's
// own answers. What is *not* covered by either is the four-line composition between them, and it
// has three branches a reader can plausibly get wrong:
//
//  1. A **nil** SiteURL is "/" — but so is a SiteURL whose parsed path is empty, and those are
//     different inputs reaching the same answer. A port that collapsed them would be right here
//     and wrong the moment either branch changed.
//  2. A **parse failure** returns `("", err)` and `RemoveSessionCookie` ignores the error
//     (`subpath, _ :=`, context.go:181) — so the cookie's Path becomes the empty string, which
//     `net/http` omits from the Set-Cookie header entirely. The failure mode is a cookie with no
//     Path, not a cookie with Path=/.
//  3. `path.Clean` is applied to `u.Path`, the **decoded** path, not to `u.RawPath`. So
//     `/mattermost%2Fx` cleans to `/mattermost/x`, which is one directory deeper than the URL
//     said.
//
// The corpus records the raw return values, error included, rather than the cookie — the cookie is
// the caller's business and is asserted against the running Go server instead.
//
// # The function body is copied verbatim, and that is deliberate
//
// Importing `channels/utils` drags in goldmark (utils/markdown.go), which is not in this
// generator's go.sum and would mean a network fetch to add. So `getSubpathFromSiteURL` below is
// subpath.go:242-259 transcribed line for line, calling Go's **real** `url.Parse` and
// `path.Clean`. The ingredients are therefore Go's own and only the eight lines of glue are
// transcribed — the same arrangement behaviour.go uses for the unexported identifier regexes, and
// it carries the same rule: **copy any upstream change character for character**, because the Rust
// side asserts against these results and a silent edit here weakens the test rather than failing
// it.

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path"
	"path/filepath"
)

// subpathCorpus covers the branches above plus the shapes an operator actually writes: a bare
// origin, a one- and two-segment subpath, trailing slashes, and the deployment forms the
// Mattermost docs use. The last few are inputs `url.Parse` rejects or treats surprisingly.
var subpathCorpus = []string{
	"",
	"/",
	"http://localhost:8065",
	"http://localhost:8065/",
	"https://mattermost.example.com",
	"https://mattermost.example.com/",
	"https://example.com/mattermost",
	"https://example.com/mattermost/",
	"https://example.com/a/b",
	"https://example.com/a/b/",
	"https://example.com/a//b",
	"https://example.com/a/./b",
	"https://example.com/a/../b",
	"https://example.com/..",
	"https://example.com/.",
	"https://example.com//",
	"https://example.com/mattermost%2Fx",
	"https://example.com/spaced%20path",
	"https://user:pw@example.com/sub",
	"https://example.com:8065/sub?q=1",
	"https://example.com/sub#frag",
	"mattermost",
	"/mattermost",
	"//example.com/sub",
	"http://%zz/sub",
	"http://[::1]:8065/sub",
	"http://exa mple.com/sub",
	"\x7f",
}

// getSubpathFromSiteURL is channels/utils/subpath.go:242-259, transcribed. The `config == nil`
// branch collapses into `siteURL == nil` here because the only thing the original reads out of the
// config is that one pointer.
func getSubpathFromSiteURL(siteURL *string) (string, error) {
	if siteURL == nil {
		return "/", nil
	}

	u, err := url.Parse(*siteURL)
	if err != nil {
		return "", fmt.Errorf("failed to parse SiteURL from config: %w", err)
	}

	if u.Path == "" {
		return "/", nil
	}

	return path.Clean(u.Path), nil
}

func writeSubpathBehaviourFixture(outDir string) error {
	rows := make([]map[string]any, 0, len(subpathCorpus)+1)

	// The nil case, which no string in the corpus can express: `SiteURL == nil` short-circuits
	// before `url.Parse` is ever reached (subpath.go:245).
	nilSubpath, nilErr := getSubpathFromSiteURL(nil)
	rows = append(rows, map[string]any{
		"site_url": nil,
		"subpath":  nilSubpath,
		"failed":   nilErr != nil,
	})

	for _, siteURL := range subpathCorpus {
		subpath, err := getSubpathFromSiteURL(&siteURL)
		rows = append(rows, map[string]any{
			"site_url": siteURL,
			"subpath":  subpath,
			"failed":   err != nil,
		})
	}

	// Guard the transcription: `path.Clean` and `url.Parse` are Go's, but the glue is not, so at
	// least assert the two invariants the original states outright.
	if s, err := getSubpathFromSiteURL(pointerTo("http://x")); err != nil || s != "/" {
		return errors.New("transcription drift: an empty path must be \"/\"")
	}
	if s, err := getSubpathFromSiteURL(pointerTo("http://x/a/b/")); err != nil || s != "/a/b" {
		return errors.New("transcription drift: a trailing slash must be cleaned away")
	}

	out := map[string]any{
		"get_subpath_from_config": rows,
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_subpath.json"), append(blob, '\n'), 0o644)
}

func pointerTo(s string) *string { return &s }
