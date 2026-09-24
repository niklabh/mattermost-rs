package main

// Behavioural oracle for the text/plain half of every Mattermost e-mail, written to
// fixtures/behaviour_html2text.json (`go run . -only html2text`). Pins two ports:
//
//   - `parse`: golang.org/x/net/html's `html.Parse` (crates/gohtml). Each case is an input and
//     Go's whole tree — node type, Data, DataAtom, Namespace, attributes in order, children — or
//     the error Parse returned. The inputs are the `#data` sections of the tree-construction
//     data files x/net ships in its module (testdata/html5lib-tests/tree-construction/*.dat and
//     testdata/go/*.dat; the expected trees in those files are NOT used — Go produces the trees
//     here), a hand-written adversarial list, and a deterministic random tag soup. Inputs that
//     mention noscript are parsed a second time with scripting disabled.
//   - `html2text`: github.com/jaytaylor/html2text's `FromString` (crates/gohtml2text), which
//     platform/shared/mail/mail.go:312 runs over every HTML body. Inputs are every template of
//     server/templates executed through Mattermost's own `templates.New`, with three data
//     variants (escapable text, trusted markup, long text); the raw template sources; a
//     targeted list for every branch of the traversal; and a random corpus. The targeted list
//     also runs with `OmitLinks` and `TextOnly`.
//
// Determinism: fixed corpora, sorted file lists, fixed PCG seeds, no clock. Each case is one
// compact JSON line, so a changed tree shows up as a changed line.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"html/template"
	"math/rand/v2"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"unicode/utf8"

	"github.com/jaytaylor/html2text"
	"golang.org/x/net/html"

	"github.com/mattermost/mattermost/server/v8/platform/shared/templates"
)

// h2tNode is one node of a parse tree in pre-order, with its depth: a flat list rather than a
// nested one, because a 512-deep tree nests past every JSON reader's recursion limit.
type h2tNode struct {
	L  int         `json:"l"`
	T  int         `json:"t"`
	D  string      `json:"d,omitempty"`
	A  string      `json:"a,omitempty"`
	NS string      `json:"ns,omitempty"`
	At [][3]string `json:"at,omitempty"`
}

type h2tParseCase struct {
	Src      string     `json:"src"`
	In       string     `json:"in"`
	NoScript bool       `json:"noscript,omitempty"`
	Tree     []*h2tNode `json:"tree,omitempty"`
	Err      string     `json:"err,omitempty"`
}

type h2tOpts struct {
	OmitLinks bool `json:"omit_links,omitempty"`
	TextOnly  bool `json:"text_only,omitempty"`
}

type h2tTextCase struct {
	Src  string   `json:"src"`
	In   string   `json:"in"`
	Opts *h2tOpts `json:"opts,omitempty"`
	Out  string   `json:"out"`
	Err  string   `json:"err,omitempty"`
}

var h2tInvalid []string

func h2tCheck(where, s string) string {
	if !utf8.ValidString(s) {
		h2tInvalid = append(h2tInvalid, where)
	}
	return s
}

// h2tDump appends n and its descendants to out in pre-order.
func h2tDump(n *html.Node, level int, out []*h2tNode) []*h2tNode {
	node := &h2tNode{L: level, T: int(n.Type), D: h2tCheck("data", n.Data), NS: n.Namespace}
	if n.DataAtom != 0 {
		node.A = n.DataAtom.String()
	}
	for _, a := range n.Attr {
		node.At = append(node.At, [3]string{a.Namespace, h2tCheck("key", a.Key), h2tCheck("val", a.Val)})
	}
	out = append(out, node)
	for c := n.FirstChild; c != nil; c = c.NextSibling {
		out = h2tDump(c, level+1, out)
	}
	return out
}

func h2tParse(src, in string, scripting bool) h2tParseCase {
	c := h2tParseCase{Src: src, In: in, NoScript: !scripting}
	doc, err := html.ParseWithOptions(strings.NewReader(in), html.ParseOptionEnableScripting(scripting))
	if err != nil {
		c.Err = err.Error()
		return c
	}
	c.Tree = h2tDump(doc, 0, nil)
	return c
}

// h2tDatInputs reads the `#data` section of every test in a tree-construction .dat file.
func h2tDatInputs(path string) ([]string, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var out []string
	var cur []string
	inData := false
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<24)
	for sc.Scan() {
		line := sc.Text()
		if strings.HasPrefix(line, "#") {
			if inData {
				out = append(out, strings.Join(cur, "\n"))
				cur = nil
			}
			inData = line == "#data"
			continue
		}
		if inData {
			cur = append(cur, line)
		}
	}
	if inData {
		out = append(out, strings.Join(cur, "\n"))
	}
	return out, sc.Err()
}

func h2tModuleDir(mod string) (string, error) {
	out, err := exec.Command("go", "list", "-m", "-f", "{{.Dir}}", mod).Output()
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(string(out)), nil
}

// h2tAdversarial are hand-written inputs aimed at the tree builder's rarer paths.
var h2tAdversarial = []string{
	"",
	"   ",
	"\x00",
	"a\x00b",
	"<table>\x00x</table>",
	"<svg>\x00</svg>",
	"<!DOCTYPE html><p>x",
	"<!doctype HTML PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\"><table><p>quirks",
	"<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/html4/loose.dtd\"><table><p>",
	"<!DOCTYPE html SYSTEM \"http://www.ibm.com/data/dtd/v11/IBMXHTML1-TRANSITIONAL.DTD\"><table><p>",
	"<!DOCTYPE html SYSTEM \"http://www.ibm.com/data/dtd/v11/ibmxhtml1-tranſitional.dtd\"><table><p>",
	"<!DOCTYPE İtml><table><p>",
	"<!DOCTYPE html PUBLIC 'unterminated",
	"<!DOCTYPE html PUBLIC \"a\" \"b\" junk><table><p>",
	"<!DOCTYPE html publicé><p>",
	"<!DOCTYPE>",
	"<!DOCTYPE html><html><head></head><body></body></html>",
	"<html a=1 b=2><html a=3 c=4><body x=1><body x=2 y=3>",
	"<b><p>x</b>y</p>",
	"<a><p>x</a>y",
	"<b>1<i>2<u>3<s>4</b>5",
	"<a href=1><a href=2>x</a>",
	"<b a=1 b=2><b b=2 a=1><b a=1 b=2><b b=2 a=1>x",
	"<b><b><b><b>x</b></b></b></b>",
	"<p><b><b><b><b></p><p>x",
	"<p><b a=1><b a=1><b a=2><b a=1></p><p>x",
	"<font color=red><font color=red><font color=red><font color=red><p>x</font>",
	"<nobr>a<nobr>b<nobr>c",
	"<table><tr><td>a<b>b</td><td>c</b>d</table>",
	"<table>text<tr><td>x</table>",
	"<table><b>x<tr><td>y</td></tr>z</b></table>",
	"<table><template>x</template></table>",
	"<template><tr><td>x</td></tr></template>",
	"<template><template><col></template></template>",
	"<svg><template>x</template></svg>",
	"<math><template>",
	"<svg><foreignObject><template><p>x</p></template></foreignObject></svg>",
	"<template><svg><template>",
	"<body><template><frameset>",
	"<frameset><frame><frameset><frame></frameset><noframes>x</noframes></frameset>",
	"<frameset>text<!--c--></frameset> after <html>",
	"<p><frameset>",
	"<select><option>a<option>b<optgroup>c</select>",
	"<select><select>x",
	"<select><input>",
	"<select><textarea>x</textarea>",
	"<table><select><tr>",
	"<ruby>a<rb>b<rt>c<rp>d<rtc>e</ruby>",
	"<svg><title>t<b>x</b></title></svg>",
	"<svg><desc><p>x</p></desc></svg>",
	"<svg><foreignobject><div>x</div></foreignobject></svg>",
	"<svg viewbox='0 0 1 1' xlink:href='x' xml:lang='en' XMLNS:XLINK='y'><clippath><path/></clippath></svg>",
	"<math definitionurl=x><mi>i<mglyph><malignmark></mi><annotation-xml encoding='TEXT/HTML'><p>x</p></annotation-xml></math>",
	"<math><annotation-xml><svg><p>x</svg></annotation-xml></math>",
	"<math><mtext><b>x</b></mtext></math>",
	"<svg><![CDATA[a<b>c]]></svg>",
	"<![CDATA[x]]>",
	"<svg><![CDATA[unterminated",
	"<svg><font color=red>x</font></svg>",
	"<svg><font>x</font></svg>",
	"<svg><p>x</svg>",
	"<svg><g></G><rect/></svg>after",
	"<svg><ſvg></SVG>",
	"<p><svg></p>x",
	"<textarea>\nx</textarea><textarea>\r\ny</textarea>",
	"<pre>\n\nx</pre><listing>\ny</listing>",
	"<plaintext><p>all text</p>",
	"<xmp><b>x</b></xmp><iframe><b></iframe><noembed><b></noembed>",
	"<noscript><p>x</p></noscript>",
	"<head><noscript><link><style>x</style><p>y</noscript></head>",
	"<head><noscript><!--c--> <meta></noscript>",
	"<title>a<b>b</title><script>if(a<b)</script><style><!--x--></style>",
	"<script><!--<script></script>--></script>x",
	"<head></head> <p>x</p></body> </html> <!--c--> <p>y",
	"</html><p>x",
	"</br>x</p>",
	"<p>a</p></p>",
	"<li>a<li>b<ul><li>c</ul>",
	"<dd>a<dt>b<div><dd>c",
	"<h1>a<h2>b</h1>c",
	"<form><form><input></form></form>",
	"<table><form><input type=HIDDEN><input type=text></form></table>",
	"<table><input type=hidden><td>",
	"<button><button>x",
	"<applet><b>x</applet>y",
	"<marquee><p>x</marquee>",
	"<object><param><embed></object>",
	"<image src=x>",
	"<isindex>",
	"<hr/><br/><img/><wbr><keygen><area><input/>",
	"<caption>x<col><colgroup><tbody><td><tfoot><th><thead><tr><frame><head>",
	"<table><caption>x<td>y</table>",
	"<table><colgroup><col><col>x</colgroup></table>",
	"<table><tbody><tr><th>x<td>y</tbody><tfoot><tr><td>z</table>",
	"<table><tr><td><table><tr><td>x</td></tr></table></td></tr></table>",
	"<a><div><a><div><a>x",
	"<div><b><i><u><s><em><strong><big><small><code><tt>x</div>y",
	"<b><div><b><div><b><div><b><div><b><div><b><div><b><div><b><div><b><div>x</b>y",
	"<custom-element a=b>x</custom-element><CUSTOM>y</custom>",
	"<a b c=d e='f' g=\"h\" b=dup>x",
	"<p =x a=/>y",
	"<br a=b/>",
	"<!-->x<!--->y<!-- -- -->z<!--x--!>w",
	"<?php echo 1 ?>",
	"</>x</ >y</#>z",
	"&amp;&lt;&gt;&quot;&#39;&#x26;&nbsp;&copy;&notit;&notin;&#0;&#x110000;&#128;",
	"<a href='&amp;&lt;x&copy=1&amp=2'>x</a>",
	strings.Repeat("<div>", 509) + "x",
	strings.Repeat("<div>", 510) + "x",
	strings.Repeat("<div>", 511) + "x",
	strings.Repeat("<div>", 512) + "x",
	strings.Repeat("<div>", 600) + "x",
	strings.Repeat("<b><p>", 300) + "x",
	strings.Repeat("<table>", 300),
	strings.Repeat("<a><p>x</a>", 100),
	strings.Repeat("<b><p>x</b>", 200),
	strings.Repeat("<svg>", 520),
	strings.Repeat("<template>", 520),
	"<body>\u00a0\u3000x",
	"x\r\ny\rz",
}

// h2tParseFuzz builds deterministic tag soup over the vocabulary the tree builder switches on.
func h2tParseFuzz(n int) []string {
	r := rand.New(rand.NewPCG(0x68746d6c, 0x7061727365))
	tags := []string{"html", "head", "body", "frameset", "frame", "noframes", "template", "table",
		"caption", "colgroup", "col", "tbody", "thead", "tfoot", "tr", "td", "th", "select",
		"option", "optgroup", "input", "textarea", "form", "p", "div", "li", "dd", "dt", "ul",
		"ol", "dl", "a", "b", "i", "u", "nobr", "font", "em", "strong", "applet", "marquee",
		"object", "button", "h1", "h2", "h6", "pre", "listing", "xmp", "iframe", "noembed",
		"noscript", "script", "style", "title", "math", "mi", "mo", "mtext", "annotation-xml",
		"svg", "foreignObject", "desc", "g", "path", "image", "br", "hr", "img", "span", "ruby",
		"rb", "rt", "rp", "rtc", "base", "link", "meta", "blockquote", "address", "center",
		"main", "search", "section", "mglyph", "malignmark", "custom", "keygen", "param", "wbr"}
	attrs := []string{"", " color=red", " type=hidden", " type=text", " encoding=text/html",
		" href=x", " a=1 b=2", " b=2 a=1", " xlink:href=y", " definitionurl=z", " viewbox=v",
		" face=f", " size=3", "/"}
	texts := []string{"x", " ", "\n", "\t", "\x00", "a b", "&amp;", "&lt;p&gt;", " \n ", "é", "\r\n",
		"<!--c-->", "<![CDATA[d]]>", "<!DOCTYPE html>", "</>", "<?x?>"}
	var out []string
	for k := 0; k < n; k++ {
		var b strings.Builder
		if r.IntN(6) == 0 {
			b.WriteString("<!DOCTYPE html>")
		}
		steps := 3 + r.IntN(25)
		for s := 0; s < steps; s++ {
			switch r.IntN(10) {
			case 0, 1, 2, 3:
				t := tags[r.IntN(len(tags))]
				if r.IntN(4) == 0 {
					t = strings.ToUpper(t)
				}
				b.WriteString("<" + t + attrs[r.IntN(len(attrs))] + ">")
			case 4, 5, 6:
				b.WriteString("</" + tags[r.IntN(len(tags))] + ">")
			default:
				b.WriteString(texts[r.IntN(len(texts))])
			}
		}
		out = append(out, b.String())
	}
	return out
}

func h2tParseCases() ([]h2tParseCase, error) {
	dir, err := h2tModuleDir("golang.org/x/net")
	if err != nil {
		return nil, err
	}
	var files []string
	for _, pat := range []string{"html/testdata/html5lib-tests/tree-construction/*.dat", "html/testdata/go/*.dat"} {
		m, err := filepath.Glob(filepath.Join(dir, pat))
		if err != nil {
			return nil, err
		}
		files = append(files, m...)
	}
	sort.Strings(files)
	if len(files) < 50 {
		return nil, fmt.Errorf("only %d tree-construction data files under %s", len(files), dir)
	}
	var cases []h2tParseCase
	add := func(src, in string) {
		if !utf8.ValidString(in) {
			return
		}
		cases = append(cases, h2tParse(src, in, true))
		if strings.Contains(strings.ToLower(in), "noscript") {
			cases = append(cases, h2tParse(src, in, false))
		}
	}
	for _, f := range files {
		inputs, err := h2tDatInputs(f)
		if err != nil {
			return nil, err
		}
		rel, _ := filepath.Rel(filepath.Join(dir, "html", "testdata"), f)
		for i, in := range inputs {
			add(fmt.Sprintf("%s#%d", rel, i), in)
		}
	}
	for i, in := range h2tAdversarial {
		add(fmt.Sprintf("adversarial#%d", i), in)
	}
	for i, in := range h2tParseFuzz(1500) {
		add(fmt.Sprintf("fuzz#%d", i), in)
	}
	return cases, nil
}

func h2tText(src, in string, o *h2tOpts) h2tTextCase {
	c := h2tTextCase{Src: src, In: in, Opts: o}
	var opts []html2text.Options
	if o != nil {
		opts = append(opts, html2text.Options{OmitLinks: o.OmitLinks, TextOnly: o.TextOnly})
	}
	out, err := html2text.FromString(in, opts...)
	if err != nil {
		c.Err = err.Error()
	}
	c.Out = h2tCheck("html2text "+src, out)
	return c
}

var h2tLong = strings.Repeat("lorem ipsum dolor sit amet ", 8) + strings.Repeat("x", 90) + " tail"

// h2tTargeted aims at every branch of html2text's traversal.
var h2tTargeted = []string{
	"", "   ", "plain text", "<br>", "a<br>b", "a<br/><br/><br/>b", "<br>lead",
	"<h1>Title</h1>", "<h2>Sub</h2>text", "<h3>Third</h3>after", "<h1>line one<br>line two is longer</h1>",
	"<h2>ünïcödé 日本語 wide</h2>", "<h1></h1>", "<h1>x</h1>", "<h2>ab</h2>", "<h3>abc<br>a</h3>",
	"<h1><a href='http://x.example'>link</a></h1>", "<h1><b>bold</b> title</h1>", "<h4>four</h4><h5>five</h5><h6>six</h6>",
	"<blockquote><h2>in quote</h2></blockquote>", "<h1>  spaced   title  </h1>", "<h2>a\nb</h2>",
	"<blockquote>q</blockquote>", "<blockquote>a<blockquote>b<blockquote>c</blockquote>d</blockquote>e</blockquote>",
	"<blockquote>" + h2tLong + "</blockquote>",
	"<blockquote>" + strings.Repeat("y", 200) + "</blockquote>",
	"<blockquote><blockquote>" + h2tLong + "</blockquote></blockquote>",
	"<p>before</p><blockquote>quoted text</blockquote><p>after</p>",
	"<blockquote>one<br>two<br>" + h2tLong + "</blockquote>",
	"<blockquote>" + strings.Repeat("a", 73) + " b</blockquote>",
	"<blockquote>" + strings.Repeat("a", 74) + " b</blockquote>",
	"<blockquote>" + strings.Repeat("a", 75) + " b</blockquote>",
	"<blockquote>" + strings.Repeat("a", 72) + " bb cc</blockquote>",
	"<blockquote>" + strings.Repeat("ab\u00a0", 40) + "</blockquote>",
	"<blockquote>" + strings.Repeat("日本\u3000", 40) + "</blockquote>",
	"<blockquote><span>" + strings.Repeat("w", 50) + "</span><span>" + strings.Repeat("z", 50) + "</span></blockquote>",
	"<blockquote><b>" + h2tLong + "</b></blockquote>",
	"<blockquote><a href='http://example.com/" + strings.Repeat("p", 80) + "'>link</a></blockquote>",
	"<blockquote><pre>" + h2tLong + "\n  keep</pre></blockquote>",
	"<blockquote><div>a</div><div>b</div></blockquote>",
	"<blockquote><ul><li>a</li><li>b</li></ul></blockquote>",
	"<blockquote><blockquote>x</blockquote></blockquote><blockquote>y</blockquote>",
	"<div>a</div><div>b</div>", "text<div>in div</div>", "<div><div>x</div></div>after", "<div></div>",
	"<div>a</div>text", "<div>a</div><br>b", "<div><span>x</span></div><div><div>y</div>z</div>",
	"<ul><li>one</li><li>two</li></ul>", "<ol><li>x</li></ol>", "<ul><li>a<ul><li>b</li></ul></li></ul>",
	"<li>bare</li>", "<ul><li></li></ul>",
	"<p>para</p><p>second</p>", "text<p>p</p>text", "<p></p><p></p><p></p><p>b</p>", "<p>a</p>\n\n<p>b</p>",
	"<b>bold</b>", "a <strong>b</strong> c", "<b></b>", "<b> spaced </b>", "<b><a href='http://x.example'>l</a></b>",
	"<b>x</b>.", "<b>.x</b>", "a<b>.b</b>", "<strong><strong>n</strong></strong>",
	"<a href='http://example.com'>http://example.com</a>", "<a href='http://example.com'>Example</a>",
	"<a href=' http://example.com '>http://example.com</a>", "<a href='mailto:foo@example.com'>foo@example.com</a>",
	"<a href='mailto:foo@example.com'>Mail me</a>", "<a href=''>empty</a>", "<a>no href</a>", "<a href='x'></a>",
	"<a href='http://x.example'><img src='i.png' alt='Alt text'></a>", "<a href='http://x.example'><img src='i.png'></a>",
	"<a href='http://x.example'><img alt=''></a>", "<a href='http://x.example'>text <b>bold</b></a>",
	"<a href='http://x.example'>  http://x.example  </a>", "<a href='MAILTO:x@y'>x@y</a>", "<a href='mailto:'>m</a>",
	"<a href='mailto: x@y'>x@y</a>", "<a href='http://x.example'><img alt='a'> </a>", "<a href='http://x.example'><span>t</span></a>",
	"<a href='http://x.example'><img alt='a'><img alt='b'></a>", "<img alt='lonely'>", "a<a href='h'>b</a>c",
	"<a href='http://x.example'>x</a>.", "<a href='.dot'>.dot</a>",
	"<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>",
	"<table><thead><tr><th>h1</th><th>h2</th></tr></thead><tbody><tr><td>x</td></tr></tbody><tfoot><tr><td>f</td></tr></tfoot></table>",
	"<table><caption>cap</caption><tr><td>x</td></tr></table>", "text<table><tr><td>t</td></tr></table>text",
	"<pre>  keep   spaces\n  and lines</pre>", "<pre>\nleading newline</pre>", "a<pre>b</pre>c", "<pre></pre>",
	"<pre><b>bold  pre</b></pre>", "<pre>\n</pre>", "<pre> </pre>x",
	"<style>body{color:red}</style>text", "<script>alert(1)</script>after", "<head><title>T</title><style>x</style></head><body>b</body>",
	"<html><head><meta charset=utf-8><title>Title</title></head><body><p>Body</p></body></html>",
	"\ufeffhello", "\ufeff\ufeffhello", "\ufeff\ufeff\ufeffhello", "a\ufeffb", "\ufeff",
	"a   b\n\n c\t\td", "  lead", "\r\n", "x\u00a0y", "\u00a0lead", "x\u2003y", "x\u3000y", "x\u0085y", "x\u000by",
	"x\u000cy", "<span>\u00a0</span>after", "tail\u00a0",
	". starts", "text<span>.after</span>", "a<span>b</span>", "<span>a</span><span>b</span>", "a <span> b</span>",
	"&lt;tag&gt; &amp; &nbsp; &copy; &#8212;", "<!-- comment -->visible", "<!DOCTYPE html>doc",
	"<p>a</p><blockquote>\n</blockquote>", "<div>\n</div>x", "<br><br><br>",
	"<svg><a href='http://svg.example'>svg link</a><title>t</title></svg>", "<math><mi>x</mi></math>",
	"<textarea>ta\n  text</textarea>", "<select><option>o1<option>o2</select>", "<noscript>ns</noscript>",
	"<iframe>if</iframe>", "<xmp><b>x</b></xmp>", "<plaintext><b>raw</b>",
	"<frameset><frame></frameset>", "<table><tr><td>" + h2tLong + "</td></tr></table>",
	"<blockquote><h1>Title</h1>" + h2tLong + "</blockquote>",
	"<p>Hi <b>Name</b>,</p><p>Click <a href='https://example.com/verify?t=abc&amp;x=1'>here</a> to verify.</p>",
	strings.Repeat("<div>", 600) + "deep",
	strings.Repeat("<div>", 509) + "deep",
	strings.Repeat("<blockquote>", 509) + "deepest quote " + h2tLong,
	strings.Repeat("<h1>", 5) + strings.Repeat("<b>", 300) + "nested emphasis",
	strings.Repeat("<blockquote>", 40) + "deep quote " + h2tLong,
}

func h2tTextFuzz(n int) []string {
	r := rand.New(rand.NewPCG(0x68327478, 0x74657874))
	open := []string{"<br>", "<h1>", "<h2>", "<h3>", "<blockquote>", "<div>", "<li>", "<ul>", "<p>",
		"<b>", "<strong>", "<a href='http://e.example/p'>", "<a href='mailto:u@e.example'>",
		"<a href=''>", "<a>", "<table>", "<tr>", "<td>", "<th>", "<tfoot>", "<pre>", "<style>",
		"<script>", "<head>", "<span>", "<img alt='alt'>", "<img>", "<h4>"}
	closes := []string{"</h1>", "</h2>", "</h3>", "</blockquote>", "</div>", "</li>", "</ul>", "</p>",
		"</b>", "</strong>", "</a>", "</table>", "</tr>", "</td>", "</pre>", "</style>",
		"</script>", "</span>"}
	texts := []string{"word", "two words", " lead", "trail ", ".dot", "http://e.example/p",
		"u@e.example", "\u00a0nb", "日本語", "\n\nnl", "  \t ", "é", h2tLong, strings.Repeat("q", 80),
		"a.b", "&amp;", "x"}
	var out []string
	for k := 0; k < n; k++ {
		var b strings.Builder
		steps := 2 + r.IntN(20)
		for s := 0; s < steps; s++ {
			switch r.IntN(10) {
			case 0, 1, 2, 3:
				b.WriteString(open[r.IntN(len(open))])
			case 4, 5:
				b.WriteString(closes[r.IntN(len(closes))])
			default:
				b.WriteString(texts[r.IntN(len(texts))])
			}
		}
		out = append(out, b.String())
	}
	return out
}

var h2tIdentRe = regexp.MustCompile(`\.([A-Z][A-Za-z0-9_]*)`)

// h2tRangeKeys are the template identifiers a `range` or `len` iterates over.
var h2tRangeKeys = map[string]bool{"Posts": true, "MessageAttachments": true, "FieldRows": true,
	"Cells": true, "Browsers": true, "BulletListItems": true}

func h2tValue(key string, variant int) any {
	isURL := strings.HasSuffix(key, "URL") || strings.HasSuffix(key, "Url") ||
		strings.HasSuffix(key, "Link") || strings.HasSuffix(key, "Src") || strings.HasSuffix(key, "Icon") ||
		strings.HasSuffix(key, "Photo") || key == "Image"
	switch variant {
	case 0:
		if isURL {
			return "https://mm.example.com/" + key + "?a=1&b=<2>"
		}
		return key + ` value <b>&amp; "quoted" 'single' — ünïcödé 日本語`
	case 1:
		if isURL {
			return template.URL("https://mm.example.com/" + key)
		}
		return template.HTML("<b>" + key + "</b> <a href=\"https://mm.example.com/" + key +
			"\">link " + key + "</a><br>second line<blockquote>quote of " + key + "</blockquote>")
	default:
		if isURL {
			return "mailto:" + strings.ToLower(key) + "@mm.example.com"
		}
		return key + " " + h2tLong
	}
}

func h2tUniversal(idents []string, variant, depth int) map[string]any {
	m := map[string]any{}
	for _, id := range idents {
		if id == "BulletListItems" {
			m[id] = []any{h2tValue("BulletOne", variant), h2tValue("BulletTwo", variant)}
			continue
		}
		if h2tRangeKeys[id] {
			var items []any
			if depth > 0 {
				items = []any{h2tUniversal(idents, variant, depth-1), h2tUniversal(idents, variant, depth-1)}
			}
			m[id] = items
			continue
		}
		m[id] = h2tValue(id, variant)
	}
	return m
}

func h2tTemplateCases() ([]h2tTextCase, error) {
	dir, err := filepath.Abs("../mattermost/server/templates")
	if err != nil {
		return nil, err
	}
	container, err := templates.New(dir)
	if err != nil {
		return nil, err
	}
	files, err := filepath.Glob(filepath.Join(dir, "*.html"))
	if err != nil {
		return nil, err
	}
	sort.Strings(files)
	identSet := map[string]bool{}
	var names []string
	defineRe := regexp.MustCompile(`\{\{\s*define\s+"([^"]+)"`)
	var cases []h2tTextCase
	for _, f := range files {
		src, err := os.ReadFile(f)
		if err != nil {
			return nil, err
		}
		for _, m := range h2tIdentRe.FindAllStringSubmatch(string(src), -1) {
			identSet[m[1]] = true
		}
		for _, m := range defineRe.FindAllStringSubmatch(string(src), -1) {
			names = append(names, m[1])
		}
		cases = append(cases, h2tText("source/"+filepath.Base(f), string(src), nil))
	}
	var idents []string
	for id := range identSet {
		idents = append(idents, id)
	}
	sort.Strings(idents)
	sort.Strings(names)
	rendered := 0
	for _, name := range names {
		for variant := 0; variant < 3; variant++ {
			props := h2tUniversal(idents, variant, 2)
			htmlMap := map[string]template.HTML{}
			for _, id := range idents {
				htmlMap[id] = template.HTML("<i>" + id + "</i>")
			}
			var body string
			var rerr error
			if variant == 0 && strings.HasPrefix(name, "unsupported_browser-") {
				body, rerr = container.RenderToString(name, templates.Data{})
			} else {
				body, rerr = container.RenderToString(name, templates.Data{Props: props, HTML: htmlMap})
			}
			if rerr != nil {
				// A template that needs a different root value (the unsupported_browser partials
				// take a browser, not templates.Data) is rendered from its universal map instead.
				var buf bytes.Buffer
				t := template.Must(template.ParseGlob(filepath.Join(dir, "*.html")))
				if err := t.ExecuteTemplate(&buf, name, props); err != nil {
					continue
				}
				body = buf.String()
			}
			rendered++
			cases = append(cases, h2tText(fmt.Sprintf("template/%s/%d", name, variant), body, nil))
		}
	}
	if rendered < 60 {
		return nil, fmt.Errorf("only %d template renders succeeded", rendered)
	}
	return cases, nil
}

func writeHTML2TextBehaviourFixture(outDir string) error {
	parseCases, err := h2tParseCases()
	if err != nil {
		return fmt.Errorf("parse corpus: %w", err)
	}
	var textCases []h2tTextCase
	tpl, err := h2tTemplateCases()
	if err != nil {
		return fmt.Errorf("template corpus: %w", err)
	}
	textCases = append(textCases, tpl...)
	for i, in := range h2tTargeted {
		src := fmt.Sprintf("targeted#%d", i)
		textCases = append(textCases, h2tText(src, in, nil),
			h2tText(src, in, &h2tOpts{OmitLinks: true}),
			h2tText(src, in, &h2tOpts{TextOnly: true}),
			h2tText(src, in, &h2tOpts{OmitLinks: true, TextOnly: true}))
	}
	for i, in := range h2tTextFuzz(1500) {
		var o *h2tOpts
		switch i % 7 {
		case 3:
			o = &h2tOpts{TextOnly: true}
		case 5:
			o = &h2tOpts{OmitLinks: true}
		}
		textCases = append(textCases, h2tText(fmt.Sprintf("fuzz#%d", i), in, o))
	}
	if len(h2tInvalid) > 0 {
		return fmt.Errorf("Go produced invalid UTF-8 from valid input in %d places, first %q", len(h2tInvalid), h2tInvalid[0])
	}

	// One compact line per case, without Go's default escaping of <, > and & (the corpus is
	// mostly markup, and \u003c triples its size).
	marshal := func(v any) ([]byte, error) {
		var b bytes.Buffer
		enc := json.NewEncoder(&b)
		enc.SetEscapeHTML(false)
		if err := enc.Encode(v); err != nil {
			return nil, err
		}
		return bytes.TrimSuffix(b.Bytes(), []byte("\n")), nil
	}
	var buf bytes.Buffer
	buf.WriteString("{\n\"parse\": [\n")
	for i, c := range parseCases {
		line, err := marshal(c)
		if err != nil {
			return err
		}
		buf.Write(line)
		if i < len(parseCases)-1 {
			buf.WriteByte(',')
		}
		buf.WriteByte('\n')
	}
	buf.WriteString("],\n\"html2text\": [\n")
	for i, c := range textCases {
		line, err := marshal(c)
		if err != nil {
			return err
		}
		buf.Write(line)
		if i < len(textCases)-1 {
			buf.WriteByte(',')
		}
		buf.WriteByte('\n')
	}
	buf.WriteString("]\n}\n")
	path := filepath.Join(outDir, "behaviour_html2text.json")
	if err := os.WriteFile(path, buf.Bytes(), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s: %d parse cases, %d html2text cases\n", path, len(parseCases), len(textCases))
	return nil
}
