package main

// Behavioural oracle for `server/public/shared/markdown` — the hand-written CommonMark subset
// behind `markdown.Inspect`, which the mention engine (`app/notification.go:1437`) and the
// `mmaction://` id scan ([D-044]) walk. Written to fixtures/behaviour_markdown.json.
//
// For every input in the corpus this records what the *real* package answers: `RenderHTML`, the
// full `Inspect` event trace (one entry per callback, including the `nil` pop after every node),
// the same trace under a callback that refuses some node types, the reference definitions `Parse`
// returns, and — per paragraph — the raw `ParseInlines` result and its `MergeInlineText`. The
// Rust port asserts all five, byte for byte, in `crates/mm-markdown/src/go_parity.rs`.
//
// The traps a reader of the Go source is most likely to fall into, all of which are in the corpus
// rather than merely described:
//
//  1. **`trimLeftSpace` shrinks the END of the range** (markdown.go:104): it computes the number
//     of leading whitespace bytes and subtracts it from `End`, not adds it to `Position`. It is
//     reachable through `Paragraph.Close` with a paragraph whose first line begins with `\f` or
//     `\v` (the only whitespace the line indentation counter does not strip), and the output
//     loses the last byte of the line.
//  2. **A single trailing space is a hard line break** (inlines.go:171). The second clause of
//     the condition re-tests `raw[pos-1] == ' '`, which the first conjunct already established,
//     so the two-space rule of the spec collapses to one.
//  3. **`Inspect` calls `f(nil)` after a node whether or not `f` returned true for it**: the
//     node stays on the traversal stack when `f` refuses it and is popped — with the nil call —
//     on the next iteration (inspect.go:83). `trace_prune` pins this.
//  4. **There is no `<...>` autolink**, no emphasis, no heading, no thematic break and no HTML
//     block; `*`, `#`, `<` and `_` are plain text. Only the `autolink.go` forms (`scheme://`,
//     `www.`) link, and a scheme needs the `//`.
//  5. **`escapeURL` writes `%X` with no zero padding**, so a byte below 0x10 is `%1`, not `%01`.
//  6. **The `Text` range after an entity is the DECODED length**, so `&#65;&#66;` is two nodes
//     that `MergeInlineText` will not join (their ranges are not adjacent).
//  7. **`checkDomain` never examines the last byte** (`i < len(data)-1`) and walks bytes, not
//     runes, so a multi-byte host character ends the domain scan one byte in; the link is then
//     extended to the next ASCII whitespace anyway, which is why `http://例え.jp` still links.
//  8. **Reference labels compare with `strings.EqualFold`**, which is Unicode simple folding:
//     `[K]` (U+212A KELVIN SIGN) resolves `[k]`, `[ſ]` resolves `[S]`, but
//     `[straße]` does not resolve `[STRASSE]`. The fold orbits come from the Go toolchain's
//     `unicode.SimpleFold` and are emitted as a table for the same reason [D-070] emits
//     `IsPrint`: the property tracks a Unicode version, and the version that matters is Go's.
//  9. **`MaxLen` is bytes, `SetMaxPostRunes` is runes**, four bytes per rune; an input one byte
//     over yields an empty `Document` from `Parse` and no callbacks at all from `Inspect`.
//
// Also written: `crates/mm-markdown/src/go_unicode_generated.rs` — `unicode.IsPunct`,
// `unicode.IsSpace` as inclusive ranges and `unicode.SimpleFold` as pairs, from this toolchain.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"unicode"

	"github.com/mattermost/mattermost/server/public/shared/markdown"
)

type markdownParagraph struct {
	Inlines []map[string]any `json:"inlines"`
	Merged  []map[string]any `json:"merged"`
}

type markdownCase struct {
	Name string `json:"name"`
	// Input is the markdown, or — when Repeat > 0 — the unit repeated Repeat times. The two
	// oversize cases would otherwise put 131 KiB each into a committed fixture.
	Input       string              `json:"input"`
	Repeat      int                 `json:"repeat,omitempty"`
	HTML        string              `json:"html"`
	Trace       []map[string]any    `json:"trace"`
	TracePrune  []map[string]any    `json:"trace_prune"`
	Definitions []map[string]any    `json:"definitions"`
	Paragraphs  []markdownParagraph `json:"paragraphs"`
}

type equalFoldCase struct {
	A  string `json:"a"`
	B  string `json:"b"`
	Eq bool   `json:"eq"`
}

type runeProbe struct {
	Rune    string `json:"rune"`
	IsPunct bool   `json:"is_punct"`
	IsSpace bool   `json:"is_space"`
}

type markdownFixture struct {
	MaxLenDefault int               `json:"max_len_default"`
	Cases         []markdownCase    `json:"cases"`
	Entities      map[string]string `json:"entities"`
	EqualFold     []equalFoldCase   `json:"equal_fold"`
	RuneProbes    []runeProbe       `json:"rune_probes"`
}

// markdownEvent is one Inspect callback as JSON. Every accessor the server reads is recorded, so
// the Rust trace has to agree on structure *and* on the derived strings (Destination() is
// unescaped, Autolink's gets an http:// prefix for www, Info() is unescaped, Code() re-expands
// indentation).
func markdownEvent(node any) map[string]any {
	switch v := node.(type) {
	case nil:
		return map[string]any{"type": "nil"}
	case *markdown.Document:
		return map[string]any{"type": "Document"}
	case *markdown.Paragraph:
		return map[string]any{"type": "Paragraph"}
	case *markdown.List:
		return map[string]any{
			"type":          "List",
			"is_ordered":    v.IsOrdered,
			"ordered_start": v.OrderedStart,
			"is_loose":      v.IsLoose,
			"bullet":        string(rune(v.BulletOrDelimiter)),
		}
	case *markdown.ListItem:
		return map[string]any{"type": "ListItem", "indentation": v.Indentation}
	case *markdown.BlockQuote:
		return map[string]any{"type": "BlockQuote"}
	case *markdown.FencedCode:
		return map[string]any{"type": "FencedCode", "info": v.Info(), "code": v.Code()}
	case *markdown.IndentedCode:
		return map[string]any{"type": "IndentedCode", "code": v.Code()}
	case *markdown.Text:
		return map[string]any{"type": "Text", "text": v.Text, "pos": v.Range.Position, "end": v.Range.End}
	case *markdown.CodeSpan:
		return map[string]any{"type": "CodeSpan", "code": v.Code}
	case *markdown.HardLineBreak:
		return map[string]any{"type": "HardLineBreak"}
	case *markdown.SoftLineBreak:
		return map[string]any{"type": "SoftLineBreak"}
	case *markdown.InlineLink:
		return map[string]any{"type": "InlineLink", "destination": v.Destination(), "title": v.Title()}
	case *markdown.InlineImage:
		return map[string]any{"type": "InlineImage", "destination": v.Destination(), "title": v.Title()}
	case *markdown.ReferenceLink:
		return map[string]any{"type": "ReferenceLink", "destination": v.Destination(), "title": v.Title(), "label": v.Label()}
	case *markdown.ReferenceImage:
		return map[string]any{"type": "ReferenceImage", "destination": v.Destination(), "title": v.Title(), "label": v.Label()}
	case *markdown.Autolink:
		return map[string]any{"type": "Autolink", "destination": v.Destination()}
	case *markdown.Emoji:
		return map[string]any{"type": "Emoji", "name": v.Name}
	}
	panic(fmt.Sprintf("markdownEvent: unhandled %T", node))
}

// markdownPrune is the refusing callback for trace_prune: it declines four node types, two blocks
// and two inlines, so the fixture pins both that their children are skipped and that the nil pop
// still arrives.
func markdownPrune(node any) bool {
	switch node.(type) {
	case *markdown.BlockQuote, *markdown.List, *markdown.InlineLink, *markdown.InlineImage:
		return false
	}
	return true
}

func markdownCollectParagraphs(block markdown.Block, defs []*markdown.ReferenceDefinition, out *[]markdownParagraph) {
	switch v := block.(type) {
	case *markdown.Document:
		for _, c := range v.Children {
			markdownCollectParagraphs(c, defs, out)
		}
	case *markdown.BlockQuote:
		for _, c := range v.Children {
			markdownCollectParagraphs(c, defs, out)
		}
	case *markdown.List:
		for _, c := range v.Children {
			markdownCollectParagraphs(c, defs, out)
		}
	case *markdown.ListItem:
		for _, c := range v.Children {
			markdownCollectParagraphs(c, defs, out)
		}
	case *markdown.Paragraph:
		raw := v.ParseInlines(defs)
		inlines := []map[string]any{}
		for _, inline := range raw {
			markdown.InspectInline(inline, func(x markdown.Inline) bool {
				inlines = append(inlines, markdownEvent(x))
				return true
			})
		}
		// MergeInlineText reuses the input's backing array, so the raw trace above had to be
		// taken first.
		merged := []map[string]any{}
		for _, inline := range markdown.MergeInlineText(raw) {
			markdown.InspectInline(inline, func(x markdown.Inline) bool {
				merged = append(merged, markdownEvent(x))
				return true
			})
		}
		*out = append(*out, markdownParagraph{Inlines: inlines, Merged: merged})
	}
}

func markdownRun(name, unit string, repeat int) markdownCase {
	input := unit
	if repeat > 0 {
		input = strings.Repeat(unit, repeat)
	}
	c := markdownCase{
		Name:        name,
		Input:       unit,
		Repeat:      repeat,
		HTML:        markdown.RenderHTML(input),
		Trace:       []map[string]any{},
		TracePrune:  []map[string]any{},
		Definitions: []map[string]any{},
		Paragraphs:  []markdownParagraph{},
	}
	markdown.Inspect(input, func(node any) bool {
		c.Trace = append(c.Trace, markdownEvent(node))
		return true
	})
	markdown.Inspect(input, func(node any) bool {
		c.TracePrune = append(c.TracePrune, markdownEvent(node))
		return markdownPrune(node)
	})
	doc, defs := markdown.Parse(input)
	for _, d := range defs {
		c.Definitions = append(c.Definitions, map[string]any{
			"label": d.Label(), "destination": d.Destination(), "title": d.Title(),
		})
	}
	markdownCollectParagraphs(doc, defs, &c.Paragraphs)
	return c
}

// markdownCorpus is the corpus. Names are for the failure message; inputs are chosen so that the
// right answer and a plausible wrong one differ.
var markdownCorpus = []struct{ name, in string }{
	// Paragraphs and line endings.
	{"empty", ""},
	{"whitespace only", "   "},
	{"newlines only", "\n\n\n"},
	{"plain", "hello"},
	{"soft break", "hello\nworld"},
	{"two paragraphs", "hello\n\nworld"},
	{"leading spaces", "  leading spaces\n   more"},
	{"hard break three spaces", "trailing spaces   \nnext"},
	{"hard break two spaces", "two  \nnext"},
	{"hard break one space", "one space \nnext"},
	// Mutation survivors, 2026-09-13: the `p >= 2` bound in parseLineEnding is only visible
	// when the line ending is at raw position 2 (one character, one space), and the 1000-byte
	// label rule (`position-originalPosition >= 1000`, so 999 content bytes) is only reachable
	// through a reference — an inline link never calls parseLinkLabel.
	{"hard break one space at position two", "a \nb"},
	{"hard break one space at position three", "ab \nc"},
	{"soft break space at position one", " \nb"},
	{"hard break tab", "tab\t\nnext"},
	{"hard break tab space", "tab then space\t \nnext"},
	{"backslash before newline", "backslash\\\nnext"},
	{"crlf", "crlf\r\nline\r\n\r\npara"},
	{"lone cr", "cr\rline\rmore"},
	{"mixed endings", "mixed\r\n\rlines\n"},
	{"trailing newline", "text\n"},
	{"trailing spaces at end", "text   "},
	{"form feed leads paragraph", "\x0chello"},
	{"vertical tab leads paragraph", "\x0b vertical tab"},
	{"form feed mid", "a\x0cb"},
	{"nbsp only", "\u00a0"},
	{"nbsp mid", "a\u00a0b"},
	{"ideographic space only", "\u3000"},
	{"line separator", "\u2028line sep"},
	{"nel", "\u0085 NEL"},
	{"many blank lines", "a\n\n\n\nb"},
	{"blank lines with spaces", "  \n  \nx"},
	{"no headings", "# heading"},
	{"no thematic break", "---"},
	{"no html blocks", "<div>html</div>"},
	{"no emphasis", "*emph* **strong** ~~strike~~ _under_"},

	// Backslash escapes.
	{"escape asterisks", "\\*not emphasis\\*"},
	{"escape every escapable", "\\!\\\"\\#\\$\\%\\&\\'\\(\\)\\*\\+\\,\\-\\.\\/\\:\\;\\<\\=\\>\\?\\@\\[\\\\\\]\\^\\_\\`\\{\\|\\}\\~"},
	{"escape non-escapables", "\\a \\1 \\ \\é \\\u00a0"},
	{"escape alone", "\\"},
	{"escape trailing", "a\\"},
	{"escape backslash pair", "\\\\*"},
	{"escaped bracket", "\\[not a link](x)"},
	{"escaped colon before url", "http\\://a.b"},

	// Code spans.
	{"code span", "`code`"},
	{"code span double", "``code with ` inside``"},
	{"code span triple", "```triple```"},
	{"code span unbalanced", "`unbalanced"},
	{"code span mismatched runs", "``a` b``"},
	{"code span stripped", "`` `foo` ``"},
	{"code span whitespace collapse", "`a  b\n c`"},
	{"code span four", "```` four ````"},
	{"code span longer inner", "`a```b`"},
	{"code span only space", "` `"},
	{"code span empty", "`` `` "},
	{"code span alternating", "a`b`c`d"},
	{"code span across lines", "`a\nb`"},
	{"code span with escape", "`\\*`"},
	{"code span with entity", "`&amp;`"},
	{"code span with url", "`http://a.b`"},

	// Fenced code.
	{"fence backtick", "```\ncode\n```"},
	{"fence tilde", "~~~\ncode\n~~~"},
	{"fence info", "```rust\nfn main() {}\n```"},
	{"fence info extra", "``` js  extra info \ncode\n```"},
	{"fence unterminated", "```\nunterminated"},
	{"fence longer opening", "````\n```\n````"},
	{"fence longer closing", "```\ncode\n````"},
	{"fence close trailing spaces", "```\ncode\n```  "},
	{"fence close indented", "```\ncode\n ```"},
	{"fence close four spaces", "```\ncode\n    ```\nstill"},
	{"fence indented", "  ```\n  code\n   more\n code\n  ```"},
	{"fence backtick in info", "``` `\ncode\n```"},
	{"fence tilde backtick in info", "~~~ `ok`\ncode\n~~~"},
	{"fence in list", "- ```\n  code\n  ```"},
	{"fence in quote", "> ```\n> code\n> ```"},
	{"fence swallows starts", "```\n> not a quote\n- not a list\n```"},
	{"fence crlf", "```\r\ncode\r\n```\r\n"},
	{"fence tab inside", "```\n\tcode\n```"},
	{"fence info unescape", "```a&amp;b\\*\ncode\n```"},
	{"fence empty then text", "```\n``` \nafter"},
	{"fence mismatched close", "```\ncode\n~~~\nmore\n```"},
	{"fence interrupts paragraph", "text\n```\ncode\n```"},
	{"fence close wrong char", "~~~\ncode\n```\n~~~"},
	{"fence blank lines kept", "```\n\ncode\n\n```"},
	{"fence in loose list", "- a\n\n  ```\n  code\n  ```\n- b"},
	{"fence in quote lazy close", "> ```\n> code\n```"},
	{"fence info escaped html", "``` <b>&\ncode\n```"},
	{"fence code html escaped", "```\n<a href=\"x\">&</a>\n```"},
	{"fence only", "```"},
	{"fence tilde only", "~~~"},
	{"fence two chars", "``\ncode\n``"},

	// Indented code.
	{"indented code", "    code"},
	{"indented code blank inside", "    code\n    more\n\n    after blank"},
	{"indented code trailing blanks", "    code\n\n\n"},
	{"indented code tab", "\tcode"},
	{"indented code lazy paragraph", "para\n    not code (lazy)"},
	{"indented code then text", "    code\nlazy?"},
	{"indented code five spaces", "     five spaces"},
	{"indented code mixed", "  \t code"},
	{"indented code in list", "- a\n\n      code in list"},
	{"indented code in quote", ">     code in quote"},
	{"indented code quote tab", ">\tcode\n>\tmore"},
	{"indented code html escaped", "    <b>&\"</b>"},
	{"indented code blank with spaces", "    code\n      \n    more"},
	{"indented code blank vt", "    code\n\x0b\n    more"},
	{"indented code after list blank", "- a\n\n    code?"},

	// Block quotes.
	{"quote", "> quote"},
	{"quote two lines", "> a\n> b"},
	{"quote lazy", "> a\nlazy"},
	{"quote blank splits", "> a\n\n> b"},
	{"quote nested", "> > nested"},
	{"quote nested no space", ">> nested no space"},
	{"quote empty line", "> a\n>\n> b"},
	{"quote bare", ">"},
	{"quote list", "> - list in quote\n> - two"},
	{"quote indented four", "    > not a quote"},
	{"quote list lazy", "> a\n> - b\nlazy"},
	{"quote four spaces after", ">    four spaces"},
	{"quote five spaces after", ">     five spaces"},
	{"quote increasing indent", "> a\n>  b\n>   c\n>    d"},
	{"quote interrupts paragraph", "text\n> quote"},
	{"quote three spaces before", "   > ok"},
	{"quote no space", ">a"},
	{"quote tab after", ">\ta"},
	{"quote deep", ">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>> deep"},
	{"quote nested lazy", "> > a\n> b\nc"},
	{"quote with definition", "> [a]: /url\n\n[a]"},
	{"quote ref link", "[a]: /url\n\n> [a]"},
	{"quote then list", "> a\n- b"},
	{"quote paragraph continuation with marker", "> a\n>\n> b\n> > c"},

	// Lists.
	{"bullets dash", "- a\n- b\n- c"},
	{"bullets star", "* a\n* b"},
	{"bullets plus", "+ a\n+ b"},
	{"bullets change", "- a\n* b"},
	{"ordered dot", "1. a\n2. b"},
	{"ordered paren", "1) a\n2) b"},
	{"ordered start three", "3. start three"},
	{"ordered zero", "0. zero"},
	{"ordered ten digits", "1234567890. ten digits"},
	{"ordered nine digits", "123456789. nine digits"},
	{"ordered delimiter change", "1. a\n2) b"},
	{"loose blank between", "- a\n\n- b"},
	{"item two lines", "- a\n  b"},
	{"nested", "- a\n  - nested\n  - nested2\n- b"},
	{"nested four spaces", "- a\n    - four space nested"},
	{"not a list no space", "-a"},
	{"empty item", "-"},
	{"empty item then content", "- \n  content"},
	{"empty item newline content", "-\n  content"},
	{"list interrupts paragraph", "para\n- item"},
	{"ordered two does not interrupt", "para\n2. item"},
	{"ordered one interrupts", "para\n1. item"},
	{"empty item does not interrupt", "para\n-\n"},
	{"item two paragraphs", "- a\n\n  b"},
	{"item two paragraphs then item", "- a\n\n  b\n- c"},
	{"blank between items", "- a\n- b\n\n- c"},
	{"item five spaces code", "-     five spaces"},
	{"ordered two spaces", "1.  two spaces"},
	{"item one space lazy", "- a\n b"},
	{"paragraph after list", "- a\n\nb"},
	{"quote in item", "- a\n  > quote in item"},
	{"nested immediately", "- - nested immediately"},
	{"ordered then bullet nested", "1. - mixed"},
	{"list two space indent", "  - two-space indented list"},
	{"list three space indent", "   - three"},
	{"list four space indent", "    - four (code)"},
	{"multiple blanks between items", "- a\n\n\n- b"},
	{"nested loose", "- a\n  - b\n\n  - c"},
	{"item then no-space marker", "- a\n-b"},
	{"star star star", "* * *"},
	{"ordered all ones", "1. a\n1. b\n1. c"},
	{"task syntax", "- [x] task"},
	{"item code six spaces", "- a\n\n      code"},
	{"ordered overflow digits", "9999999999. overflow"},
	{"four bullet kinds", "- a\n- b\n* c\n+ d"},
	{"tab bullet", "\t- tab bullet"},
	{"tab after marker", "-\t tab after marker"},
	{"quote nested lists", "> - a\n>   - b\n> - c"},
	{"ordered nested deep", "1. a\n   1. b\n      1. c"},
	{"list deep", strings.Repeat("- ", 40) + "deep"},
	{"list after list different indent", "- a\n - b\n  - c\n   - d\n    - e"},
	{"item blank then lazy", "- a\n\nb\n- c"},
	{"list then quote", "- a\n> b"},
	{"ordered large start", "999999999. big"},
	{"item with trailing blank and nested", "- a\n\n  - b\n- c"},
	{"item marker only then blank then item", "-\n\n- b"},
	{"list item with hard break", "- a  \n  b"},
	{"list item lazy across blank", "- a\n\n b"},
	{"item indented content", "- a\n\n   b"},

	// Inline links and images.
	{"link", "[a](b)"},
	{"link title double", "[a](b \"title\")"},
	{"link title single", "[a](b 'title')"},
	{"link title paren", "[a](b (title))"},
	{"link angle dest", "[a](<b c>)"},
	{"link angle dest title", "[a](<b c> \"t\")"},
	{"link nested parens", "[a](b(c)d)"},
	{"link unbalanced paren", "[a](b(c)"},
	{"link empty", "[a]()"},
	{"link whitespace only", "[a](  )"},
	{"link title extra", "[a](b \"t\" extra)"},
	{"link title next line", "[a](b\n\"t\")"},
	{"link title multiline", "[a](b \"multi\nline\")"},
	{"link in link", "[[a](b)](c)"},
	{"link around link", "[a [b](c) d](e)"},
	{"image", "![a](b)"},
	{"image title", "![a](b \"t\")"},
	{"image dimensions", "![a](b =100x200)"},
	{"image dimensions title", "![a](b =100x200 \"t\")"},
	{"image height only", "![a](b =x200)"},
	{"image width only", "![a](b =100x)"},
	{"image dimensions empty", "![a](b =x)"},
	{"image dimensions junk", "![a](b =abc)"},
	{"image width no x", "![a](b =100)"},
	{"link dimensions", "[a](b =100x200)"},
	{"image in image", "![![a](b)](c)"},
	{"image in link", "[![a](b)](c)"},
	{"link in image", "![[a](b)](c)"},
	{"link escaped paren dest", "[a](b\\)c)"},
	{"link escaped bracket text", "[a\\]b](c)"},
	{"link escaped quote title", "[a](b \"t\\\"q\")"},
	{"link angle escaped", "[a](<b\\>c>)"},
	{"link entity dest", "[a](b&amp;c)"},
	{"link space dest", "[a](b c)"},
	{"link space before paren", "[a] (b)"},
	{"brackets empty", "[]"},
	{"bracket text", "[a]"},
	{"close bracket alone", "]"},
	{"open bracket alone", "["},
	{"image opener alone", "!["},
	{"bang alone", "!"},
	{"image label only", "![a]"},
	{"two links", "[a](b) [c](d)"},
	{"two links adjacent", "[a](b)[c](d)"},
	{"link control char dest", "[a](/url\x01)"},
	{"link utf8 dest", "[a](/ü)"},
	{"link percent dest", "[a](/a%20b%2)"},
	{"link javascript", "[a](javascript:alert(1))"},
	{"link title escaped html", "[a](b \"<>&\\\"\")"},
	{"link then definition", "[a](b)\n[c]: d"},
	{"link rich text", "[link *text* `code`](u)"},
	{"link dest special chars", "[a](;/?:@&=+$,-_.!~*'()#)"},
	{"link dest brackets", "[a](/x[y])"},
	{"link text entity", "[&amp;](b)"},
	{"link text emoji", "[:smile:](b)"},
	{"link text hard break", "[a  \nb](c)"},
	{"link dest newline", "[a](b\nc)"},
	{"link image alt nested", "![a ![b](c) [d](e) `f`](g)"},
	{"link title unterminated", "[a](b \"t)"},
	{"link title paren nested", "[a](b (t(x)))"},
	{"link dest angle unterminated", "[a](<b c)"},
	{"link dest angle newline", "[a](<b\nc>)"},
	{"link empty text", "[](b)"},
	{"image empty alt", "![](b)"},
	{"link bang not image", "! [a](b)"},
	{"link inactive after link", "[a [b](c)](d) [e](f)"},
	{"image keeps outer active", "[a ![b](c)](d)"},
	{"link dest only parens", "[a](())"},
	{"link dest closing paren first", "[a]())"},
	{"link long label", "[" + strings.Repeat("a", 1000) + "](b)"},
	{"link label 999", "[" + strings.Repeat("a", 999) + "](b)"},
	{"link nested brackets text", "[a [b] c](d)"},

	// Reference definitions and reference links.
	{"ref full", "[foo]: /url \"title\"\n\n[foo]"},
	{"ref collapsed", "[foo]: /url\n[foo][]"},
	{"ref explicit", "[foo]: /url\n[bar][foo]"},
	{"ref case fold ascii", "[FOO]: /url\n[foo]"},
	{"ref duplicate first wins", "[foo]: /url1\n[foo]: /url2\n[foo]"},
	{"ref duplicate separate paragraph", "[foo]: /url\n[foo]: /url2\n\n[foo]"},
	{"ref whitespace normalised", "[foo bar]: /url\n[foo   bar]"},
	{"ref label across lines", "[foo\nbar]: /url\n\n[foo bar]"},
	{"ref title next line", "[foo]: /url\n'title'\n\n[foo]"},
	{"ref broken title same line", "[foo]: /url 'title\n\n[foo]"},
	{"ref broken title next line", "[foo]: /url\n'bad title\n\n[foo]"},
	{"ref dest next line", "[foo]:\n/url\n\n[foo]"},
	{"ref angle dest", "[foo]: <bar baz>\n\n[foo]"},
	{"ref image shortcut", "[foo]: /url\n\n![foo]"},
	{"ref image explicit", "[foo]: /url\n\n![bar][foo]"},
	{"ref image collapsed title", "[foo]: /url \"t\"\n\n![foo][]"},
	{"ref definitions only", "[foo]: /url\n[foo]: /url\n"},
	{"ref definition then text", "[foo]: /url\nnext para line"},
	{"ref not at start", "text\n[foo]: /url\n\n[foo]"},
	{"ref trailing junk", "[foo]: /url junk\n\n[foo]"},
	{"ref title trailing junk", "[foo]: /url \"t\" junk\n\n[foo]"},
	{"ref no space after colon", "[foo]:/url\n\n[foo]"},
	{"ref space label", "[ ]: /url\n\n[ ]"},
	{"ref empty label", "[]: /url\n\n[]"},
	{"ref used twice", "[foo]: /url\n\n[foo] [foo]"},
	{"ref double brackets", "[foo]: /url\n\n[[foo]]"},
	{"ref undefined explicit", "[foo]: /url\n\n[foo][bar]"},
	{"ref explicit text undefined", "[foo]: /url\n\n[bar][foo]"},
	{"ref explicit space label", "[foo]: /url\n\n[foo][ ]"},
	{"ref fold sharp s", "[ẞ]: /url\n\n[ß]"},
	{"ref fold kelvin", "[K]: /url\n\n[k]"},
	{"ref fold long s", "[ſ]: /url\n\n[S]"},
	{"ref fold sigma", "[ΣΑΣ]: /url\n\n[σας]"},
	{"ref fold strasse", "[straße]: /url\n\n[STRASSE]"},
	{"ref fold dotted i", "[İ]: /url\n\n[i]"},
	{"ref in quote", "[a]: /url\n\n> [a]"},
	{"ref definitions in list", "- [a]: /url\n- [a]"},
	{"ref definition in quote", "> [a]: /url\n\n[a]"},
	{"ref paren title", "[foo]: /url (paren title)\n\n[foo]"},
	{"ref title trailing space", "[foo]: /url \"title\" \n\n[foo]"},
	{"ref empty angle dest", "[foo]: <>\n\n[foo]"},
	{"ref no destination", "[foo]: \n\n[foo]"},
	{"ref two definitions used", "[a]: b\n[c]: d\n\n[a] [c]"},
	{"ref inline beats reference", "[a]: /url\n\n[a](b)"},
	{"ref open paren after", "[a]: /url\n\n[a]("},
	{"ref explicit undefined", "[a]: /url\n\n[a][b]"},
	{"ref explicit defined", "[b]: /url\n\n[a][b]"},
	{"ref two lines", "[a]: /url\n\n[a]\n[b]"},
	{"ref title escaped", "[a]: /u\\*rl \"t\\\"x&amp;\"\n\n[a]"},
	{"ref label escaped bracket", "[a\\]b]: /url\n\n[a\\]b]"},
	{"ref definition consumes lines", "[a]: /url\n\"t\"\nrest\n\n[a]"},
	{"ref definition title then junk", "[a]: /url\n\"t\" x\n\n[a]"},
	{"ref shortcut in link text", "[a]: /url\n\n[[a]](b)"},
	{"ref inside image", "[a]: /url\n\n![x [a]](b)"},
	{"ref long label bytes not runes", "[" + strings.Repeat("é", 600) + "]: /url\n\n[" + strings.Repeat("é", 600) + "]"},
	{"ref long label runes", "[" + strings.Repeat("a", 1000) + "]: /url\n\n[" + strings.Repeat("a", 1000) + "]"},
	{"ref label 999 refused", "[" + strings.Repeat("a", 999) + "]: /url\n\n[" + strings.Repeat("a", 999) + "]"},
	{"ref label 998 accepted", "[" + strings.Repeat("a", 998) + "]: /url\n\n[" + strings.Repeat("a", 998) + "]"},
	{"ref case fold in definition lookup order", "[A]: /1\n[a]: /2\n\n[a]"},
	{"ref definition with nbsp", "[a]:\u00a0/url\n\n[a]"},
	{"ref dest with parens", "[a]: /u(r)l\n\n[a]"},
	{"ref colon only", "[a]:"},
	{"ref label then colon then text", "[a]: /u\n[b]"},

	// Autolinks — scheme://, www., and their trailing punctuation rules.
	{"angle autolink not supported", "<http://example.com>"},
	{"angle mailto", "<mailto:a@b.com>"},
	{"url", "http://example.com"},
	{"url query fragment", "https://example.com/path?q=1&r=2#frag"},
	{"url ftp", "ftp://x.y"},
	{"url mailto slashes", "mailto://x"},
	{"url tel", "tel://123"},
	{"url upper scheme", "HTTP://UPPER.COM"},
	{"url bad scheme", "xhttp://not.a.link"},
	{"url no host", "http://"},
	{"url single char host", "http://x"},
	{"url space host", "http:// space"},
	{"url trailing period", "http://example.com."},
	{"url trailing comma", "http://example.com,"},
	{"url trailing bang question", "http://example.com!?"},
	{"url trailing colon", "http://example.com:"},
	{"url trailing star", "http://example.com*"},
	{"url trailing underscore", "http://example.com_"},
	{"url trailing tilde", "http://example.com~"},
	{"url trailing quote", "http://example.com'"},
	{"url trailing dquote", "http://example.com\""},
	{"url trailing semicolon", "http://example.com;"},
	{"url trailing entity", "http://example.com&amp;"},
	{"url trailing entity no semicolon", "http://example.com&amp"},
	{"url trailing amp semicolon", "http://example.com&;"},
	{"url trailing short entity", "http://example.com&a;"},
	{"url in parens", "(http://example.com)"},
	{"url pokemon balanced", "http://www.pokemon.com/Pikachu_(Electric)"},
	{"url pokemon extra open", "http://www.pokemon.com/Pikachu_((Electric)"},
	{"url pokemon extra close", "http://www.pokemon.com/Pikachu_(Electric))"},
	{"url pokemon double", "http://www.pokemon.com/Pikachu_((Electric))"},
	{"url two extra close", "http://a.b/c)d)"},
	{"url angle inside", "http://a.b/<x>"},
	{"url gt inside", "http://a.b/x>y"},
	{"url underscore domain", "http://a_b.com"},
	{"url hyphen domain", "http://a-b.com"},
	{"url unicode domain", "http://ü.com"},
	{"url dot host", "http://.com"},
	{"url ideographic stop", "http://a.b/c\u3002"},
	{"url guillemets", "http://a.b/«x»"},
	{"url ellipsis", "http://a.b/x…"},
	{"www", "www.example.com"},
	{"www path", "www.example.com/path"},
	{"www1", "www1.example.com"},
	{"www123", "www123.example.com"},
	{"www1234", "www1234.example.com"},
	{"www no tld", "www.example"},
	{"www short", "www.x"},
	{"www dot only", "www."},
	{"www bare", "www"},
	{"www after letter", "awww.example.com"},
	{"www in stars", "*www.example.com*"},
	{"www in parens", "(www.example.com)"},
	{"www in underscores", "_www.example.com_"},
	{"www trailing period", "www.example.com."},
	{"www upper", "WWW.EXAMPLE.COM"},
	{"wwww", "wwww.example.com"},
	{"www port", "www.example.com:8080/x"},
	{"www in link text", "[www.example.com](x)"},
	{"url in link text", "[http://a.b](x)"},
	{"url as link dest", "[a](http://a.b)"},
	{"url in code span", "`http://a.b`"},
	{"url mid sentence", "a http://a.b b"},
	{"url two lines", "http://a.b\nhttp://c.d"},
	{"url two same line", "http://a.b http://c.d"},
	{"url brackets in path", "http://a.b/[x]"},
	{"url backslash in path", "http://a.b/x\\y"},
	{"url entity in path", "http://a.b/&lt;"},
	{"url backtick in path", "http://a.b/x`y"},
	{"email no autolink", "email@example.com"},
	{"www tab", "www.a.b\ttab"},
	{"url nbsp continues", "http://a.b/x\u00a0nbsp"},
	{"w alone", "w"},
	{"W alone", "W"},
	{"colon alone", ":"},
	{"double colon", "::"},
	{"w colon", "w:"},
	{"url one slash", "http:/x"},
	{"url no slashes", "http:x"},
	{"url no colon", "http//x"},
	{"url digit prefix scheme", "1http://a.b"},
	{"url letter prefix scheme", "ahttp://a.b"},
	{"url close paren", "http://a.b/x)"},
	{"url open paren", "http://a.b/(x"},
	{"url trailing amp", "http://a.b/x&"},
	{"url entity mid", "http://a.b/x&amp;y"},
	{"url trailing slash semicolon", "http://a.b/;"},
	{"url at start after bracket", "]http://a.b"},
	{"url after escaped char", "\\*http://a.b"},
	{"url after entity", "&amp;http://a.b"},
	{"url after emoji", ":smile:http://a.b"},
	{"url after w", "whttp://a.b"},
	{"url after code span", "`x`http://a.b"},
	{"url after close paren", ")http://a.b"},
	{"url inside unclosed bracket", "[http://a.b"},
	{"www inside unclosed bracket", "[www.a.b"},
	{"www after inactive delimiter", "[a](b)]www.a.b"},
	{"url scheme only colon slashes", "http://\n"},
	{"url unicode path", "http://例え.jp"},
	{"www unicode host", "www.例え.jp"},
	{"url only punctuation trimmed", "http://a.b/..."},
	{"url all trimmed", "http://..."},
	{"www all trimmed", "www.:"},
	{"www at position one", " www.a.b"},
	{"www at position one after letter", "awww.a.b x"},
	{"url replacement char host", "http://\ufffd.com"},
	{"url quote nbsp", "http://a.b/x\u00a0"},
	{"url angle first", "http://<a.b"},
	{"url paren depth", "http://a.b/(((x)))))"},
	{"www entity trailing", "www.a.b&copy;"},
	{"www semicolon letters trailing", "www.a.b&abc;;"},

	// HTML entities.
	{"entities named", "&amp; &lt; &gt; &quot; &copy; &AElig;"},
	{"entities numeric", "&#65; &#x41; &#X41; &#0; &#xD800; &#x110000; &#123456789; &#x123456789;"},
	{"entities malformed", "&#; &#x; &#xg; &#12a;"},
	{"entities unknown", "&nosuchentity; &amp &;"},
	{"entity mid word", "a&amp;b"},
	{"entities multi codepoint", "&zwj;x &ThickSpace; &fjlig; &nvlt;"},
	{"entity eight digits", "&#99999999; &#9999999; &#1114111; &#1114112;"},
	{"entity hex lengths", "&#x10FFFF; &#x0041; &#x00000041; &#x000000041;"},
	{"entity double", "&amp;amp;"},
	{"entity case sensitive", "&AMP; &Amp;"},
	{"entities adjacent", "&#65;&#66;"},
	{"entity embedded", "a&#65;b"},
	{"entity in code", "`&amp;`"},
	{"entity semicolon far", "&amp x; y"},
	{"entity surrogate", "&#xDFFF; &#55296;"},
	{"entity control", "&#1; &#31; &#127;"},
	{"entity newline inside", "&am\np;"},
	{"entity ffffffff", "&#xFFFFFFFF; &#x80000041;"},
	{"entity leading zeros decimal", "&#000000065; &#00000065;"},

	// Emoji.
	{"emoji", ":smile:"},
	{"emoji several", ":smile: :+1: :-1: :a_b:"},
	{"emoji after word", "a:smile:"},
	{"emoji before word", ":smile:a"},
	{"emoji adjacent", ":smile::wave:"},
	{"emoji upper", ":Smile:"},
	{"emoji unterminated", ":smile"},
	{"emoji no opener", "smile:"},
	{"emoji empty", "::"},
	{"emoji triple colon", ":::"},
	{"emoji space", ": :"},
	{"emoji then period", ":smile:."},
	{"emoji after space", "x :smile:"},
	{"emoji before multibyte", ":smile:é"},
	{"emoji in parens", "(:smile:)"},
	{"emoji colon inside", ":a:b:"},
	{"emoji escaped colon", "\\:smile:"},
	{"emoji in code", "`:smile:`"},
	{"emoji then newline", ":smile:\n"},
	{"emoji double colons", "::smile::"},
	{"emoji digits", ":1:"},
	{"emoji dash", ":-:"},
	{"emoji after digit", "1:smile:"},
	{"emoji after underscore", "_:smile:"},
	{"emoji at position one", "a:smile:"},
	{"emoji at position one nonword", " :smile:"},
	{"emoji at position one word", "x:smile: y"},
	{"emoji before underscore", ":smile:_"},
	{"emoji html escape", ":a<b:"},
	{"emoji in link text", "[:smile:](u)"},
	{"emoji after url", "http://a.b :smile:"},
	{"emoji plus", ":+1:x"},

	// Mentions, in every context the engine sees them.
	{"mention", "@user"},
	{"mention after text", "hello @user"},
	{"mention before text", "@user hello"},
	{"mention period", "@user."},
	{"mention colon", "@user:"},
	{"mention dash", "@user-"},
	{"mention list", "@user, @other"},
	{"mention in code span", "`@user`"},
	{"mention in fence", "```\n@user\n```"},
	{"mention in indented code", "    @user"},
	{"mention in link text", "[@user](http://x)"},
	{"mention as link dest", "[text](@user)"},
	{"mention in list item", "- @user"},
	{"mention in quote", "> @user"},
	{"mention two lines", "@user\n@other"},
	{"mention double at", "@@user"},
	{"mention email", "email@example.com"},
	{"mention possessive", "@user's"},
	{"mention emoji", "@user:smile:"},
	{"mention specials", "@here @channel @all"},
	{"mention stars", "**@user**"},
	{"mention escaped underscore", "@user\\_name"},
	{"mention underscore", "@user_name"},
	{"mention dot", "@user.name"},
	{"mention entity", "@user&amp;"},
	{"mention parens", "(@user)"},
	{"mention crlf", "@user\r\n"},
	{"mention bang", "hi @user!"},
	{"mention in ref link", "[@user]: http://x\n\n[@user]"},
	{"mention in image alt", "![@user](x)"},
	{"mention after url", "http://a.b @user"},
	{"mention hard break", "@user  \n@other"},
	{"mention in nested quote list", "> - @user\n>   - @other"},
	{"mention w prefix", "@wuser @William"},
	{"mention www", "@www.user"},
	{"mention with colon url", "@user: http://a.b"},

	// Multibyte text.
	{"japanese", "日本語のテキスト"},
	{"emoji codepoint", "emoji 😀 text"},
	{"combining mark", "e\u0301 combining"},
	{"cjk two lines", "日本語\n中文"},
	{"emoji around code", "😀`code`😀"},
	{"cjk link", "[日本](http://例え.jp/パス)"},
	{"cjk quote", "> 引用"},
	{"cjk list", "- 項目"},
	{"cjk hard break", "日本語  \n中文"},
	{"cjk escaped", "\\日"},
	{"cjk fence info", "```日本\ncode\n```"},
	{"cjk entity between", "日&amp;本"},
	{"cjk url trailing", "http://a.b/日本。"},
	{"combining after bracket", "[\u0301](x)"},
	{"emoji in code fence", "```\n😀\n```"},
	{"zero width space", "a\u200bb"},
	{"bom", "\ufeffhello"},
	{"cjk ref label", "[日本]: /url\n\n[日本]"},
	{"cjk emoji name", ":日本:"},

	// Misc structure.
	{"mixed document", "# Title\n\nSome *text* with `code` and [a link](http://x.y \"t\").\n\n- one\n- two\n  - three\n\n> quote\n> more\n\n```go\nfmt.Println()\n```\n\n    indented\n\n[ref]: /url\n\nUse [ref] and www.example.com and :smile: and @user."},
	{"only definitions in list", "- [a]: /b"},
	{"paragraph with all inline kinds", "text `code` [l](u) ![i](u) [r] http://a.b www.a.b :smile: &amp; \\* a  \nb\nc"},
	{"quote list code", "> - ```\n>   x\n>   ```"},
	{"list item fence unterminated", "- ```\n  code\n- b"},
	{"fence then list", "```\n```\n- a"},
	{"quote fence lazy", "> ```\ncode\n```"},
	{"item paragraph then fence lazy", "- a\n```\nb\n```"},
	{"nested list blank then code", "- a\n  - b\n\n        code"},
	{"crlf list", "- a\r\n- b\r\n\r\n- c"},
	{"cr quote", "> a\r> b"},
	{"tab indented list", "-\ta\n\t- b"},
	{"paragraph with cr softbreaks", "a\rb\r\nc\nd"},
	{"definition after code", "    code\n[a]: /url\n\n[a]"},
	{"definition lazy in quote", "> a\n[b]: /url\n\n[b]"},
}

func writeMarkdownBehaviourFixture(outDir, rustOutDir string) error {
	fixture := markdownFixture{
		MaxLenDefault: markdown.MaxLen(),
		Cases:         []markdownCase{},
		Entities:      map[string]string{},
		EqualFold:     []equalFoldCase{},
		RuneProbes:    []runeProbe{},
	}
	if markdown.MaxLen() != 4*2*16*1024 {
		return fmt.Errorf("MaxLen is %d, expected the package default; something called SetMaxPostRunes", markdown.MaxLen())
	}

	for _, c := range markdownCorpus {
		fixture.Cases = append(fixture.Cases, markdownRun(c.name, c.in, 0))
	}
	// One byte over MaxLen: Inspect sees nothing and Parse returns an empty Document. Exactly
	// MaxLen: parsed like any other input.
	fixture.Cases = append(fixture.Cases, markdownRun("over max len", "a", markdown.MaxLen()+1))
	fixture.Cases = append(fixture.Cases, markdownRun("exactly max len", "a", markdown.MaxLen()))
	// Over MaxLen with structure that would otherwise produce events.
	fixture.Cases = append(fixture.Cases, markdownRun("over max len mentions", "@user ", markdown.MaxLen()/6+1))

	// The entity table: the map is unexported, so its keys come from the source text and its
	// values from the exported decoder. 2,125 names in html_entities.go at the pinned SHA.
	src, err := os.ReadFile(filepath.Join("..", "mattermost", "server", "public", "shared", "markdown", "html_entities.go"))
	if err != nil {
		return fmt.Errorf("html_entities.go: %w", err)
	}
	keyRe := regexp.MustCompile(`(?m)^\s*"([^"]+)":\s*"`)
	for _, m := range keyRe.FindAllStringSubmatch(string(src), -1) {
		name := m[1]
		value := markdown.CharacterReference(name)
		if value == "" {
			return fmt.Errorf("entity %q decodes to nothing", name)
		}
		fixture.Entities[name] = value
	}
	if len(fixture.Entities) < 2000 {
		return fmt.Errorf("only %d entities scraped from html_entities.go", len(fixture.Entities))
	}

	for _, p := range [][2]string{
		{"", ""}, {"a", ""}, {"", "a"}, {"abc", "ABC"}, {"abc", "abd"}, {"a", "ab"}, {"ab", "a"},
		{"ß", "ẞ"}, {"ẞ", "ß"}, {"ß", "SS"}, {"k", "K"}, {"K", "k"}, {"K", "K"}, {"s", "ſ"}, {"S", "ſ"},
		{"σ", "ς"}, {"Σ", "ς"}, {"ς", "σ"}, {"ǅ", "ǆ"}, {"ǅ", "Ǆ"}, {"ǆ", "Ǆ"}, {"i", "İ"}, {"ı", "I"},
		{"ﬀ", "FF"}, {"ᲀ", "в"}, {"ᲀ", "В"}, {"ϴ", "θ"}, {"ϑ", "θ"}, {"ϑ", "Θ"}, {"µ", "μ"}, {"µ", "Μ"},
		{"Å", "å"}, {"Å", "Å"}, {"å", "Å"}, {"é", "É"}, {"é", "e"}, {"foo bar", "FOO BAR"}, {"日本", "日本"},
		{"日本", "日木"}, {"aé", "AÉ"}, {"éa", "ÉA"}, {"aß", "Aß"}, {"a\u00a0", "A "}, {"ǰ", "J̌"},
		{"ǈ", "ǉ"}, {"ǈ", "Ǉ"}, {"ῳ", "ῼ"}, {"ῳ", "ΩΙ"}, {"ω", "Ω"}, {"ω", "Ω"}, {"ⅰ", "Ⅰ"}, {"ⓐ", "Ⓐ"},
		{"ａ", "Ａ"}, {"𐐨", "𐐀"}, {"ꭰ", "Ꭰ"}, {"a", "à"}, {"ǳ", "Ǳ"}, {"ǳ", "ǲ"}, {"ᾀ", "ᾈ"}, {"ﬃ", "ﬃ"},
		{"θ", "ϴ"}, {"Θ", "ϴ"}, {"ǰ", "ǰ"}, {"abcß", "ABCẞ"}, {"ABCK", "abck"},
	} {
		fixture.EqualFold = append(fixture.EqualFold, equalFoldCase{A: p[0], B: p[1], Eq: strings.EqualFold(p[0], p[1])})
	}

	for _, r := range []rune{
		'!', '.', '。', '«', '»', '‽', '_', '-', '$', '+', '^', '`', '|', '~', '¿', '¡', '·', '•', '…',
		'—', '‘', '\u2028', '\u2029', '\u00a0', '\u0085', '\u3000', '\u1680', '\u200b', 'a', '1',
		' ', '\t', '\n', '\v', '\f', '\r', '\u180e', '\ufffd', '😀', '\U0010FFFF', '⁉', '〜', '＿',
		'，', 'ー', '؟', '।', '\u00ad', '§', '©', '®', '°', '±', '¶', '×', '÷', '@', '#', '%', '&',
		'*', '(', ')', ',', '/', ':', ';', '<', '=', '>', '?', '[', '\\', ']', '{', '}', '"', '\'',
		'\u2010', '\u2011', '\u2012', '\u2013', '\u2014', '\u2015', '\u2016', '\u2017', '\u201a',
		'\u2020', '\u2027', '\u2030', '\u203b', '\u2043', '\u2044', '\u2045', '\u2046', '\u2047',
		'\u2053', '\u2054', '\u2055', '\u205e', '\u207d', '\u207e', '\u208d', '\u208e', '\u2308',
		'\u2309', '\u230a', '\u230b', '\u2329', '\u232a', '\u2768', '\u27c5', '\u27c6', '\u27e6',
		'\u2983', '\u2999', '\u29fc', '\u29fd', '\u2cf9', '\u2cfa', '\u2d70', '\u2e00', '\u2e2e',
		'\u2e30', '\u2e52', '\u2e53', '\u2e5d', '\u3001', '\u3003', '\u3008', '\u3030', '\u303d',
		'\u30a0', '\u30fb', '\ua4fe', '\ua60d', '\ua673', '\ua67e', '\ua6f2', '\ua874', '\ua8ce',
		'\ua8f8', '\ua92e', '\ua95f', '\ua9c1', '\uaa5c', '\uaade', '\uaaf0', '\uabeb', '\ufd3e',
		'\ufe10', '\ufe30', '\ufe45', '\ufe49', '\ufe50', '\ufe54', '\ufe63', '\ufe68', '\uff01',
		'\uff05', '\uff0a', '\uff0f', '\uff1a', '\uff1f', '\uff3b', '\uff3f', '\uff5b', '\uff5f',
		'\U00010100', '\U0001039f', '\U000103d0', '\U0001056f', '\U00010857', '\U0001091f',
		'\U0001093f', '\U00010a50', '\U00010a7f', '\U00010af0', '\U00010b39', '\U00010b99',
		'\U00010ead', '\U00010f55', '\U00010f86', '\U00011047', '\U000110bb', '\U000110be',
		'\U00011140', '\U00011174', '\U000111c5', '\U000111cd', '\U000111db', '\U000111dd',
		'\U00011238', '\U000112a9', '\U0001144b', '\U0001145a', '\U0001145d', '\U000114c6',
		'\U000115c1', '\U00011641', '\U00011660', '\U0001173c', '\U0001183b', '\U00011944',
		'\U000119e2', '\U00011a3f', '\U00011a9a', '\U00011a9e', '\U00011b00', '\U00011c41',
		'\U00011c70', '\U00011ef7', '\U00011f43', '\U00011fff', '\U00012470', '\U00012ff1',
		'\U00016a6e', '\U00016af5', '\U00016b37', '\U00016b44', '\U00016e97', '\U00016fe2',
		'\U0001bc9f', '\U0001da87', '\U0001e95e', '\U0001e95f', '\U0001f600', '\U0001f300',
		'\U000e0000', '\U0000e000', '\u2400', '\u2100', '\u0f04', '\u0f3a', '\u0f3b', '\u0e4f',
		'\u061b', '\u061f', '\u060c', '\u066a', '\u06d4', '\u0700', '\u07f7', '\u0830', '\u085e',
		'\u0964', '\u0965', '\u0970', '\u09fd', '\u0a76', '\u0af0', '\u0c77', '\u0c84', '\u0df4',
		'\u0e5a', '\u0e5b', '\u10fb', '\u1360', '\u1368', '\u1400', '\u166e', '\u169b', '\u169c',
		'\u16eb', '\u1735', '\u17d4', '\u17d8', '\u17da', '\u1800', '\u180a', '\u1944', '\u1945',
		'\u1a1e', '\u1aa0', '\u1aa8', '\u1b5a', '\u1bfc', '\u1c3b', '\u1c7e', '\u1cc0', '\u1cd3',
		'\u0387', '\u037e', '\u055a', '\u058a', '\u05be', '\u05c0', '\u05c3', '\u05c6', '\u05f3',
		'\u0609', '\u061d', '\u061e', '\u0abd', '\u0964', '\u2000', '\u2001', '\u200a', '\u202f',
		'\u205f', '\u1c80', '\u212a', '\u212b', '\u017f', '\u03c2', '\u1e9e', '\u00df',
	} {
		fixture.RuneProbes = append(fixture.RuneProbes, runeProbe{Rune: string(r), IsPunct: unicode.IsPunct(r), IsSpace: unicode.IsSpace(r)})
	}

	data, err := json.MarshalIndent(fixture, "", "  ")
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(outDir, "behaviour_markdown.json"), append(data, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d cases, %d entities)\n", filepath.Join(outDir, "behaviour_markdown.json"), len(fixture.Cases), len(fixture.Entities))

	return writeMarkdownUnicodeGenerated(rustOutDir)
}

// writeMarkdownUnicodeGenerated emits the three Go unicode tables `autolink.go` and
// `strings.EqualFold` depend on, as go_unicode_gen.go does for `IsPrint` ([D-070]).
func writeMarkdownUnicodeGenerated(rustOutDir string) error {
	punct := scanRuneRanges(unicode.IsPunct)
	space := scanRuneRanges(unicode.IsSpace)
	type foldPair struct{ from, to rune }
	var folds []foldPair
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if f := unicode.SimpleFold(r); f != r {
			folds = append(folds, foldPair{r, f})
		}
	}
	if len(punct) == 0 || len(space) == 0 || len(folds) == 0 {
		return fmt.Errorf("a unicode scan came back empty")
	}

	var b strings.Builder
	b.WriteString("//! @generated by `reference/dump/behaviour_markdown.go` from Go's `unicode` package.\n")
	b.WriteString("//! DO NOT EDIT — re-run `cd reference/dump && go run .` instead.\n")
	b.WriteString("//!\n")
	b.WriteString("//! The three Unicode tables `shared/markdown` reaches into the Go standard library for:\n")
	b.WriteString("//! `unicode.IsPunct` and `unicode.IsSpace` (`isValidHostCharacter`, autolink.go:161) and\n")
	b.WriteString("//! `unicode.SimpleFold` (`strings.EqualFold`, the reference-label comparison at\n")
	b.WriteString("//! inlines.go:334). Rust's `char` API has no general-category query and no simple case\n")
	b.WriteString("//! folding, and its `is_whitespace` tracks a different Unicode version than the Go toolchain,\n")
	b.WriteString("//! so the tables are emitted rather than approximated — the same decision as [D-070].\n")
	b.WriteString("//!\n")
	b.WriteString("//! Unicode version: the **Go toolchain's**. Sorted and non-overlapping, so binary search applies.\n")
	emitRanges(&b, "IS_PUNCT_RANGES", "unicode.IsPunct (general category `P`)", punct)
	emitRanges(&b, "IS_SPACE_RANGES", "unicode.IsSpace", space)
	fmt.Fprintf(&b, "\n/// `(r, unicode.SimpleFold(r))` for the %d code points whose fold is not themselves,\n", len(folds))
	b.WriteString("/// sorted by `r`. Following the second column from any member walks its whole orbit and returns.\n")
	b.WriteString("#[rustfmt::skip]\npub static SIMPLE_FOLD: &[(u32, u32)] = &[\n")
	for i := 0; i < len(folds); i += 4 {
		end := min(i+4, len(folds))
		b.WriteString("   ")
		for _, f := range folds[i:end] {
			fmt.Fprintf(&b, " (0x%X, 0x%X),", f.from, f.to)
		}
		b.WriteString("\n")
	}
	b.WriteString("];\n")

	rustPath := filepath.Join(rustOutDir, "go_unicode_generated.rs")
	if err := os.WriteFile(rustPath, []byte(b.String()), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d punct, %d space ranges, %d fold pairs)\n", rustPath, len(punct), len(space), len(folds))
	return nil
}
