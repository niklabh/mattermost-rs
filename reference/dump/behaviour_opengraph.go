package main

// Behavioural oracle for the OpenGraph half of a link preview, written to
// fixtures/behaviour_opengraph.json. Pins five ports at once, each against Go's own code:
//
//   - `tokens`: golang.org/x/net/html's Tokenizer, token by token (type, Text or TagName, and
//     every TagAttr), over a hand-written hostile corpus plus a deterministic random one.
//     dyatlov/go-opengraph reads meta tags off exactly this stream (mm_model::go_html).
//   - `unescape` / `std_unescape`: x/net's UnescapeString and the standard library's
//     html.UnescapeString — two implementations that disagree (mm_model::go_html).
//   - `labels`, `charset`, `sweep`: charset.Lookup over every htmlindex label (read from the
//     module cache's tables.go at run time, so the list is Go's), DetermineEncoding and NewReader
//     over a corpus, and every single-byte table decoded for all 256 byte values
//     (mm_model::go_charset).
//   - `times`: time.Parse(time.RFC3339, …) and the JSON a *time.Time marshals to.
//   - `opengraph`: app.parseOpenGraphMetadata (without the image proxy) — its body is unexported,
//     so the four helpers are copied below verbatim from channels/app/opengraph.go:54-170 — then
//     model.TruncateOpenGraph. Both marshalled with json.Marshal, as bytes.
//   - `oembed`: oembed.ResponseFromJSON, FindEndpointForURL / GetProviderURL, and
//     parseOpenGraphFromOEmbed (copied from opengraph.go:172-200).
//
// Byte-valued inputs and outputs are []byte, i.e. base64, because a Go string can hold what a
// JSON string cannot.
//
// Determinism: the random corpus uses a fixed PCG seed.

import (
	"bytes"
	"encoding/json"
	"fmt"
	stdhtml "html"
	"io"
	"math/rand/v2"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/dyatlov/go-opengraph/opengraph"
	ogImage "github.com/dyatlov/go-opengraph/opengraph/types/image"
	"golang.org/x/net/html"
	"golang.org/x/net/html/charset"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/app/oembed"
)

type ogToken struct {
	Type  string     `json:"t"`
	Data  []byte     `json:"d"`
	Attrs [][][]byte `json:"a"`
}

type ogTokenCase struct {
	Doc    []byte    `json:"doc"`
	Tokens []ogToken `json:"tokens"`
}

func ogTokenize(doc []byte) []ogToken {
	z := html.NewTokenizer(bytes.NewReader(doc))
	out := []ogToken{}
	for {
		tt := z.Next()
		if tt == html.ErrorToken {
			return out
		}
		tok := ogToken{Type: tt.String(), Attrs: [][][]byte{}}
		switch tt {
		case html.TextToken, html.CommentToken, html.DoctypeToken:
			tok.Data = bytes.Clone(z.Text())
		default:
			name, more := z.TagName()
			tok.Data = bytes.Clone(name)
			for more {
				var k, v []byte
				k, v, more = z.TagAttr()
				tok.Attrs = append(tok.Attrs, [][]byte{bytes.Clone(k), bytes.Clone(v)})
			}
		}
		out = append(out, tok)
	}
}

// ogDocs is the hand-written corpus. Each targets one tokenizer or ProcessMeta rule.
var ogDocs = []string{
	``,
	`plain text`,
	`<meta property="og:title" content="Title">`,
	`<META PROPERTY="og:title" CONTENT="Upper">`,
	`<meta property=og:title content=unquoted>`,
	`<meta property='og:title' content='single'>`,
	`<meta property="og:title" content="a"/>`,
	`<meta property="og:title" content=a/>`,
	`<meta/>`,
	`<meta>`,
	`<meta property="og:title" property="og:description" content="dup key keeps first">`,
	`<meta property="og:title" content="first" content="second">`,
	`</meta property="og:title" content="end tag">`,
	`<meta property="og:title" content="a &amp; b &lt; c &gt; d &quot;e&quot; &#39;f&#39;">`,
	`<meta property="og:title" content="&amp=x &ampx &amp;x &notit; &notin; &#x41;&#65;&#x;&#;&#1114112;&#0;&#128;">`,
	`<meta property="og:title" content="&nLt; &nGt; &NotEqualTilde;">`,
	`<meta property="og:description" content="&amp;lt;double&amp;gt; &amp;amp;">`,
	`<meta property="og:url" content="http://x/?a=1&amp=2&copy=3&copy">`,
	`<script><meta property="og:title" content="in script"></script><meta property="og:title" content="after script">`,
	`<script><!--<script></script><meta property="og:title" content="double escaped"></script>--></script><meta property="og:type" content="t">`,
	`<script><!-- <meta property="og:title" content="escaped"> --></script>`,
	`<style><meta property="og:title" content="in style"></style>`,
	`<noscript><meta property="og:title" content="in noscript"></noscript>`,
	`<title><meta property="og:title" content="in title"></title>`,
	`<textarea>&amp;<meta property="og:title" content="in textarea"></TEXTAREA >x`,
	`<xmp><meta property="og:title" content="x"></xmp><iframe><meta></iframe><noembed><meta></noembed><noframes><meta></noframes>`,
	`<plaintext><meta property="og:title" content="plaintext">`,
	`<!-- <meta property="og:title" content="in comment"> -->`,
	`<!--><meta property="og:title" content="after empty comment">`,
	`<!---><meta property="og:title" content="after dash comment">`,
	`<!-- a --!><meta property="og:title" content="bang close">`,
	`<!-- a --!- b --><meta property="og:title" content="c">`,
	`<!-- unterminated --`,
	`<!-- unterminated --!`,
	`<!-- unterminated -`,
	`<!DOCTYPE html><meta property="og:title" content="d">`,
	`<!doctype`,
	`<!DOCTY`,
	`<!DOCTYPE>`,
	`<![CDATA[<meta property="og:title" content="cdata">]]>`,
	`<?xml version="1.0"?><meta property="og:type" content="website">`,
	`</><meta property="og:title" content="x">`,
	`</ bogus><meta property="og:title" content="y">`,
	`<!x`,
	`<!`,
	`<`,
	`<a`,
	`<meta property="og:title" content="unterminated`,
	`<meta property="og:title" content=`,
	`<meta property`,
	`a < b <3 <meta property="og:title" content="lt text">`,
	"<meta\tproperty=\"og:title\"\ncontent=\"white\r\nspace\r\">",
	"<meta property=\"og:title\" content=\"nul\x00byte\"><m\x00eta>",
	`<meta =property="og:title" content="equals first">`,
	`<meta property = "og:title" content = "spaced">`,
	`<p a=/><meta property="og:title" content="self close check"/>`,
	`<meta property="og:type" content="article"><meta property="og:article:published_time" content="2020-01-02T03:04:05Z"><meta property="og:article:modified_time" content="2020-01-02T03:04:05.123456789+05:30"><meta property="og:article:expiration_time" content="bad"><meta property="og:article:section" content="S"><meta property="og:article:tag" content="t1"><meta property="og:article:tag" content="t2"><meta property="og:article:author" content="A"><meta name="description" content="unmatched">`,
	`<meta property="og:article:section" content="before type"><meta property="og:type" content="article">`,
	`<meta property="og:type" content="book"><meta property="og:book:release_date" content="2021-02-03T04:05:06-07:00"><meta property="og:book:isbn" content="123"><meta property="og:book:tag" content="x"><meta property="og:book:author" content="y">`,
	`<meta property="og:type" content="profile"><meta property="og:profile:first_name" content="F"><meta property="og:profile:last_name" content="L"><meta property="og:profile:username" content="U"><meta property="og:profile:gender" content="G">`,
	`<meta property="og:type" content="book"><meta property="og:type" content="article"><meta property="og:article:section" content="both flags">`,
	`<meta property="og:image" content="a.png"><meta property="og:image:secure_url" content="https://s/a.png"><meta property="og:image:type" content="image/png"><meta property="og:image:width" content="10"><meta property="og:image:height" content="20"><meta property="og:image" content="a.png"><meta property="og:image:url" content="b.png"><meta property="og:image:width" content="-1"><meta property="og:image:height" content="99999999999999999999"><meta property="og:image:width" content=" 5">`,
	`<meta property="og:image:width" content="7"><meta property="og:image:type" content="image/svg+xml"><meta property="og:image" content="c.png">`,
	`<meta property="og:image" content="1.png"><meta property="og:image" content="2.svg"><meta property="og:image" content="3.png"><meta property="og:image:secure_url" content="https://x/3.SVGZ"><meta property="og:image" content="4.png"><meta property="og:image" content="5.png"><meta property="og:image" content="6.png"><meta property="og:image" content="7.png">`,
	`<meta property="og:image" content="//cdn.example/i.png"><meta property="og:image" content="../up.png"><meta property="og:image" content="?q=1"><meta property="og:image" content="#frag"><meta property="og:image" content="/abs/./x/../y.png"><meta property="og:image" content="http://[bad"><meta property="og:image" content="mailto:x@y"><meta property="og:image" content="%zz">`,
	`<meta property="og:url" content="relative/page"><meta property="og:type" content="website">`,
	`<meta property="og:audio" content="a.mp3"><meta property="og:audio:secure_url" content="https://a.mp3"><meta property="og:audio:type" content="audio/mpeg"><meta property="og:audio:type" content="again"><meta property="og:audio" content="b.mp3">`,
	`<meta property="og:video" content="v.mp4"><meta property="og:video:secure_url" content="https://v"><meta property="og:video:type" content="video/mp4"><meta property="og:video:tag" content="tag"><meta property="og:video:width" content="640"><meta property="og:video:height" content="480"><meta property="og:video:duration" content="61"><meta property="og:video:release_date" content="2019-12-31T23:59:59Z"><meta property="og:video:actor" content="p1"><meta property="og:video:actor:role" content="r1"><meta property="og:video:actor:role" content="r2"><meta property="og:video:director" content="d"><meta property="og:video:writer" content="w"><meta property="og:video:url" content="v2.mp4">`,
	`<meta property="og:video:actor" content="actor first">`,
	`<meta property="og:music:duration" content="x"><meta property="og:music:release_date" content="2018-01-01T00:00:00+01:00"><meta property="og:music:album" content="alb"><meta property="og:music:album:disc" content="2"><meta property="og:music:album:track" content="3"><meta property="og:music:musician" content="m"><meta property="og:music:creator" content="c"><meta property="og:music:song" content="s1"><meta property="og:music:disc" content="1"><meta property="og:music:track" content="9"><meta property="og:music:song" content="s2"><meta property="og:music:track" content="bad">`,
	`<meta property="og:music:disc" content="4"><meta property="og:type" content="music.song">`,
	`<meta property="og:music:release_date" content="2018-01-01T00:00:00+24:00"><meta property="og:title" content="unmarshalable">`,
	`<meta property="og:determiner" content="the"><meta property="og:site_name" content="Site"><meta property="og:locale" content="en_US"><meta property="og:locale:alternate" content="fr_FR"><meta property="og:locale:alternate" content="de_DE"><meta property="og:description" content="D">`,
	`<meta property="og:title" content="` + strings.Repeat("ü", 305) + `"><meta property="og:description" content="` + strings.Repeat("x", 300) + `"><meta property="og:site_name" content="` + strings.Repeat("日本", 151) + `">`,
	`<meta property="og:type" content=""><meta property="og:url" content="">`,
	`<meta property="og:type" content="website"><meta name="og:title" content="name not property">`,
	`<meta property="OG:TITLE" content="case of value matters">`,
	`<svg><meta property="og:title" content="in svg"></svg>`,
	`<div title="<meta property='og:title' content='in attr'>">x</div>`,
	`<meta content="c" property="og:title">`,
	"\xef\xbb\xbf<meta property=\"og:title\" content=\"bom\">",
	"<meta property=\"og:title\" content=\"caf\xc3\xa9 \xff\xfe raw\">",
}

// ogRandomDocs builds documents from fragments that exercise the tokenizer's state changes.
func ogRandomDocs() []string {
	frags := []string{
		"<meta property=\"og:title\" content=\"T\">", "<meta property=og:image content=x.png>",
		"<meta property='og:image:width' content='3'>", "<", ">", "/", "<!", "<!--", "-->", "--!>",
		"-", "!", "<script>", "</script>", "<SCRIPT>", "</ScRiPt >", "<style>", "</style>",
		"<title>", "</title>", "<textarea>", "<plaintext>", "<!DOCTYPE", " ", "\n", "\r", "\t",
		"'", "\"", "=", "&amp;", "&", "&#", "&#x41", "a", "b", "<a href=x>", "</a>", "<br/>",
		"<meta", " property=\"og:description\"", " content=\"d\"", "<![CDATA[", "]]>", "<?", "\x00",
		"é", "\xff", "<!-->", "<noscript>", "</noscript>", "<xmp>", "</xmp", "<meta/>",
	}
	r := rand.New(rand.NewPCG(20260919, 105))
	docs := make([]string, 0, 400)
	for range 400 {
		var b strings.Builder
		n := 1 + r.IntN(14)
		for range n {
			b.WriteString(frags[r.IntN(len(frags))])
		}
		docs = append(docs, b.String())
	}
	return docs
}

var ogUnescapeCorpus = []string{
	"", "no amp", "&", "&&", "&amp", "&amp;", "&AMP", "&AMP;", "&Amp;", "&amp=", "&ampx", "&notit;",
	"&notin;", "&not", "&notx", "&#", "&#;", "&#x", "&#x;", "&#X41;", "&#65", "&#65;", "&#0;",
	"&#128;", "&#159;", "&#160;", "&#xD800;", "&#x10FFFF;", "&#x110000;", "&#99999999999;",
	"&#4294967361;", "&#xFFFFFFFF41;", "&nLt;", "&nGt;", "&NotEqualTilde;", "&acE;", "&lt&gt",
	"&ltx", "&lt=", "a&b&c", "&;", "&a;", "&aacute", "&aacutex", "&Aacute;", "x&copy2", "&frac12;",
	"&frac12", "&frac123", "&#x1F600;", "&#128512;", "&#x7F;", "&#13;",
}

type ogUnescapeCase struct {
	In  []byte `json:"in"`
	Out []byte `json:"out"`
}

type ogLabelCase struct {
	Label string `json:"label"`
	Name  string `json:"name"`
}

type ogCharsetCase struct {
	ContentType string `json:"content_type"`
	Body        []byte `json:"body"`
	Name        string `json:"name"`
	Certain     bool   `json:"certain"`
	Err         bool   `json:"err"`
	Out         []byte `json:"out"`
}

type ogSweepCase struct {
	Name string `json:"name"`
	Out  []byte `json:"out"`
}

type ogTimeCase struct {
	In      string `json:"in"`
	Ok      bool   `json:"ok"`
	JSON    string `json:"json"`
	JSONErr bool   `json:"json_err"`
}

type ogParseCase struct {
	Name        string `json:"name"`
	RequestURL  string `json:"request_url"`
	ContentType string `json:"content_type"`
	Body        []byte `json:"body"`
	Parsed      string `json:"parsed"`
	ParsedErr   bool   `json:"parsed_err"`
	Truncated   string `json:"truncated"`
}

type ogOEmbedCase struct {
	Body      []byte `json:"body"`
	Err       bool   `json:"err"`
	Type      string `json:"type"`
	Version   string `json:"version"`
	Title     string `json:"title"`
	Thumbnail string `json:"thumbnail_url"`
	Width     int    `json:"thumbnail_width"`
	Height    int    `json:"thumbnail_height"`
	OG        string `json:"og"`
}

type ogEndpointCase struct {
	URL      string `json:"url"`
	Provider string `json:"provider"`
}

// --- copied from channels/app/opengraph.go:54-170, minus the image proxy -----------------------

func ogParseOpenGraphMetadata(requestURL string, body io.Reader, contentType string) *opengraph.OpenGraph {
	og := opengraph.NewOpenGraph()
	body = ogForceHTMLEncodingToUTF8(io.LimitReader(body, 1024*1024*50), contentType)
	_ = og.ProcessHTML(body)
	ogMakeOpenGraphURLsAbsolute(og, requestURL)
	ogOpenGraphDecodeHTMLEntities(og)
	og = ogFilterSVGImagesFromOpenGraph(og)
	if og.URL != "" {
		og.URL = requestURL
	}
	return og
}

func ogForceHTMLEncodingToUTF8(body io.Reader, contentType string) io.Reader {
	r, err := charset.NewReader(body, contentType)
	if err != nil {
		return body
	}
	return r
}

func ogMakeOpenGraphURLsAbsolute(og *opengraph.OpenGraph, requestURL string) {
	parsedRequestURL, err := url.Parse(requestURL)
	if err != nil {
		return
	}
	makeURLAbsolute := func(resultURL string) string {
		if resultURL == "" {
			return resultURL
		}
		parsedResultURL, err := url.Parse(resultURL)
		if err != nil {
			return resultURL
		}
		if parsedResultURL.IsAbs() {
			return resultURL
		}
		return parsedRequestURL.ResolveReference(parsedResultURL).String()
	}
	og.URL = makeURLAbsolute(og.URL)
	for _, image := range og.Images {
		image.URL = makeURLAbsolute(image.URL)
		image.SecureURL = makeURLAbsolute(image.SecureURL)
	}
	for _, audio := range og.Audios {
		audio.URL = makeURLAbsolute(audio.URL)
		audio.SecureURL = makeURLAbsolute(audio.SecureURL)
	}
	for _, video := range og.Videos {
		video.URL = makeURLAbsolute(video.URL)
		video.SecureURL = makeURLAbsolute(video.SecureURL)
	}
}

func ogFilterSVGImagesFromOpenGraph(og *opengraph.OpenGraph) *opengraph.OpenGraph {
	if og == nil || len(og.Images) == 0 {
		return og
	}
	og.Images = model.FilterSVGImages(og.Images)
	return og
}

func ogOpenGraphDecodeHTMLEntities(og *opengraph.OpenGraph) {
	og.Title = stdhtml.UnescapeString(og.Title)
	og.Description = stdhtml.UnescapeString(og.Description)
}

func ogParseOpenGraphFromOEmbed(requestURL string, body io.Reader) (*opengraph.OpenGraph, error) {
	oEmbedResponse, err := oembed.ResponseFromJSON(io.LimitReader(body, 1024*1024*50))
	if err != nil {
		return nil, err
	}
	og := &opengraph.OpenGraph{
		Type:  "opengraph",
		Title: oEmbedResponse.Title,
		URL:   requestURL,
	}
	if oEmbedResponse.ThumbnailURL != "" {
		og.Images = append(og.Images, &ogImage.Image{
			Type:   "image",
			URL:    oEmbedResponse.ThumbnailURL,
			Width:  uint64(oEmbedResponse.ThumbnailWidth),
			Height: uint64(oEmbedResponse.ThumbnailHeight),
		})
	}
	og = ogFilterSVGImagesFromOpenGraph(og)
	return og, nil
}

// --- sections ---------------------------------------------------------------------------------

func ogLabels() ([]ogLabelCase, error) {
	gomodcache, err := exec.Command("go", "env", "GOMODCACHE").Output()
	if err != nil {
		return nil, err
	}
	src, err := os.ReadFile(filepath.Join(strings.TrimSpace(string(gomodcache)),
		"golang.org/x/text@v0.40.0/encoding/htmlindex/tables.go"))
	if err != nil {
		return nil, err
	}
	block := regexp.MustCompile(`(?s)var nameMap = map\[string\]htmlEncoding\{\n(.*?)\n\}`).FindSubmatch(src)
	labels := []string{}
	for _, m := range regexp.MustCompile(`"([^"]+)":`).FindAllSubmatch(block[1], -1) {
		labels = append(labels, string(m[1]))
	}
	labels = append(labels, " UTF-8 ", "utf_8", "Latin1", "KOI8-R\t", "Koi8-r", "unknown", "",
		" utf-8", "\vutf-8", "ISO-8859-8-I", "x-mac-roman", "ucs-2", "utf-16", "UTF-16LE")
	out := make([]ogLabelCase, 0, len(labels))
	for _, l := range labels {
		_, name := charset.Lookup(l)
		out = append(out, ogLabelCase{Label: l, Name: name})
	}
	return out, nil
}

func ogCharsetCases() []ogCharsetCase {
	type in struct{ ct, body string }
	long := strings.Repeat("a", 1020)
	ins := []in{
		{"", ""},
		{"text/html", "plain ascii"},
		{"text/html; charset=utf-8", "caf\xc3\xa9 \xff\xfe \xe2\x82 \xf0\x9f\x98 end"},
		{"text/html; charset=UTF-8", "\xed\xa0\x80 surrogate \xc0\xaf overlong"},
		{"", "caf\xc3\xa9 is valid"},
		{"", "caf\xc3\xa9"},
		{"", "\xc3\xa9"},
		{"", "\xe2\x82\xac!"},
		{"", "\xf0\x9f\x98\x80"},
		{"", "ab\xf0\x9f\x98\x80"},
		{"", "caf\xe9 latin1"},
		{"", long + "\xc3\xa9\xff after preview"},
		{"", long + "caf\xc3"},
		{"text/html; charset=windows-1252", "\x80\x81\x8d\x9f\xff"},
		{"text/html; charset=iso-8859-1", "\xe9\xa4"},
		{"text/html; charset=\"koi8-r\"", "\xc1\xc2"},
		{"text/html; charset=bogus", "<meta charset=iso-8859-2>\xa1"},
		{"text/html; charset=koi8-r; charset=utf-8", "\xc1"},
		{"text/html; charset=koi8-r; charset=koi8-r", "\xc1"},
		{"text/html;charset=koi8-r", "\xc1"},
		{"text/html; charset*=utf-8''koi8-r", "\xc1"},
		{"text/html; charset*0=koi; charset*1=8-r", "\xc1"},
		{"text/html; foo", "\xc1"},
		{"text/html;", "\xc1"},
		{"text/html; ;", "\xc1"},
		{"text", "\xc1"},
		{"text/", "\xc1"},
		{"text/html/x", "\xc1"},
		{"TEXT/HTML; CHARSET=ISO-8859-2", "\xa1"},
		{"", "<meta charset=iso-8859-2>\xa1"},
		{"", "<meta charset='windows-1251'>\xc0"},
		{"", "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=koi8-r\">\xc1"},
		{"", "<meta content=\"text/html; charset=koi8-r\">\xc1"},
		{"", "<meta content=\"text/html; charset=koi8-r\" http-equiv=content-type>\xc1"},
		{"", "<meta content='charset = \"iso-8859-5\"' http-equiv=content-type>\xb0"},
		{"", "<meta content='charset=' http-equiv=content-type>\xb0"},
		{"", "<meta charset=utf-16>caf\xc3\xa9 \xff"},
		{"", "<meta charset=utf-16le>x"},
		{"", "<meta charset=bogus><meta charset=koi8-r>\xc1"},
		{"", "<meta charset=koi8-r charset=utf-8>\xc1"},
		{"", "<!-- <meta charset=koi8-r> -->\xc1"},
		{"", "<script><meta charset=koi8-r></script>\xc1"},
		{"", long + "<meta charset=koi8-r>\xc1"},
		{"", "\xef\xbb\xbfcaf\xc3\xa9 \xff"},
		{"text/html; charset=koi8-r", "\xef\xbb\xbfx"},
		{"", "\xff\xfex\x00"},
		{"", "\xfe\xff\x00x"},
		{"text/html; charset=x-user-defined", "\x80\xff"},
		{"text/html; charset=shift_jis", "ascii only"},
		{"text/html; charset=shift_jis", "\x82\xa0"},
		{"text/html; charset=gbk", "ascii only"},
		{"text/html; charset=big5", "ascii"},
		{"text/html; charset=euc-kr", "ascii"},
		{"text/html; charset=euc-jp", "ascii"},
		{"text/html; charset=gb18030", "ascii"},
		{"text/html; charset=iso-2022-jp", "ascii"},
		{"text/html; charset=iso-2022-jp", "\x1b$B$\"\x1b(B"},
		{"text/html; charset=iso-2022-kr", "abc"},
		{"text/html; charset=utf-16le", "a\x00"},
	}
	out := make([]ogCharsetCase, 0, len(ins))
	for _, c := range ins {
		preview := []byte(c.body)
		if len(preview) > 1024 {
			preview = preview[:1024]
		}
		_, name, certain := charset.DetermineEncoding(preview, c.ct)
		oc := ogCharsetCase{ContentType: c.ct, Body: []byte(c.body), Name: name, Certain: certain}
		r, err := charset.NewReader(strings.NewReader(c.body), c.ct)
		if err != nil {
			oc.Err = true
		} else {
			oc.Out, _ = io.ReadAll(r)
		}
		out = append(out, oc)
	}
	return out
}

func ogSweep() []ogSweepCase {
	names := []string{
		"ibm866", "iso-8859-2", "iso-8859-3", "iso-8859-4", "iso-8859-5", "iso-8859-6",
		"iso-8859-7", "iso-8859-8", "iso-8859-8-i", "iso-8859-10", "iso-8859-13", "iso-8859-14",
		"iso-8859-15", "iso-8859-16", "koi8-r", "koi8-u", "macintosh", "windows-874",
		"windows-1250", "windows-1251", "windows-1252", "windows-1253", "windows-1254",
		"windows-1255", "windows-1256", "windows-1257", "windows-1258", "x-mac-cyrillic",
		"x-user-defined",
	}
	all := make([]byte, 256)
	for i := range all {
		all[i] = byte(i)
	}
	out := make([]ogSweepCase, 0, len(names))
	for _, n := range names {
		r, err := charset.NewReader(bytes.NewReader(all), "text/html; charset="+n)
		if err != nil {
			continue
		}
		b, _ := io.ReadAll(r)
		out = append(out, ogSweepCase{Name: n, Out: b})
	}
	return out
}

var ogTimeCorpus = []string{
	"2020-01-02T03:04:05Z", "2020-01-02T03:04:05.1Z", "2020-01-02T03:04:05.000000000Z",
	"2020-01-02T03:04:05.123456789123Z", "2020-01-02T03:04:05,5Z", "2020-01-02T3:04:05Z",
	"2020-01-02T03:04:05+05:30", "2020-01-02T03:04:05-00:00", "2020-01-02T03:04:05+00:00",
	"2020-01-02T03:04:05+24:00", "2020-01-02T03:04:05+23:60", "2020-01-02T03:04:05+25:00",
	"2020-01-02T03:04:05+05:61", "2020-01-02T03:04:05+0530", "2020-01-02T03:04:05z",
	"2020-02-29T00:00:00Z", "2021-02-29T00:00:00Z", "2020-13-01T00:00:00Z", "2020-00-01T00:00:00Z",
	"2020-01-00T00:00:00Z", "2020-1-02T03:04:05Z", "2020-01-2T03:04:05Z", "2020-01-02T24:00:00Z",
	"2020-01-02T03:60:00Z", "2020-01-02T03:04:60Z", "2020-01-02t03:04:05Z", "2020-01-02 03:04:05Z",
	"20200-01-02T03:04:05Z", "0000-01-01T00:00:00Z", "9999-12-31T23:59:59.999999999-23:59",
	"2020-01-02T03:04:05Zjunk", "2020-01-02T03:04:05", "", "2020-01-02T03:04:05.Z",
	"2020-01-02T03:04:05.5", "+020-01-02T03:04:05Z", "2020-01-02T03:04:05 +05:30",
	"2020-01-02T03:04:05+5:30", "2020-01-02T03:04:05+05:3", "2020-01-02T03:04:05*05:30",
}

func ogTimes() []ogTimeCase {
	out := make([]ogTimeCase, 0, len(ogTimeCorpus))
	for _, s := range ogTimeCorpus {
		c := ogTimeCase{In: s}
		t, err := time.Parse(time.RFC3339, s)
		if err == nil {
			c.Ok = true
			b, err := json.Marshal(&t)
			if err != nil {
				c.JSONErr = true
			} else {
				c.JSON = string(b)
			}
		}
		out = append(out, c)
	}
	return out
}

func ogParseCases() []ogParseCase {
	type in struct{ name, url, ct, body string }
	ins := []in{
		{"basic", "http://example.com/a/b?c=d", "text/html; charset=utf-8", `<html><head><meta property="og:type" content="website"><meta property="og:title" content="T &amp;amp; U"><meta property="og:url" content="https://elsewhere/"><meta property="og:image" content="/img.png"></head></html>`},
		{"no_ct", "https://example.com/", "", `<meta property="og:title" content="no content type">`},
		{"latin1_header", "https://example.com/", "text/html; charset=iso-8859-1", "<meta property=\"og:title\" content=\"caf\xe9\">"},
		{"latin1_default", "https://example.com/", "text/html", "<meta property=\"og:title\" content=\"caf\xe9\">"},
		{"meta_charset", "https://example.com/", "text/html", "<meta charset=koi8-r><meta property=\"og:title\" content=\"\xc1\xc2\">"},
		{"bad_request_url", "http://[bad", "text/html", `<meta property="og:image" content="rel.png"><meta property="og:url" content="rel">`},
		{"relative_base", "/relative/base", "text/html", `<meta property="og:image" content="rel.png">`},
		{"invalid_utf8_raw_absolute", "https://example.com/", "", "<meta property=\"og:title\" content=\"caf\xc3\xa9\">" + strings.Repeat(" ", 1100) + "<meta property=\"og:description\" content=\"bad \xff byte\"><meta property=\"og:image\" content=\"https://x/\xff.png\">"},
		{"svg_all", "https://example.com/", "text/html", `<meta property="og:image" content="a.svg"><meta property="og:image" content="b.SVG">`},
		{"description_entities", "https://example.com/", "text/html", `<meta property="og:description" content="&amp;#x41; &amp;nLt; &amp;notit;">`},
	}
	for i, d := range ogDocs {
		ins = append(ins, in{name: "doc_" + itoa(i), url: "https://example.com/dir/page.html", ct: "text/html", body: d})
	}
	for i, d := range ogRandomDocs() {
		ins = append(ins, in{name: "random_" + itoa(i), url: "https://example.com/dir/page.html", ct: "text/html; charset=utf-8", body: d})
	}
	out := make([]ogParseCase, 0, len(ins))
	for _, c := range ins {
		og := ogParseOpenGraphMetadata(c.url, strings.NewReader(c.body), c.ct)
		pc := ogParseCase{Name: c.name, RequestURL: c.url, ContentType: c.ct, Body: []byte(c.body)}
		if b, err := json.Marshal(og); err != nil {
			pc.ParsedErr = true
		} else {
			pc.Parsed = string(b)
			b, _ := json.Marshal(model.TruncateOpenGraph(og))
			pc.Truncated = string(b)
		}
		out = append(out, pc)
	}
	return out
}

func itoa(i int) string {
	return fmt.Sprintf("%03d", i)
}

func ogOEmbeds() []ogOEmbedCase {
	bodies := []string{
		`{"version":"1.0","type":"video","title":"V","thumbnail_url":"https://i/t.jpg","thumbnail_width":480,"thumbnail_height":360}`,
		`{"version":"1.0","type":"photo","thumbnail_url":"https://i/t.svg"}`,
		`{"version":"1.0","type":"rich","thumbnail_width":-1,"thumbnail_url":"x"}`,
		`{"version":"1.0","type":"link"} trailing garbage`,
		`{"VERSION":"1.0","Type":"video","TITLE":"folded","title":"exact"}`,
		`{"title":"exact first","TITLE":"then folded","version":"1.0","type":"video"}`,
		"{\"version\":\"1.0\",\"type\":\"video\",\"ſtitle\":\"x\",\"K\":1,\"title\":\"t\"}",
		`{"version":"1.0","type":"video","title":null}`,
		`{"version":"1.0","type":"video","width":"1"}`,
		`{"version":"1.0","type":"video","width":1.5}`,
		`{"version":"1.0","type":"video","width":1e2}`,
		`{"version":"1.0","type":"video","html":{}}`,
		`{"version":"1.0","type":"video","thumbnail_width":99999999999999999999}`,
		`{"version":"1.0","type":"video","unknown":[1,2]}`,
		`{"version":"2.0","type":"video"}`,
		`{"version":"1.0","type":"other"}`,
		`{"version":1,"type":"video"}`,
		`[]`,
		`null`,
		`"str"`,
		``,
		`   `,
		`{"version":"1.0","type":"video"`,
		"{\"version\":\"1.0\",\"type\":\"video\",\"title\":\"bad \xff byte\"}",
	}
	out := make([]ogOEmbedCase, 0, len(bodies))
	for _, b := range bodies {
		c := ogOEmbedCase{Body: []byte(b)}
		r, err := oembed.ResponseFromJSON(strings.NewReader(b))
		if err != nil {
			c.Err = true
		} else {
			c.Type, c.Version, c.Title = r.Type, r.Version, r.Title
			c.Thumbnail, c.Width, c.Height = r.ThumbnailURL, r.ThumbnailWidth, r.ThumbnailHeight
			og, _ := ogParseOpenGraphFromOEmbed("https://www.youtube.com/watch?v=x", strings.NewReader(b))
			j, _ := json.Marshal(og)
			c.OG = string(j)
		}
		out = append(out, c)
	}
	return out
}

func ogEndpoints() []ogEndpointCase {
	urls := []string{
		"https://www.youtube.com/watch?v=abc", "https://youtube.com/watch?v=abc",
		"https://m.youtube.com/v/abc", "https://youtu.be/abc", "http://youtu.be/abc",
		"https://www.youtube.com/playlist?list=x", "https://youtube.com/playlist?list=x",
		"https://www.youtube.com/shorts/x", "https://youtube.com/shorts/x",
		"https://www.youtube.com/embed/x", "https://www.youtube.com/live/x",
		"https://youtube.com/live/x", "https://evil.com/.youtube.com/watch",
		"https://a.youtube.com/watch\nx", "https://www.youtube.com/watch?v=a b&c=d",
		"https://example.com/",
	}
	out := make([]ogEndpointCase, 0, len(urls))
	for _, u := range urls {
		c := ogEndpointCase{URL: u}
		if p := oembed.FindEndpointForURL(u); p != nil {
			c.Provider = p.GetProviderURL(u)
		}
		out = append(out, c)
	}
	return out
}

func writeOpenGraphBehaviourFixture(outDir string) error {
	docs := append(append([]string{}, ogDocs...), ogRandomDocs()...)
	tokens := make([]ogTokenCase, 0, len(docs))
	for _, d := range docs {
		tokens = append(tokens, ogTokenCase{Doc: []byte(d), Tokens: ogTokenize([]byte(d))})
	}
	unesc := make([]ogUnescapeCase, 0, len(ogUnescapeCorpus))
	stdUnesc := make([]ogUnescapeCase, 0, len(ogUnescapeCorpus))
	for _, s := range ogUnescapeCorpus {
		unesc = append(unesc, ogUnescapeCase{In: []byte(s), Out: []byte(html.UnescapeString(s))})
		stdUnesc = append(stdUnesc, ogUnescapeCase{In: []byte(s), Out: []byte(stdhtml.UnescapeString(s))})
	}
	labels, err := ogLabels()
	if err != nil {
		return err
	}
	out := map[string]any{
		"tokens":       tokens,
		"unescape":     unesc,
		"std_unescape": stdUnesc,
		"labels":       labels,
		"charset":      ogCharsetCases(),
		"sweep":        ogSweep(),
		"times":        ogTimes(),
		"opengraph":    ogParseCases(),
		"oembed":       ogOEmbeds(),
		"endpoints":    ogEndpoints(),
	}
	blob, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_opengraph.json"), append(blob, '\n'), 0o644)
}
