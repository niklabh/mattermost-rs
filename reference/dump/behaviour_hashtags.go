package main

// Behavioural oracle for `model.ParseHashtags` (utils.go:750), written into
// fixtures/behaviour_utils.json under `parse_hashtags`.
//
// The function is fifteen lines and four regular expressions, and every one of the four is a
// transcription hazard: **RE2's Perl classes are ASCII where the Rust `regex` crate's are
// Unicode.** In Go `\d` is `[0-9]` and `\s` is `[\t\n\f\r ]`; in Rust they are `\p{Nd}` and the
// Unicode White_Space property. So a pattern copied character for character accepts `#tag١` — an
// Arabic-Indic digit — in Rust and rejects it in Go. `\pL` is the one class that means the same
// thing in both.
//
// The other traps, in the order a reader hits them:
//
//  1. **`puncStart` excludes `#` from what it strips** (`[^\pL\d\s#]+`), which is the only reason
//     a hashtag survives leading punctuation at all: `(#tag)` loses the parenthesis and keeps the
//     pound. `puncEnd` does **not** exclude it, so `#tag#` becomes `#tag`.
//
//  2. **`hashtagStart` collapses two or more pounds to one**, so `##tag` is a hashtag. One pound
//     is left alone by that pattern and matched by `validHashtag` directly.
//
//  3. **`validHashtag` needs a letter after the `#` and a letter-or-digit at the end**, with at
//     least two characters total: `#a` fails, `#ab` passes, `#1a` fails on the first character
//     class and `#a-` fails on the last.
//
//  4. **There is no de-duplication and no sorting.** A word repeated in the message is emitted
//     twice, in order, which is what makes the stored `Hashtags` column a multiset.
//
//  5. **The 1000-byte cap is measured in bytes and cut at 999**, then rolled back to the last
//     space — so a multi-byte rune split by the cut is discarded along with the rest of its
//     hashtag, and the result is always valid UTF-8 despite the byte slice. A single hashtag
//     longer than the cap yields the **empty string**, because the only space in the prefix is the
//     one at index 0.
//
//  6. Both return values are `strings.TrimSpace`d, and the plain half is built from the *trimmed*
//     words, so `hello!` reaches it as `hello`. Nothing in the server reads the plain half, but
//     recording it is what stops a port from quietly swapping the two.

import (
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

type hashtagCase struct {
	In       string `json:"in"`
	Hashtags string `json:"hashtags"`
	Plain    string `json:"plain"`
}

func parseHashtagsAll() []hashtagCase {
	inputs := []string{
		"",
		"   ",
		"no hashtags here",
		"#tag",
		"#tag and #another",
		"#tag #tag",
		"##tag",
		"###tag",
		"#a",
		"#ab",
		"#1a",
		"#a1",
		"#a-b_c.d",
		"#a-",
		"#a.",
		"#a_",
		"(#tag)",
		"[#tag]",
		"«#tag»",
		"#tag.",
		"#tag!",
		"#tag#",
		"#tag##",
		"-#tag",
		"#-tag",
		"#_tag",
		"#.tag",
		"a#tag",
		"#tag@example",
		"#é",
		"#éa",
		"#タグ",
		"#tagé",
		"#tag١",   // Arabic-Indic digit: \pNd but not RE2's \d
		"#tag１",   // fullwidth digit: same trap
		"#taǵ",    // combining acute after a letter: category Mn, not L
		"#日本語",
		"tabs\tand\nnewlines #tag",
		"nbsp #tag",
		"ideographic　#tag",
		"#tag #other",
		"#tag mixed #another plain words #third",
		"#UPPER #lower",
		"#tag1 #tag2 #tag3",
		strings.Repeat("#hashtagofsomelength ", 60),          // over the 1000-byte cap
		strings.Repeat("#日本語のハッシュタグ ", 40),                   // over the cap, multi-byte
		"#" + strings.Repeat("a", 1200),                      // one hashtag over the cap
		"#" + strings.Repeat("é", 600),                       // one multi-byte hashtag over the cap
		strings.Repeat("#ab ", 250) + "#zzz",                 // exactly around the cap boundary
		"plain " + strings.Repeat("#tagged ", 130) + "trail",  // cap with plain words interleaved
	}
	res := make([]hashtagCase, 0, len(inputs))
	for _, in := range inputs {
		hashtags, plain := model.ParseHashtags(in)
		res = append(res, hashtagCase{In: in, Hashtags: hashtags, Plain: plain})
	}
	return res
}
