package main

// Behavioural oracle for the link-selection half of channels/app/post_metadata.go — which link
// of a message gets previewed, which images get measured, which links are permalinks — written to
// fixtures/behaviour_link_preview.json.
//
// The functions are unexported, so each is copied below **verbatim** from post_metadata.go (the
// line of each is on its copy) with only the logger calls removed. What they call is Go's own:
// `markdown.Inspect` (shared/markdown), `idna.Lookup.ToASCII` (x/net/idna), `url.Parse`,
// `regexp`, `model.RemoveDuplicateStrings`. So the corpus pins the parser, the IDNA mapping and
// URL resolution rather than a transcription of them.
//
// Four traps worth recording:
//
//   - `isLinkAllowedForPreview` does its `url.Parse` and `ToASCII` **inside** the loop over the
//     restricted domains, so with `RestrictLinkPreviews` empty a link that would not even parse
//     is allowed, and with it set an IPv6 link is refused (`:` is disallowed under STD3 rules).
//   - The domain test is `strings.Contains`, not a suffix match.
//   - `getFirstLink` evaluates `isLinkAllowedForPreview` only while nothing has been found, and a
//     refused first link lets the **second** link through.
//   - `model.RemoveDuplicateStrings` sorts, so the images are fetched in byte order.
//
// Determinism: fixed inputs only.

import (
	"encoding/json"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"golang.org/x/net/idna"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/shared/markdown"
)

// post_metadata.go:735
func lpIsLinkAllowedForPreview(restrict, link string) bool {
	domains := lpNormalizeDomains(restrict)
	for _, d := range domains {
		parsed, err := url.Parse(link)
		if err != nil {
			return false
		}
		cleaned, err := idna.Lookup.ToASCII(parsed.Hostname())
		if err != nil {
			return false
		}
		if strings.Contains(cleaned, d) {
			return false
		}
	}
	return true
}

// post_metadata.go:760
func lpNormalizeDomains(domains string) []string {
	return strings.Fields(
		strings.TrimSpace(
			strings.ToLower(
				strings.ReplaceAll(
					strings.ReplaceAll(domains, "@", " "),
					",", " "),
			),
		),
	)
}

// post_metadata.go:785
func lpGetFirstLink(restrict, str string) string {
	firstLink := ""
	markdown.Inspect(str, func(blockOrInline any) bool {
		if v, ok := blockOrInline.(*markdown.Autolink); ok {
			if link := v.Destination(); firstLink == "" && lpIsLinkAllowedForPreview(restrict, link) {
				firstLink = link
			}
		}
		return true
	})
	return firstLink
}

// post_metadata.go:799
func lpGetImages(restrict, str string) []string {
	images := []string{}
	markdown.Inspect(str, func(blockOrInline any) bool {
		switch v := blockOrInline.(type) {
		case *markdown.InlineImage:
			if link := v.Destination(); lpIsLinkAllowedForPreview(restrict, link) {
				images = append(images, link)
			}
		case *markdown.ReferenceImage:
			if link := v.ReferenceDefinition.Destination(); lpIsLinkAllowedForPreview(restrict, link) {
				images = append(images, link)
			}
		}
		return true
	})
	return images
}

// post_metadata.go:817
func lpLooksLikeAPermalink(url, siteURL string) bool {
	path, hasPrefix := strings.CutPrefix(strings.TrimSpace(url), siteURL)
	if !hasPrefix {
		return false
	}
	path = strings.TrimPrefix(path, "/")
	matched, _ := regexp.MatchString(`^[0-9a-z_-]{1,64}/pl/[a-z0-9]{26}$`, path)
	return matched
}

// post_metadata.go:1063
func lpResolveMetadataURL(requestURL string, siteURL string) string {
	base, err := url.Parse(siteURL)
	if err != nil {
		return ""
	}
	resolved, err := base.Parse(requestURL)
	if err != nil {
		return ""
	}
	return resolved.String()
}

var lpRestricts = []string{
	"",
	"example.com",
	"@Example.COM, other.org",
	"  mock  ",
	"127.0.0.1",
	"xn--",
}

var lpLinks = []string{
	"http://example.com/a",
	"https://sub.example.com:8443/x?y#z",
	"http://notexample.com",
	"http://EXAMPLE.com/UPPER",
	"http://127.0.0.1:9/mock/page",
	"http://[::1]:80/v6",
	"http://ex_ample.com/",
	"http://-lead.com/",
	"http://trail-.com/",
	"http://ab--cd.com/",
	"http://xn--nxasmq6b.com/",
	"http://xn--zz.com/",
	"http://münchen.de/",
	"http://%zz/bad-escape",
	"http://a..b/",
	"http://host with space/",
	"ftp://example.com/file",
	"www.example.com",
	"http://",
	"mailto:someone@example.com",
	"http://user:pass@mock.test/",
	"http://ÉCOLE.fr/",
}

var lpMessages = []string{
	"",
	"no links here",
	"see http://example.com/a and http://other.org/b",
	"www.example.com is first, then https://second.test/",
	"https://first.test/one https://second.test/two",
	"`http://in.code/span` then http://outside.test/",
	"```\nhttp://in.fence/\n```\nhttp://after.fence/",
	"[text](http://markdown.link/) and http://auto.link/",
	"![alt](http://image.test/a.png) and http://auto.link/",
	"![a](http://image.test/b.png) ![b](http://image.test/a.png) ![c](http://image.test/b.png)",
	"![ref][img]\n\n[img]: http://image.test/ref.png",
	"![dims](http://image.test/dims.png =100x200)",
	"<http://angle.test/>",
	"http://example.com/restricted then http://mock.test/allowed",
	"trailing punctuation http://punct.test/path).",
	"http://paren.test/a_(b) end",
	"> quoted http://quote.test/",
	"- item http://list.test/",
	"HTTP://UPPER.TEST/",
	"ftp://ftp.test/file",
	"http://127.0.0.1:9/mock/page and http://[::1]/v6",
	"@user http://mention.test/",
	"![](data:image/png;base64,AAAA)",
	"![](relative/path.png) ![](/abs/path.png)",
	"http://localhost:8065/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://x.test/?q=1&r=2#frag",
	"www2.example.com/page",
	"https://example.com/é/ü",
}

var lpSiteURLs = []string{
	"",
	"http://localhost:8065",
	"http://localhost:8065/",
	"https://chat.example.com/sub",
	"%zz",
}

var lpPermalinkURLs = []string{
	"http://localhost:8065/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/Slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/slice_team-1/pl/0123456789abcdefghijklmnop",
	"http://localhost:8065/slice-team/pl/AAAAAAAAAAAAAAAAAAAAAAAAAA",
	"http://localhost:8065//slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"  http://localhost:8065/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa\n",
	"http://localhost:8065/a/b/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/" + strings.Repeat("t", 64) + "/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/" + strings.Repeat("t", 65) + "/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:80650/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"https://chat.example.com/sub/team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa",
	"http://localhost:8065/slice-team/pl/aaaaaaaaaaaaaaaaaaaaaaaaaa?x=1",
}

var lpRequestURLs = []string{
	"http://example.com/a",
	"HTTP://Example.COM/a/../b/./c",
	"/relative/path.png",
	"relative/path.png",
	"//protocol.relative/x",
	"?only=query",
	"#frag",
	"http://example.com/a b",
	"http://example.com/%41%2f",
	"http://[::1]:80/x",
	"data:image/png;base64,AAAA",
	"http://%zz/",
	"",
	"https://example.com/é",
	"http://example.com:8080",
	"http://example.com?",
}

func writeLinkPreviewBehaviourFixture(outDir string) error {
	var allowed []map[string]any
	for _, r := range lpRestricts {
		for _, l := range lpLinks {
			allowed = append(allowed, map[string]any{
				"restrict": r, "link": l, "allowed": lpIsLinkAllowedForPreview(r, l),
			})
		}
	}

	var normalized []map[string]any
	for _, r := range append(lpRestricts, "a,b@c\td e", "ÄBC.de", ",,,", "@") {
		normalized = append(normalized, map[string]any{"input": r, "domains": lpNormalizeDomains(r)})
	}

	var ascii []map[string]any
	hosts := []string{
		"example.com", "EXAMPLE.com", "ex_ample.com", "-lead.com", "trail-.com", "ab--cd.com",
		"xn--nxasmq6b.com", "xn--zz.com", "münchen.de", "127.0.0.1", "::1", "", "a..b", ".a",
		"a.", "exa mple.com", "ÉCOLE.fr", "ß.de", "a-b.c-d", "123", "a.b.c.d.e", "xn--", "UPPER",
		"host!", "host~name", "ho*st",
	}
	for _, h := range hosts {
		out, err := idna.Lookup.ToASCII(h)
		ascii = append(ascii, map[string]any{"host": h, "ascii": out, "error": err != nil})
	}

	var firstLinks, images []map[string]any
	for _, r := range lpRestricts {
		for _, m := range lpMessages {
			firstLinks = append(firstLinks, map[string]any{"restrict": r, "message": m, "first_link": lpGetFirstLink(r, m)})
			found := lpGetImages(r, m)
			sorted := model.RemoveDuplicateStrings(append([]string(nil), found...))
			images = append(images, map[string]any{"restrict": r, "message": m, "images": found, "deduplicated": sorted})
		}
	}

	var permalinks []map[string]any
	for _, s := range lpSiteURLs {
		for _, u := range lpPermalinkURLs {
			permalinks = append(permalinks, map[string]any{"site_url": s, "url": u, "matches": lpLooksLikeAPermalink(u, s)})
		}
	}

	var resolved []map[string]any
	for _, s := range lpSiteURLs {
		for _, u := range lpRequestURLs {
			resolved = append(resolved, map[string]any{"site_url": s, "url": u, "resolved": lpResolveMetadataURL(u, s)})
		}
	}

	out := map[string]any{
		"is_link_allowed_for_preview": allowed,
		"normalize_domains":           normalized,
		"idna_lookup_to_ascii":        ascii,
		"first_link":                  firstLinks,
		"images":                      images,
		"looks_like_a_permalink":      permalinks,
		"resolve_metadata_url":        resolved,
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_link_preview.json"), append(blob, '\n'), 0o644)
}
