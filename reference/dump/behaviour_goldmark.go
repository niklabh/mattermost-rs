package main

// Behavioural oracle for **github.com/yuin/goldmark v1.8.2** and Mattermost's wrapper over it,
// `channels/utils/markdown.go` — written to fixtures/behaviour_goldmark.json, and the generator
// for crates/gogoldmark/src/tables_generated.rs. Run with `go run . -only goldmark`.
//
// Mattermost reaches goldmark twice: `utils.StripMarkdown` / `StripMarkdownAndDecode` (the text
// of every push notification: goldmark + Strikethrough, rendered by the custom
// `notificationRenderer`) and `utils.MarkdownToHTML` (the HTML of every notification e-mail:
// two regex pre-passes, then goldmark + GFM). The Rust ports are `crates/gogoldmark` and
// `crates/mm-app/src/markdown_utils.rs`; both assert every row here byte for byte.
//
// For every input the fixture records goldmark's HTML under six configurations (no extension,
// each of the four GFM extensions alone, and GFM), the AST under GFM as a pre-order list (node kind,
// lines, text flags, link fields — what a custom node renderer can read), and the three
// Mattermost functions with their errors. `prepass` is a verbatim copy of MarkdownToHTML's two
// regex passes (the regexes are unexported), so a divergence can be pinned to the pre-pass or to
// goldmark.
//
// Corpus: the CommonMark spec examples goldmark ships (`_test/spec.json`), the extension and
// extra cases it ships (`extension/_test/*.txt`, `_test/extra.txt`, `_test/options.txt`, read
// as plain inputs), a hand-picked chat-shaped corpus, and a deterministic token fuzz
// (math/rand/v2 PCG with a fixed seed — the inputs themselves are stored, so the Rust side never
// regenerates them).
//
// The generated tables are goldmark's own data, not the Go toolchain's html package: the HTML5
// entity table is read out of `util/html5entities.gen.go` in the module cache and every entry is
// cross-checked against `util.LookUpHTML5EntityByName`; the case-folding table is recovered by
// probing `util.DoFullUnicodeCaseFolding` on every scalar value; and the punctuation/space
// predicates (`util.IsPunctRune`, `util.IsSpaceRune`) are scanned from the Go toolchain's
// `unicode` tables, whose Unicode version is the toolchain's.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"html"
	"math/rand/v2"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/mattermost/mattermost/server/v8/channels/utils"
	"github.com/yuin/goldmark"
	gast "github.com/yuin/goldmark/ast"
	"github.com/yuin/goldmark/extension"
	east "github.com/yuin/goldmark/extension/ast"
	"github.com/yuin/goldmark/text"
	gutil "github.com/yuin/goldmark/util"
)

// goldmarkCase is one input and everything recorded for it. To keep the file reviewable, a
// field that would repeat another is omitted, and the Rust side substitutes it back:
// `strikethrough`/`table`/`linkify`/`tasklist` absent means equal to `html`, `md_to_html` absent
// means equal to `gfm`, `prepass` absent means equal to `input`, `strip_decode` absent means
// equal to `strip`, and an absent `*_err` is no error. `ast` is absent for the two largest
// fuzz corpora.
type goldmarkCase struct {
	ID             string           `json:"id"`
	Input          string           `json:"input"`
	HTML           string           `json:"html"`
	Strikethrough  *string          `json:"strikethrough,omitempty"`
	Table          *string          `json:"table,omitempty"`
	Linkify        *string          `json:"linkify,omitempty"`
	TaskList       *string          `json:"tasklist,omitempty"`
	GFM            string           `json:"gfm"`
	AST            []map[string]any `json:"ast,omitempty"`
	Strip          string           `json:"strip"`
	StripErr       string           `json:"strip_err,omitempty"`
	StripDecode    *string          `json:"strip_decode,omitempty"`
	StripDecodeErr string           `json:"strip_decode_err,omitempty"`
	Prepass        *string          `json:"prepass,omitempty"`
	MdToHTML       *string          `json:"md_to_html,omitempty"`
	MdToHTMLErr    string           `json:"md_to_html_err,omitempty"`
}

// unlessEqual is nil when v repeats base.
func unlessEqual(v, base string) *string {
	if v == base {
		return nil
	}
	return &v
}

type goldmarkSiteCase struct {
	Input   string `json:"input"`
	SiteURL string `json:"site_url"`
	Prepass string `json:"prepass"`
	HTML    string `json:"html"`
	Err     string `json:"err"`
}

const goldmarkSiteURL = "https://site.example"

// Verbatim copies of markdown.go's unexported pre-pass regexes.
var gmRelLinkReg = regexp.MustCompile(`\[(.*)]\((/.*)\)`)
var gmBlockquoteReg = regexp.MustCompile(`^|\n(&gt;)`)

func goldmarkPrepass(markdown, siteURL string) string {
	absLinkMarkdown := gmRelLinkReg.ReplaceAllStringFunc(markdown, func(s string) string {
		return gmRelLinkReg.ReplaceAllString(s, "[$1]("+siteURL+"$2)")
	})
	return gmBlockquoteReg.ReplaceAllStringFunc(absLinkMarkdown, func(s string) string {
		return html.UnescapeString(s)
	})
}

func goldmarkConvert(input string, exts ...goldmark.Extender) string {
	md := goldmark.New(goldmark.WithExtensions(exts...))
	var b bytes.Buffer
	if err := md.Convert([]byte(input), &b); err != nil {
		return "ERROR: " + err.Error()
	}
	return b.String()
}

func gmErrString(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

// goldmarkASTNode appends n and its subtree in pre-order, each node carrying its depth: a flat
// list keeps deeply nested inputs within JSON decoders' recursion limits.
func goldmarkASTNode(out []map[string]any, n gast.Node, depth int, src []byte) []map[string]any {
	m := map[string]any{"kind": n.Kind().String(), "depth": depth}
	if n.Type() == gast.TypeBlock {
		lines := []string{}
		for i := 0; i < n.Lines().Len(); i++ {
			seg := n.Lines().At(i)
			lines = append(lines, string(seg.Value(src)))
		}
		m["lines"] = lines
		m["blank_previous_lines"] = n.HasBlankPreviousLines()
	}
	m["raw"] = n.IsRaw()
	switch v := n.(type) {
	case *gast.Heading:
		m["level"] = v.Level
	case *gast.List:
		m["marker"] = string(v.Marker)
		m["tight"] = v.IsTight
		m["start"] = v.Start
	case *gast.ListItem:
		m["offset"] = v.Offset
	case *gast.FencedCodeBlock:
		if v.Info != nil {
			m["info"] = string(v.Info.Segment.Value(src))
		}
		m["language"] = string(v.Language(src))
	case *gast.HTMLBlock:
		m["html_type"] = int(v.HTMLBlockType)
		if v.HasClosure() {
			m["closure"] = string(v.ClosureLine.Value(src))
		}
	case *gast.LinkReferenceDefinition:
		m["label"] = string(v.Label)
		m["destination"] = string(v.Destination)
		if v.Title != nil {
			m["title"] = string(v.Title)
		}
	case *gast.Text:
		m["value"] = string(v.Segment.Value(src))
		m["soft_line_break"] = v.SoftLineBreak()
		m["hard_line_break"] = v.HardLineBreak()
	case *gast.String:
		m["value"] = string(v.Value)
		m["code"] = v.IsCode()
	case *gast.Emphasis:
		m["level"] = v.Level
	case *gast.Link:
		m["destination"] = string(v.Destination)
		if v.Title != nil {
			m["title"] = string(v.Title)
		}
	case *gast.Image:
		m["destination"] = string(v.Destination)
		if v.Title != nil {
			m["title"] = string(v.Title)
		}
	case *gast.AutoLink:
		m["autolink_type"] = int(v.AutoLinkType)
		m["url"] = string(v.URL(src))
		m["label"] = string(v.Label(src))
	case *gast.RawHTML:
		segs := []string{}
		for i := 0; i < v.Segments.Len(); i++ {
			seg := v.Segments.At(i)
			segs = append(segs, string(seg.Value(src)))
		}
		m["segments"] = segs
	case *east.TableCell:
		m["alignment"] = v.Alignment.String()
	case *east.Table:
		al := []string{}
		for _, a := range v.Alignments {
			al = append(al, a.String())
		}
		m["alignments"] = al
	case *east.TaskCheckBox:
		m["checked"] = v.IsChecked
	}
	out = append(out, m)
	for c := n.FirstChild(); c != nil; c = c.NextSibling() {
		out = goldmarkASTNode(out, c, depth+1, src)
	}
	return out
}

func goldmarkAST(input string) []map[string]any {
	md := goldmark.New(goldmark.WithExtensions(extension.GFM))
	src := []byte(input)
	doc := md.Parser().Parse(text.NewReader(src))
	return goldmarkASTNode(nil, doc, 0, src)
}

func runGoldmarkCase(id, input string) goldmarkCase {
	c := goldmarkCase{ID: id, Input: input}
	c.HTML = goldmarkConvert(input)
	c.Strikethrough = unlessEqual(goldmarkConvert(input, extension.Strikethrough), c.HTML)
	c.Table = unlessEqual(goldmarkConvert(input, extension.Table), c.HTML)
	c.Linkify = unlessEqual(goldmarkConvert(input, extension.Linkify), c.HTML)
	c.TaskList = unlessEqual(goldmarkConvert(input, extension.TaskList), c.HTML)
	c.GFM = goldmarkConvert(input, extension.GFM)
	c.AST = goldmarkAST(input)
	var err error
	c.Strip, err = utils.StripMarkdown(input)
	c.StripErr = gmErrString(err)
	stripDecode, err := utils.StripMarkdownAndDecode(input)
	c.StripDecode = unlessEqual(stripDecode, c.Strip)
	c.StripDecodeErr = gmErrString(err)
	c.Prepass = unlessEqual(goldmarkPrepass(input, goldmarkSiteURL), input)
	mdToHTML, err := utils.MarkdownToHTML(input, goldmarkSiteURL)
	c.MdToHTML = unlessEqual(mdToHTML, c.GFM)
	c.MdToHTMLErr = gmErrString(err)
	return c
}

// goldmarkTxtCases reads goldmark's `//- - -//`-separated case files as plain inputs.
func goldmarkTxtCases(path string) ([]string, error) {
	fp, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer fp.Close()
	const attrSep = "//- - - - - - - - -//"
	const caseSep = "//= = = = = = = = = = = = = = = = = = = = = = = =//"
	sc := bufio.NewScanner(fp)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	var out []string
	state := 0 // 0: header, 1: markdown, 2: expected
	var buf []string
	for sc.Scan() {
		line := sc.Text()
		switch state {
		case 0:
			if line == attrSep {
				state = 1
				buf = nil
			}
		case 1:
			if line == attrSep {
				out = append(out, strings.Join(buf, "\n"))
				state = 2
				continue
			}
			buf = append(buf, line)
		case 2:
			if line == caseSep {
				state = 0
			}
		}
	}
	return out, sc.Err()
}

func goldmarkChatCorpus() []string {
	return []string{
		"",
		" ",
		"\n",
		"hello",
		"  hello  ",
		"@channel please look",
		"hey @alice and @bob.smith, see ~town-square",
		"~off-topic is quiet",
		":smile: :+1: :white_check_mark:",
		"**bold** and *italic* and ***both***",
		"__bold__ and _italic_ and snake_case_word",
		"~~struck~~ and ~single~ and ~~~three~~~",
		"`code` and ``co`de`` and ` spaced `",
		"```\nfenced\n```",
		"```go\nfunc main() {}\n```\nafter",
		"~~~python extra words\nprint(1)\n~~~",
		"```\nunterminated",
		"    indented code\n    line two",
		"\tcode with tab",
		"# Heading 1\n## Heading 2\n###### Heading 6\n####### not",
		"Setext\n===\n\nSetext two\n---",
		"#hashtag not heading",
		"> quote\n> more\ncontinued",
		"&gt; escaped quote\n&gt; line two",
		"first\n&gt; second is a quote after pre-pass",
		"&gt;&gt; nested escaped",
		"a &gt; b &lt; c &amp; d &quot;e&quot; &#39;f&#39;",
		"&copy; &nbsp; &#169; &#xA9; &#0; &#x110000; &bogus;",
		"- one\n- two\n- three",
		"* a\n\n* b",
		"1. first\n2. second\n10. tenth",
		"3) start at three\n4) four",
		"- outer\n  - inner\n    - innermost\n- back",
		"- [ ] todo\n- [x] done\n- [X] also done\n- [y] not a task",
		"1. [ ] numbered task",
		"| a | b |\n|---|:-:|\n| 1 | 2 |",
		"| left | center | right |\n| :--- | :---: | ---: |\n| l | c | r |\n| only one |",
		"a | b\n-- | --\nc | d",
		"| `a\\|b` | c \\| d |\n|---|---|\n| e | f |",
		"| h1 | h2 |\n|----|\n| x | y |",
		"http://example.com",
		"https://example.com/path?q=1&r=2#frag",
		"see https://example.com.",
		"see https://example.com, then",
		"(https://example.com/foo_(bar))",
		"https://example.com/foo_(bar))",
		"www.example.com/page",
		"visit www.mattermost.com!",
		"ftp://files.example.org/x",
		"mail me at user@example.com.",
		"user@example",
		"a_b@c.d-",
		"http://localhost:8065/path",
		"https://EXAMPLE.COM",
		"https://example.com:8080/x",
		"https://example.com/a&amp;b",
		"https://example.com/q?a=1&b;",
		"<https://autolink.example>",
		"<user@example.com>",
		"[link](https://example.com \"title\")",
		"[rel](/relative/path)",
		"[rel](/a) and [rel2](/b)",
		"[two](/a) text [three](https://x) more",
		"![image](/img.png)",
		"![alt *text*](https://example.com/i.png 'T')",
		"[ref]\n\n[ref]: https://example.com/ref \"Ref\"",
		"[unterminated link(",
		"[a](<b c>)",
		"[a](javascript:alert(1))",
		"[a](data:image/png;base64,xx) [b](data:text/html,x)",
		"line one\nline two",
		"hard  \nbreak",
		"hard\\\nbreak",
		"para one\n\npara two",
		"<b>raw</b> inline",
		"<div>\nblock html\n</div>",
		"<script>alert(1)</script>",
		"<!-- comment -->",
		"<?php echo 1; ?>",
		"<![CDATA[x]]>",
		"<!DOCTYPE html>",
		"\\*not emphasis\\*",
		"\\\\ backslash",
		"---\n***\n___",
		"日本語のテキスト **太字** です",
		"中文 ~~删除~~ 文本",
		"emoji 🎉 and **🎉**",
		"Ünïcödé _ītalic_",
		"a b",
		"tab\there",
		"crlf\r\nline",
		"trailing spaces   ",
		strings.Repeat("word ", 400),
		strings.Repeat("a", 5000),
		strings.Repeat("> ", 60) + "deep",
		strings.Repeat("- ", 40) + "deep list",
		strings.Repeat("*", 200) + "x" + strings.Repeat("*", 200),
		strings.Repeat("[", 300) + "x" + strings.Repeat("]", 300),
		strings.Repeat("_a ", 300),
		strings.Repeat("`", 50) + "x",
		strings.Repeat("<", 100) + "a" + strings.Repeat(">", 100),
		"*a **b** c*",
		"**a *b* c**",
		"*foo`*`",
		"[*foo*](/url)",
		"[*foo](/url)",
		"*[foo*](/url)",
		"[a [b] c](/url)",
		"[a](/u1) [b](/u2) [c](/u3)",
		"text [x](/p) text\nnext [y](/q)",
		"[x](/p)\n[y](/q)",
		"[a](/p(q))",
		"[a]( /p )",
		"`[a](/p)`",
		"<a href=\"/x\">[a](/p)</a>",
		"> [quoted link](/q)",
		"&gt; [escaped quoted link](/q)",
		"\n&gt; leading newline quote",
		"&gt;\n&gt;",
		"x\n&gt;&amp;y",
		"x\n&gt\ny",
		"x\r\n&gt; crlf",
		"* item\n\n  continued paragraph\n* next",
		"- a\n-\n- c",
		"1.\n2. b",
		"- ```\n  code in list\n  ```",
		"> - quoted list\n> - two",
		"Setext with *emphasis*\n---",
		"| table | with **bold** |\n| --- | --- |\n| `code` | [link](/x) |",
		"~~**nested**~~ and **~~nested~~**",
		"@user: **urgent** see ~channel and https://x.io/a_b",
		"![](/empty-alt.png)",
		"[](/empty-text)",
		"[text]()",
		"<https://a.b/c d>",
		"`` ` ``",
		"a*\"foo\"*",
		"*$*alpha.\n\n*£*bravo.",
		"_foo_bar_",
		"foo-_(bar)_",
		"  - a\n - b\n- c",
		"a\n    b",
		"  # heading with leading space",
		"#\tTab heading",
		"### closed ###",
		"\\# escaped",
	}
}

var goldmarkFuzzTokens = []string{
	"*", "**", "_", "__", "~", "~~", "`", "```", "~~~", "[", "]", "(", ")", "![", "](", "<", ">",
	"#", "# ", "- ", "+ ", "* ", "1. ", "2) ", "> ", "\n", "\n\n", " ", "  \n", "\\\n", "\t",
	"    ", "|", "| --- |", ":-:", "---", "===", "\\", "&", "&amp;", "&gt;", "\n&gt; ", "&#35;",
	"&#x41;", "&nbsp;", "&copy", ";", "a", "foo", "bar", "é", "日本", "http://x.org",
	"https://a.b/c?d=e", "www.example.com", "x@y.org", "<div>", "</div>", "<a href='x'>",
	"<!-- c -->", "<?p?>", "<![CDATA[", "]]>", "\"", "'", "@user", "~town", ":smile:", "[ ] ",
	"[x] ", "[ref]: /url", "[ref]", "!", ".", ",", "'title'", "\"t\"", "/path", "(/rel)", "-",
	"=", "0", "9", "\r\n", "<b>", "</b>", "`x`", "\\|", "\\*",
}

func goldmarkFuzzCorpus(n int) []string {
	r := rand.New(rand.NewPCG(20260924, 1))
	out := make([]string, 0, n)
	for len(out) < n {
		k := 1 + r.IntN(16)
		var b strings.Builder
		for j := 0; j < k; j++ {
			b.WriteString(goldmarkFuzzTokens[r.IntN(len(goldmarkFuzzTokens))])
		}
		out = append(out, b.String())
	}
	return out
}

// goldmarkBlockPrefixes start the lines of goldmarkLineCorpus: container markers, indentation
// (spaces, tabs, mixed), fences, HTML block openers and closers, table rows, breaks, setext
// underlines and link reference definitions.
var goldmarkBlockPrefixes = []string{
	"", "", "", "> ", ">", "- ", "* ", "+ ", "1. ", "2) ", "10. ", "  ", "   ", "    ", "\t",
	" \t", "  - ", "> > ", "- > ", "> - ", "1. - ", "-\t", ">\t", "# ", "## ", "###### ",
	"####### ", "#", "```", "~~~", "```go", "    ```", "<div>", "<!--", "-->", "</div>",
	"<script>", "</script>", "<pre>", "<?x", "?>", "<!X", "<![CDATA[", "]]>", "<a href='x'>",
	"</b>", "| ", "|---|", "| :-: |", "--- | ---", "---", "***", "___", "===", "- - -", "[r]: ",
	"[r]:", "[x]: /u 'T'", "[y]: <a b>", "  [z]: /q\n\"t\"", "- [ ] ", "* [x] ",
}

// goldmarkLineCorpus is line-structured: each line is a block prefix and a few inline tokens,
// which is what exercises container continuation, laziness and list tightness.
func goldmarkLineCorpus(n int) []string {
	r := rand.New(rand.NewPCG(20260924, 2))
	out := make([]string, 0, n)
	for len(out) < n {
		lines := 1 + r.IntN(8)
		var b strings.Builder
		for l := 0; l < lines; l++ {
			if l > 0 {
				b.WriteString("\n")
			}
			b.WriteString(goldmarkBlockPrefixes[r.IntN(len(goldmarkBlockPrefixes))])
			if r.IntN(3) == 0 {
				b.WriteString(goldmarkBlockPrefixes[r.IntN(len(goldmarkBlockPrefixes))])
			}
			k := r.IntN(5)
			for j := 0; j < k; j++ {
				b.WriteString(goldmarkFuzzTokens[r.IntN(len(goldmarkFuzzTokens))])
			}
		}
		out = append(out, b.String())
	}
	return out
}

// goldmarkCharCorpus draws single characters from markdown's punctuation: the delimiter,
// bracket and escape rules decide almost everything here.
func goldmarkCharCorpus(n int) []string {
	const alphabet = "*_`[]()<>!\\ \n\t#-+>|~:&;.xa1\"'/=@w"
	r := rand.New(rand.NewPCG(20260924, 3))
	out := make([]string, 0, n)
	for len(out) < n {
		k := 1 + r.IntN(40)
		b := make([]byte, k)
		for j := range b {
			b[j] = alphabet[r.IntN(len(alphabet))]
		}
		out = append(out, string(b))
	}
	return out
}

var goldmarkLinkifyTokens = []string{
	"http://", "https://", "www.", "ftp://", "http:", "a", "b", ".", "com", "org", "/", "?", "#",
	"(", ")", "&amp;", "&", ";", "@", "_", "-", ":", "8080", "!", ",", "*", "~", " ", "Z", "\n",
	"<", ">", "'", "\"", "$", "=", "é", "x@y.z", "[", "]",
}

// goldmarkLinkifyCorpus aims at the linkify boundary rules: scheme and www prefixes, the
// `{1,256}` domain back-off, ports, trailing punctuation, parentheses, entities and e-mails.
func goldmarkLinkifyCorpus(n int) []string {
	r := rand.New(rand.NewPCG(20260924, 4))
	out := make([]string, 0, n)
	for len(out) < n {
		k := 1 + r.IntN(12)
		var b strings.Builder
		for j := 0; j < k; j++ {
			b.WriteString(goldmarkLinkifyTokens[r.IntN(len(goldmarkLinkifyTokens))])
		}
		out = append(out, b.String())
	}
	return out
}

// goldmarkMarshal is json.Marshal without HTML escaping (the corpus is full of `<`, `>`, `&`).
func goldmarkMarshal(v any) ([]byte, error) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimRight(buf.Bytes(), "\n"), nil
}

func goldmarkModuleDir() (string, error) {
	cmd := exec.Command("go", "list", "-m", "-f", "{{.Dir}}", "github.com/yuin/goldmark")
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("go list goldmark: %w", err)
	}
	return strings.TrimSpace(string(out)), nil
}

func writeGoldmarkBehaviourFixture(outDir, rustOutDir string) error {
	dir, err := goldmarkModuleDir()
	if err != nil {
		return err
	}
	if err := writeGoldmarkTables(dir, rustOutDir); err != nil {
		return err
	}

	var cases []goldmarkCase

	specBlob, err := os.ReadFile(filepath.Join(dir, "_test", "spec.json"))
	if err != nil {
		return err
	}
	var spec []struct {
		Markdown string `json:"markdown"`
		Example  int    `json:"example"`
	}
	if err := json.Unmarshal(specBlob, &spec); err != nil {
		return err
	}
	for _, s := range spec {
		cases = append(cases, runGoldmarkCase(fmt.Sprintf("spec-%d", s.Example), s.Markdown))
	}

	txts := []string{
		"extension/_test/linkify.txt", "extension/_test/strikethrough.txt",
		"extension/_test/table.txt", "extension/_test/tasklist.txt",
		"_test/extra.txt", "_test/options.txt",
	}
	for _, t := range txts {
		inputs, err := goldmarkTxtCases(filepath.Join(dir, t))
		if err != nil {
			return err
		}
		base := strings.TrimSuffix(filepath.Base(t), ".txt")
		for i, in := range inputs {
			cases = append(cases, runGoldmarkCase(fmt.Sprintf("%s-%d", base, i+1), in))
		}
	}
	for i, in := range goldmarkChatCorpus() {
		cases = append(cases, runGoldmarkCase(fmt.Sprintf("chat-%d", i+1), in))
	}
	for i, in := range goldmarkFuzzCorpus(3000) {
		cases = append(cases, runGoldmarkCase(fmt.Sprintf("fuzz-%d", i+1), in))
	}
	for i, in := range goldmarkLinkifyCorpus(2000) {
		cases = append(cases, runGoldmarkCase(fmt.Sprintf("linkfuzz-%d", i+1), in))
	}
	// The two larger corpora carry no AST: their HTML under six configurations is the check,
	// and the file stays a size a reviewer can open.
	for i, in := range goldmarkLineCorpus(3000) {
		c := runGoldmarkCase(fmt.Sprintf("linefuzz-%d", i+1), in)
		c.AST = nil
		cases = append(cases, c)
	}
	for i, in := range goldmarkCharCorpus(3000) {
		c := runGoldmarkCase(fmt.Sprintf("charfuzz-%d", i+1), in)
		c.AST = nil
		cases = append(cases, c)
	}

	siteInputs := []string{
		"[rel](/relative/path)", "[a](/x) [b](/y)", "[a](/x)\n[b](/y)", "no links",
		"[x](/p$1) [y](/q)", "&gt; q [z](/r)",
	}
	siteURLs := []string{
		goldmarkSiteURL, "", "https://x.example/sub", "https://x/$1", "https://x/$$", "http://a/${2}",
		"http://a/${1", "http://a/$0", "http://a/$01", "http://a/$9", "http://a/$name", "http://a/$",
		"http://a/$é1",
	}
	var siteCases []goldmarkSiteCase
	for _, in := range siteInputs {
		for _, su := range siteURLs {
			h, err := utils.MarkdownToHTML(in, su)
			siteCases = append(siteCases, goldmarkSiteCase{
				Input: in, SiteURL: su, Prepass: goldmarkPrepass(in, su), HTML: h, Err: gmErrString(err),
			})
		}
	}

	// One case per line, unindented: indenting the nested ASTs triples the file for no reader.
	var b bytes.Buffer
	b.WriteString("{\"goldmark_version\": \"v1.8.2\",\n\"site_url\": ")
	su, _ := json.Marshal(goldmarkSiteURL)
	b.Write(su)
	b.WriteString(",\n\"cases\": [\n")
	for i, c := range cases {
		blob, err := goldmarkMarshal(c)
		if err != nil {
			return err
		}
		b.Write(blob)
		if i != len(cases)-1 {
			b.WriteByte(',')
		}
		b.WriteByte('\n')
	}
	b.WriteString("],\n\"site_cases\": [\n")
	for i, c := range siteCases {
		blob, err := goldmarkMarshal(c)
		if err != nil {
			return err
		}
		b.Write(blob)
		if i != len(siteCases)-1 {
			b.WriteByte(',')
		}
		b.WriteByte('\n')
	}
	b.WriteString("]}\n")
	path := filepath.Join(outDir, "behaviour_goldmark.json")
	if err := os.WriteFile(path, b.Bytes(), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d cases, %d site cases)\n", path, len(cases), len(siteCases))
	return nil
}

// ---- generated Rust tables ----

func goldmarkEvalString(e ast.Expr) (string, error) {
	switch v := e.(type) {
	case *ast.BasicLit:
		return strconv.Unquote(v.Value)
	case *ast.BinaryExpr:
		l, err := goldmarkEvalString(v.X)
		if err != nil {
			return "", err
		}
		r, err := goldmarkEvalString(v.Y)
		if err != nil {
			return "", err
		}
		return l + r, nil
	}
	return "", fmt.Errorf("unsupported expr %T", e)
}

func goldmarkEvalInts(e ast.Expr) ([]int64, error) {
	cl, ok := e.(*ast.CompositeLit)
	if !ok {
		return nil, fmt.Errorf("not a composite literal: %T", e)
	}
	var out []int64
	for _, el := range cl.Elts {
		bl, ok := el.(*ast.BasicLit)
		if !ok {
			return nil, fmt.Errorf("unsupported element %T", el)
		}
		v, err := strconv.ParseInt(bl.Value, 0, 64)
		if err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, nil
}

func goldmarkReadEntities(dir string) ([][2]string, error) {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, filepath.Join(dir, "util", "html5entities.gen.go"), nil, 0)
	if err != nil {
		return nil, err
	}
	vals := map[string]ast.Expr{}
	for _, d := range f.Decls {
		gd, ok := d.(*ast.GenDecl)
		if !ok {
			continue
		}
		for _, s := range gd.Specs {
			vs, ok := s.(*ast.ValueSpec)
			if !ok {
				continue
			}
			for i, n := range vs.Names {
				if i < len(vs.Values) {
					vals[n.Name] = vs.Values[i]
				}
			}
		}
	}
	names, err := goldmarkEvalString(vals["_html5entitiesName"])
	if err != nil {
		return nil, err
	}
	charBytes, err := goldmarkEvalInts(vals["_html5entitiesCharacters"])
	if err != nil {
		return nil, err
	}
	cb := make([]byte, len(charBytes))
	for i, v := range charBytes {
		cb[i] = byte(v)
	}
	chars := string(cb)
	nameIdxS, err := goldmarkEvalString(vals["_html5entitiesNameIndex"])
	if err != nil {
		return nil, err
	}
	charIdxS, err := goldmarkEvalString(vals["_html5entitiesCharactersIndex"])
	if err != nil {
		return nil, err
	}
	nameIdx, charIdx := []byte(nameIdxS), []byte(charIdxS)
	if len(nameIdx) != len(charIdx) {
		return nil, fmt.Errorf("entity index length mismatch")
	}
	var out [][2]string
	cn, cc := 0, 0
	for i := range nameIdx {
		tn, tc := cn+int(nameIdx[i]), cc+int(charIdx[i])
		name, ch := names[cn:tn], chars[cc:tc]
		e, ok := gutil.LookUpHTML5EntityByName(name)
		if !ok || string(e.Characters) != ch {
			return nil, fmt.Errorf("entity %q does not round-trip through LookUpHTML5EntityByName", name)
		}
		out = append(out, [2]string{name, ch})
		cn, cc = tn, tc
	}
	sort.Slice(out, func(i, j int) bool { return out[i][0] < out[j][0] })
	return out, nil
}

func rustByteString(s string) string {
	var b strings.Builder
	b.WriteString("b\"")
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c == '"':
			b.WriteString("\\\"")
		case c == '\\':
			b.WriteString("\\\\")
		case c >= 0x20 && c < 0x7f:
			b.WriteByte(c)
		default:
			fmt.Fprintf(&b, "\\x%02x", c)
		}
	}
	b.WriteString("\"")
	return b.String()
}

func goldmarkRanges(pred func(rune) bool) [][2]rune {
	var out [][2]rune
	start := rune(-1)
	for r := rune(0); r <= utf8.MaxRune+1; r++ {
		in := r <= utf8.MaxRune && !(r >= 0xD800 && r <= 0xDFFF) && pred(r)
		if in && start < 0 {
			start = r
		} else if !in && start >= 0 {
			out = append(out, [2]rune{start, r - 1})
			start = -1
		}
	}
	return out
}

func writeGoldmarkTables(dir, rustOutDir string) error {
	entities, err := goldmarkReadEntities(dir)
	if err != nil {
		return err
	}
	// Case folding: probe every scalar value through the public function.
	type fold struct {
		from rune
		to   string
	}
	var folds []fold
	for r := rune(0); r <= utf8.MaxRune; r++ {
		if r >= 0xD800 && r <= 0xDFFF {
			continue
		}
		in := string(r)
		got := string(gutil.DoFullUnicodeCaseFolding([]byte(in)))
		if got != in {
			folds = append(folds, fold{r, got})
		}
	}
	punct := goldmarkRanges(gutil.IsPunctRune)
	space := goldmarkRanges(gutil.IsSpaceRune)

	var b strings.Builder
	b.WriteString("//! @generated by `reference/dump/behaviour_goldmark.go` (`go run . -only goldmark`).\n")
	b.WriteString("//! DO NOT EDIT.\n//!\n")
	b.WriteString("//! goldmark v1.8.2's own data: the HTML5 entity table (`util/html5entities.gen.go`, each\n")
	b.WriteString("//! entry cross-checked against `util.LookUpHTML5EntityByName`), the full Unicode case folding\n")
	b.WriteString("//! `util.DoFullUnicodeCaseFolding` applies (recovered by probing every scalar value), and the\n")
	b.WriteString("//! `util.IsPunctRune` / `util.IsSpaceRune` predicates as inclusive range tables. The last two\n")
	b.WriteString("//! follow the Go toolchain's Unicode version.\n\n")
	fmt.Fprintf(&b, "/// HTML5 entities by name (without `&` and `;`), sorted by name for binary search.\n")
	fmt.Fprintf(&b, "pub(crate) static HTML5_ENTITIES: [(&[u8], &[u8]); %d] = [\n", len(entities))
	for _, e := range entities {
		fmt.Fprintf(&b, "    (%s, %s),\n", rustByteString(e[0]), rustByteString(e[1]))
	}
	b.WriteString("];\n\n")
	fmt.Fprintf(&b, "/// Full case folding of one scalar value, sorted by the source value.\n")
	fmt.Fprintf(&b, "pub(crate) static CASE_FOLDINGS: [(char, &str); %d] = [\n", len(folds))
	for _, f := range folds {
		esc := ""
		for _, r := range f.to {
			esc += fmt.Sprintf("\\u{%x}", r)
		}
		fmt.Fprintf(&b, "    ('\\u{%x}', \"%s\"),\n", f.from, esc)
	}
	b.WriteString("];\n\n")
	emit := func(name, doc string, rs [][2]rune) {
		fmt.Fprintf(&b, "/// %s, as sorted inclusive ranges.\n", doc)
		fmt.Fprintf(&b, "pub(crate) static %s: [(u32, u32); %d] = [\n", name, len(rs))
		for _, r := range rs {
			fmt.Fprintf(&b, "    (0x%x, 0x%x),\n", r[0], r[1])
		}
		b.WriteString("];\n\n")
	}
	emit("PUNCT_RUNE_RANGES", "`util.IsPunctRune` (`unicode.IsSymbol || unicode.IsPunct`)", punct)
	emit("SPACE_RUNE_RANGES", "`util.IsSpaceRune`", space)
	path := filepath.Join(rustOutDir, "tables_generated.rs")
	if err := os.MkdirAll(rustOutDir, 0o755); err != nil {
		return err
	}
	if err := os.WriteFile(path, []byte(strings.TrimRight(b.String(), "\n")+"\n"), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d entities, %d foldings, %d punct ranges, %d space ranges)\n",
		path, len(entities), len(folds), len(punct), len(space))
	return nil
}
