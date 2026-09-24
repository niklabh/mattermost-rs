package main

// Behavioural oracle for Mattermost's outbound mail path, written to fixtures/behaviour_mail.json
// and consumed by `crates/gomail` (the generic ports) and `crates/mm-app/src/mail.rs` (the port of
// `platform/shared/mail/mail.go`).
//
// Run it alone with `-only mail`. What it records, top to bottom:
//
//   - `net/mail`: ParseAddress / ParseAddressList over valid and invalid input with Go's exact
//     error text, and `Address.String()` over hand-picked Name/Address pairs.
//   - `mime`: `WordEncoder.Encode` (Q and B, UTF-8 and not), `quotedprintable.Writer` fed as a
//     sequence of Write calls, `multipart.Writer` with a fixed boundary and `SetBoundary`'s
//     verdicts.
//   - `encoding/base64` StdEncoding decode errors (the SMTP AUTH challenge decoder).
//   - `net/textproto`: `Reader.ReadResponse` over raw server bytes, and the DATA dot-writer.
//   - `messages`: go-mail messages built by the **same call sequence as Mattermost's sendMail**
//     with a fixed date and message id, and crypto/rand.Reader swapped for a counter so the
//     multipart boundaries are known (recorded per message, in the order go-mail drew them).
//   - `smtp`: the real `mail.SendMailUsingConfig` / `SendMailWithEmbeddedFilesUsingConfig` /
//     `TestConnection` driven against an in-process SMTP sink whose behaviour is a JSON script
//     (recorded, so the Rust sink replays the same script). The sink records every byte the
//     client sent. `normaliseTranscript` below is the one normalisation rule and the Rust test
//     applies it identically.
//   - `dial`: the dial failures whose text reaches `app.admin.test_email.failure`.
//
// Determinism: fixed corpora, a fixed TLS key pair (sinkCertPEM, never regenerated), no clock in
// anything recorded un-normalised. Host-dependent rows (the resolver's name server) are marked
// `host_dependent`.

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"crypto/tls"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"mime"
	"mime/multipart"
	"mime/quotedprintable"
	"net"
	netmail "net/mail"
	"net/textproto"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/jaytaylor/html2text"
	gomail "github.com/wneessen/go-mail"

	mmmail "github.com/mattermost/mattermost/server/v8/platform/shared/mail"
)

// sinkCertPEM / sinkKeyPEM: a self-signed ECDSA P-256 pair for the TLS sink, valid 2020-2120 for
// `localhost` and 127.0.0.1. Generated once and pinned so the fixture never churns; the Rust sink
// serves the same pair (read from the fixture).
const sinkCertPEM = `-----BEGIN CERTIFICATE-----
MIIBaDCCAQ2gAwIBAgICEJIwCgYIKoZIzj0EAwIwGTEXMBUGA1UEAxMObW1ycy1z
bXRwLXNpbmswIBcNMjAwMTAxMDAwMDAwWhgPMjEyMDAxMDEwMDAwMDBaMBkxFzAV
BgNVBAMTDm1tcnMtc210cC1zaW5rMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE
hzXi4YeKruJXrqZeHagkSoBhcNzC4HTwaUcvvUE7jKHhUE+PZiTtMd4lTcQXGpTd
pCrmyBge6jIJlMai8Ulp0KNDMEEwDgYDVR0PAQH/BAQDAgeAMBMGA1UdJQQMMAoG
CCsGAQUFBwMBMBoGA1UdEQQTMBGCCWxvY2FsaG9zdIcEfwAAATAKBggqhkjOPQQD
AgNJADBGAiEA4nkxexM+UARdAQhdghnaeYZuzPd4VeI8iQEzGAWb4TgCIQDUH1tE
z5xadZPm2rGaGTWKYmWbgJzCACJfkEWIfE++IQ==
-----END CERTIFICATE-----
`

const sinkKeyPEM = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg3mGOSejJF7CaDmbI
FBAUcHucE3+Mv7W2rnV63nYNmjmhRANCAASHNeLhh4qu4leupl4dqCRKgGFw3MLg
dPBpRy+9QTuMoeFQT49mJO0x3iVNxBcalN2kKubIGB7qMgmUxqLxSWnQ
-----END PRIVATE KEY-----
`

func mailErr(err error) any {
	if err == nil {
		return nil
	}
	return err.Error()
}

// ---------------------------------------------------------------------------------------------
// net/mail

var mailAddressCorpus = []string{
	"user@example.com",
	"  user@example.com  ",
	"<user@example.com>",
	"John Doe <john@example.com>",
	"\"John Doe\" <john@example.com>",
	"\"Doe, John\" <john@example.com>",
	"\"J\\\"D\" <jd@example.com>",
	"john@example.com (John Doe)",
	"john@example.com (John (nested) Doe)",
	"john@example.com (unclosed",
	"John <john@example.com> (comment)",
	"John <john@example.com> (bad",
	"=?utf-8?q?J=C3=B6rg_Doe?= <joerg@example.com>",
	"=?UTF-8?B?SsO2cmc=?= <joerg@example.com>",
	"=?utf-8?q?J=C3=B6rg?= =?utf-8?q?_Doe?= <joerg@example.com>",
	"=?iso-8859-1?q?J=F6rg?= <joerg@example.com>",
	"=?us-ascii?q?J=F6rg?= <joerg@example.com>",
	"=?koi8-r?q?abc?= <joerg@example.com>",
	"Hello =?koi8-r?q?abc?= <joerg@example.com>",
	"=?utf-8?x?abc?= <joerg@example.com>",
	"=?utf-8?q?bad=ZZ?= <joerg@example.com>",
	"john@example.com (=?utf-8?q?J=C3=B6rg?= Doe)",
	"john@example.com (=?koi8-r?q?abc?=)",
	"Jörg <joerg@example.com>",
	"jörg@exämple.com",
	"\"quoted local\"@example.com",
	"\"\"@example.com",
	"\"unclosed@example.com",
	"\"bad\x01char\"@example.com",
	"\"esc\\\x01\"@example.com",
	"\"esc\\q\"@example.com",
	"user@[127.0.0.1]",
	"user@[::1]",
	"user@[IPv6:::1]",
	"user@[300.0.0.1]",
	"user@[127.0.0.1",
	"user@[127.0.\\0.1]",
	"user@[1.2.3.4]]",
	".user@example.com",
	"us..er@example.com",
	"user.@example.com",
	"user@.example.com",
	"user@example..com",
	"user@example.com.",
	"user@",
	"@example.com",
	"user",
	"user example",
	"John Doe",
	"John Doe <john@example.com",
	"John Doe john@example.com>",
	"<>",
	"",
	"   ",
	"a@b, c@d",
	"a@b,",
	", a@b",
	"a@b,,c@d",
	"a@b c@d",
	"group: a@b, c@d;",
	"group:;",
	"group: a@b;",
	"group: a@b",
	"group: a@b; (comment)",
	"group: a@b; (bad",
	"group: (c) a@b, c@d ;",
	"a@b, group: c@d, e@f;, g@h",
	"John Doe <john@example.com>, Jane <jane@example.com>",
	"John\tDoe <john@example.com>",
	"\"John\tDoe\" <john@example.com>",
	"John \"Q\" Doe <john@example.com>",
	"John.Doe <john@example.com>",
	"John..Doe <john@example.com>",
	".John <john@example.com>",
	"John@Doe <john@example.com>",
	"John: Doe <john@example.com>",
	"<john@example.com> trailing",
	"john@example.com trailing",
	"john@example.com, (comment) jane@example.com",
	"john@example.com (comment), jane@example.com",
	"\xff@example.com",
	"\"\xff\"@example.com",
	"user@\xff.com",
	"user@[\xff]",
	"A\xffB <a@b>",
	"🙂 <smile@example.com>",
	"test+tag@sub.example.co.uk",
	"a!#$%&'*+-/=?^_`{|}~@example.com",
	"x@y (a\\)b)",
	"John <\"bad\x01\"@example.com>",
	"John <\"unclosed@example.com>",
	"John <\"\"@example.com>",
	"x@y (a\\",
}

type addrOut struct {
	Name    string `json:"name"`
	Address string `json:"address"`
	String  string `json:"string"`
}

func addrsOut(list []*netmail.Address) []addrOut {
	out := []addrOut{}
	for _, a := range list {
		out = append(out, addrOut{a.Name, a.Address, a.String()})
	}
	return out
}

func mailParseCorpus() []map[string]any {
	var rows []map[string]any
	for _, in := range mailAddressCorpus {
		a, err := netmail.ParseAddress(in)
		row := map[string]any{"input": in, "single_error": mailErr(err)}
		if err == nil {
			row["single"] = addrsOut([]*netmail.Address{a})[0]
		}
		list, lerr := netmail.ParseAddressList(in)
		row["list_error"] = mailErr(lerr)
		if lerr == nil {
			row["list"] = addrsOut(list)
		}
		rows = append(rows, row)
	}
	return rows
}

var addressStringCorpus = []netmail.Address{
	{"", "user@example.com"},
	{"", ""},
	{"", "noat"},
	{"", "a@b@c"},
	{"", ".lead@example.com"},
	{"", "trail.@example.com"},
	{"", "do..ts@example.com"},
	{"", "a.b@example.com"},
	{"", "sp ace@example.com"},
	{"", "quo\"te@example.com"},
	{"", "back\\slash@example.com"},
	{"", "ctl\x01@example.com"},
	{"", "jörg@example.com"},
	{"", "@"},
	{"Plain", "p@example.com"},
	{"John Doe", "john@example.com"},
	{"Doe, John", "john@example.com"},
	{"J \"Q\" D", "jqd@example.com"},
	{"back\\slash", "b@example.com"},
	{"tab\there", "t@example.com"},
	{"ctl\x01char", "c@example.com"},
	{"Jörg", "joerg@example.com"},
	{"Jörg Doe", "joerg@example.com"},
	{"Jörg, Doe", "joerg@example.com"},
	{"Jörg.Doe", "joerg@example.com"},
	{"Jörg (x)", "joerg@example.com"},
	{"Mattermost Notifications ✉", "feedback@example.com"},
	{"Ünïcödé Ñame That Is Rather Long And Needs Several Encoded Words To Fit Everything In", "u@example.com"},
	{"Ünïcödé, Ñame That Is Rather Long And Needs Several Encoded Words To Fit Everything In", "u@example.com"},
	{"日本語の名前", "jp@example.com"},
	{"new\nline", "n@example.com"},
	{"", "user@[127.0.0.1]"},
}

func addressStringRows() []map[string]any {
	var rows []map[string]any
	for _, a := range addressStringCorpus {
		a := a
		rows = append(rows, map[string]any{"name": a.Name, "address": a.Address, "string": a.String()})
	}
	return rows
}

// ---------------------------------------------------------------------------------------------
// mime word encoding

var wordEncodeInputs = []string{
	"",
	"plain ascii",
	"tab\tinside",
	"ctl\x01",
	"del\x7f",
	"Jörg",
	"a=b?c_d e",
	"Ünïcödé Ñame That Is Rather Long And Needs Several Encoded Words To Fit Everything In",
	strings.Repeat("é", 40),
	strings.Repeat("a", 60) + "é" + strings.Repeat("b", 30),
	strings.Repeat("a", 62) + "日本" + strings.Repeat("b", 10),
	strings.Repeat("a", 63) + "é",
	strings.Repeat("a", 64) + "é",
	strings.Repeat("x", 44) + "é",
	strings.Repeat("x", 45) + "é" + strings.Repeat("x", 3),
	strings.Repeat("x", 46) + "é",
	strings.Repeat("x", 47) + "日本語",
	strings.Repeat("日本語", 30),
	"🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂",
	"[Mattermost] Notification in Tëam Nàme from someone with a long display name",
	"<abc123-1700000000@exämple.com>",
	"\xff",
}

func wordEncodeRows() []map[string]any {
	var rows []map[string]any
	for _, charset := range []string{"UTF-8", "utf-8", "ISO-8859-1"} {
		for _, in := range wordEncodeInputs {
			rows = append(rows, map[string]any{
				"charset": charset,
				"input":   in,
				"q":       mime.QEncoding.Encode(charset, in),
				"b":       mime.BEncoding.Encode(charset, in),
			})
		}
	}
	return rows
}

// ---------------------------------------------------------------------------------------------
// quoted-printable

var qpCorpus = [][]string{
	{""},
	{"hello"},
	{"hello\r\nworld"},
	{"hello\nworld"},
	{"hello\rworld"},
	{"a\r", "\nb"},
	{"a\r\r\nb"},
	{"a\n\rb"},
	{"trailing space "},
	{"trailing tab\t"},
	{"space before newline \r\nnext"},
	{"tab before lf\t\nnext"},
	{"eq=sign"},
	{"héllo wörld"},
	{strings.Repeat("a", 75)},
	{strings.Repeat("a", 76)},
	{strings.Repeat("a", 77)},
	{strings.Repeat("a", 150)},
	{strings.Repeat("a", 73) + "="},
	{strings.Repeat("a", 72) + "="},
	{strings.Repeat("a", 74) + "é"},
	{strings.Repeat("a", 75) + " "},
	{strings.Repeat("a", 74) + " b"},
	{strings.Repeat("a", 75) + "\r\n"},
	{strings.Repeat("é", 40)},
	{"split ", "write"},
	{strings.Repeat("a", 40), strings.Repeat("b", 40)},
	{"\x00\x01\x7f\xff"},
	{"line1\r\n\r\nline3\r\n"},
	{"ends with lf\n"},
	{" \t "},
	{".leading dot\r\n.another"},
	{"From here\r\nFrom there"},
}

func qpRows() []map[string]any {
	var rows []map[string]any
	for _, writes := range qpCorpus {
		var buf bytes.Buffer
		w := quotedprintable.NewWriter(&buf)
		for _, s := range writes {
			if _, err := w.Write([]byte(s)); err != nil {
				panic(err)
			}
		}
		if err := w.Close(); err != nil {
			panic(err)
		}
		rows = append(rows, map[string]any{"writes": writes, "output": buf.String()})
	}
	return rows
}

// ---------------------------------------------------------------------------------------------
// multipart

func multipartRows() map[string]any {
	var setBoundary []map[string]any
	for _, b := range []string{
		"", "a", strings.Repeat("x", 70), strings.Repeat("x", 71), "abc def", "abc ", " abc",
		"abc\"", "abc'()+_,-./:=?", "abc@", "abc<", "héllo", "abc\t",
	} {
		w := multipart.NewWriter(io.Discard)
		err := w.SetBoundary(b)
		setBoundary = append(setBoundary, map[string]any{"boundary": b, "error": mailErr(err)})
	}
	// SetBoundary after a part was created.
	{
		w := multipart.NewWriter(io.Discard)
		_, _ = w.CreatePart(textproto.MIMEHeader{})
		err := w.SetBoundary("late")
		setBoundary = append(setBoundary, map[string]any{"boundary": "late", "after_write": true, "error": mailErr(err)})
	}

	type mpPart struct {
		Header map[string][]string `json:"header"`
		Body   string              `json:"body"`
	}
	type mpCase struct {
		Boundary string   `json:"boundary"`
		Parts    []mpPart `json:"parts"`
	}
	cases := []mpCase{
		{"b1", nil},
		{"b2", []mpPart{{map[string][]string{}, "only"}}},
		{"b3", []mpPart{
			{map[string][]string{"Content-Type": {"text/plain"}, "A-First": {"1", "2"}, "Zed": {"z"}}, "one"},
			{map[string][]string{"content-type": {"text/html"}}, ""},
		}},
		{"b4", []mpPart{{map[string][]string{"Content-Type": {"multipart/alternative;\r\n boundary=inner"}}, "x\r\n"}}},
	}
	var outputs []map[string]any
	for _, c := range cases {
		var buf bytes.Buffer
		w := multipart.NewWriter(&buf)
		if err := w.SetBoundary(c.Boundary); err != nil {
			panic(err)
		}
		for _, p := range c.Parts {
			pw, err := w.CreatePart(textproto.MIMEHeader(p.Header))
			if err != nil {
				panic(err)
			}
			io.WriteString(pw, p.Body)
		}
		if err := w.Close(); err != nil {
			panic(err)
		}
		outputs = append(outputs, map[string]any{"boundary": c.Boundary, "parts": c.Parts, "output": buf.String()})
	}
	return map[string]any{"set_boundary": setBoundary, "writes": outputs}
}

// ---------------------------------------------------------------------------------------------
// base64 (SMTP AUTH challenges)

func base64Rows() []map[string]any {
	var rows []map[string]any
	for _, in := range []string{
		"", "VXNlcm5hbWU6", "UGFzc3dvcmQ6", "YQ==", "YWI=", "YWJj", "YQ", "YQ=", "Y", "Y===",
		"YQ==YQ==", "YQ==\r\n", "YQ=\r\n=", "Y!==", "!!!!", "VXNl\r\ncm5h", "VXNlcm5hbWU6x",
		"YWJjZGVmZ2hpams=", "YWJjZGVm!2hpams=", "YWJjZGVmZ2hp!ms=", "=", "==", "YQ==x", "YWI=x",
		"YR==", "YWJ=",
	} {
		out, err := base64.StdEncoding.DecodeString(in)
		row := map[string]any{"input": in, "error": mailErr(err)}
		if err == nil {
			row["output"] = string(out)
		}
		rows = append(rows, row)
	}
	return rows
}

// ---------------------------------------------------------------------------------------------
// textproto

func readResponseRows() []map[string]any {
	type rc struct {
		in     string
		expect int
	}
	var rows []map[string]any
	for _, c := range []rc{
		{"220 ready\r\n", 220},
		{"220 ready\n", 220},
		{"220 ready\r\n", 250},
		{"554 go away\r\n", 220},
		{"220-first\r\n220 second\r\n", 220},
		{"250-sink\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250 AUTH PLAIN LOGIN\r\n", 250},
		{"550-first\r\n550 second\r\n", 250},
		{"550-first\r\n551 second\r\n550 third\r\n", 250},
		{"250-first\r\nnonsense\r\n250 last\r\n", 250},
		{"251 ok\r\n", 25},
		{"252 ok\r\n", 25},
		{"261 ok\r\n", 25},
		{"354 go\r\n", 354},
		{"334 VXNlcm5hbWU6\r\n", 0},
		{"235 ok\r\n", 0},
		{"22\r\n", 220},
		{"220\r\n", 220},
		{"220x ok\r\n", 220},
		{"abc ok\r\n", 220},
		{"099 low\r\n", 220},
		{"220 \r\n", 220},
		{"", 220},
		{"220 no newline", 220},
		{"220-cont then eof\r\n", 220},
		{"220 cr\rinside\r\n", 220},
		{"-20 neg\r\n", 220},
		{"+20 plus\r\n", 220},
		{"2 0 sp\r\n", 220},
		{"5\r\n", 2},
		{"250 ok\r\n", 2},
		{"450 no\r\n", 2},
	} {
		r := textproto.NewReader(bufio.NewReader(strings.NewReader(c.in)))
		code, msg, err := r.ReadResponse(c.expect)
		rows = append(rows, map[string]any{"input": c.in, "expect": c.expect, "code": code, "message": msg, "error": mailErr(err)})
	}
	return rows
}

func dotWriterRows() []map[string]any {
	var rows []map[string]any
	for _, writes := range [][]string{
		{""},
		{"hello"},
		{"hello\r\n"},
		{"hello\n"},
		{"hello\r"},
		{".dot"},
		{"a\r\n.b\r\n..c\r\n"},
		{"a\n.b"},
		{"a\r", "\n.b"},
		{"a\r\r\nb"},
		{"a\rb"},
		{"a\r\n", ".", "b"},
		{".\r\n"},
		{"\r\n.\r\n"},
		{"x\r\n\r\n"},
	} {
		var buf bytes.Buffer
		bw := bufio.NewWriter(&buf)
		w := textproto.NewWriter(bw)
		d := w.DotWriter()
		for _, s := range writes {
			io.WriteString(d, s)
		}
		d.Close()
		rows = append(rows, map[string]any{"writes": writes, "output": buf.String()})
	}
	return rows
}

// ---------------------------------------------------------------------------------------------
// go-mail messages in sendMail's call sequence

// countingRand replaces crypto/rand.Reader while the message corpus renders, so every
// multipart boundary is known. Each Read is logged as the hex go-mail's boundary will be.
type countingRand struct {
	next byte
	log  []string
}

func (r *countingRand) Read(p []byte) (int, error) {
	for i := range p {
		p[i] = r.next
		r.next += 7
	}
	r.log = append(r.log, hex.EncodeToString(p))
	return len(p), nil
}

type msgInput struct {
	Name         string        `json:"name"`
	FeedbackName string        `json:"feedback_name"`
	FeedbackMail string        `json:"feedback_email"`
	ReplyTo      string        `json:"reply_to_address"`
	To           string        `json:"to"`
	Cc           string        `json:"cc"`
	Subject      string        `json:"subject"`
	HTML         string        `json:"html_body"`
	MessageID    string        `json:"message_id"`
	InReplyTo    string        `json:"in_reply_to"`
	References   string        `json:"references"`
	Category     string        `json:"category"`
	Embedded     []embeddedOut `json:"embedded"`
	DateUnix     int64         `json:"date_unix"`
	DateOffset   int           `json:"date_offset_seconds"`
}

type embeddedOut struct {
	Name    string `json:"name"`
	Content string `json:"content_base64"`
}

// buildLikeSendMail is mail.go:sendMail (from the m := gomail.NewMsg() line to the embeds),
// statement for statement, with the clock and message id supplied.
func buildLikeSendMail(in msgInput, text string) (*gomail.Msg, error) {
	from := netmail.Address{Name: in.FeedbackName, Address: in.FeedbackMail}
	replyTo := netmail.Address{Name: in.FeedbackName, Address: in.ReplyTo}
	m := gomail.NewMsg()
	m.SetAddrHeaderFromMailAddress(gomail.HeaderFrom, &from)
	if err := m.SetAddrHeader(gomail.HeaderTo, in.To); err != nil {
		return nil, err
	}
	m.SetGenHeader(gomail.HeaderSubject, in.Subject)
	m.SetGenHeader(gomail.Header("Content-Transfer-Encoding"), "8bit")
	m.SetGenHeader(gomail.Header("Auto-Submitted"), "auto-generated")
	m.SetGenHeader(gomail.Header("Precedence"), "bulk")
	if in.Category != "" {
		m.SetGenHeader(gomail.Header(mmmail.SendGridXSMTPAPIHeader), fmt.Sprintf(`{"category": %q}`, in.Category))
	}
	if replyTo.Address != "" {
		m.SetAddrHeaderFromMailAddress(gomail.HeaderReplyTo, &replyTo)
	}
	if in.Cc != "" {
		if err := m.SetAddrHeader(gomail.HeaderCc, in.Cc); err != nil {
			return nil, err
		}
	}
	m.SetGenHeader(gomail.HeaderMessageID, in.MessageID)
	if in.InReplyTo != "" {
		m.SetGenHeaderPreformatted(gomail.HeaderInReplyTo, in.InReplyTo)
	}
	if in.References != "" {
		m.SetGenHeaderPreformatted(gomail.HeaderReferences, in.References)
	}
	m.SetDateWithValue(time.Unix(in.DateUnix, 0).In(time.FixedZone("", in.DateOffset)))
	m.SetBodyString(gomail.TypeTextPlain, text)
	m.AddAlternativeString(gomail.TypeTextHTML, in.HTML)
	for _, e := range in.Embedded {
		content, _ := base64.StdEncoding.DecodeString(e.Content)
		if err := m.EmbedReader(e.Name, bytes.NewReader(content)); err != nil {
			return nil, err
		}
	}
	return m, nil
}

var longHTML = "<html><body><h1>Hello</h1><p>" + strings.Repeat("Some text with = signs and trailing spaces   \n", 5) +
	"</p><p>Ünïcödé line " + strings.Repeat("é", 60) + "</p><p>" + strings.Repeat("x", 200) + "</p>\n.\n.dot line\r\n</body></html>"

func pngBytes(n int) string {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte(i * 31)
	}
	copy(b, []byte("\x89PNG\r\n\x1a\n"))
	return base64.StdEncoding.EncodeToString(b)
}

var messageCorpus = []msgInput{
	{Name: "minimal", FeedbackName: "Mattermost", FeedbackMail: "feedback@example.com", To: "user@example.com",
		Subject: "Hello", HTML: "<p>Hi</p>", MessageID: "<abc-1@example.com>", DateUnix: 1758700000, DateOffset: 19800},
	{Name: "empty_feedback", To: "user@example.com", Subject: "", HTML: "", MessageID: "<empty@x>", DateUnix: 1, DateOffset: 0},
	{Name: "everything", FeedbackName: "Mattermost Notifications", FeedbackMail: "no-reply@example.com",
		ReplyTo: "reply@example.com", To: "\"Doe, John\" <john@example.com>", Cc: "Jane <jane@example.com>",
		Subject: "[Mattermost] Notification in Team Name from someone with a rather long display name here",
		HTML:    longHTML, MessageID: "<x7k2mfy8nqwe3tbu-1758700000@chat.example.com>", InReplyTo: "<parent@example.com>",
		References: "<root@example.com> <parent@example.com>", Category: "Notification", DateUnix: 1758700000, DateOffset: -25200},
	{Name: "unicode_subject_and_name", FeedbackName: "Ünïcödé Ñame", FeedbackMail: "u@example.com", ReplyTo: "r@example.com",
		To: "Jörg <joerg@example.com>", Subject: "Ünïcödé subject that is long enough to need more than one encoded word — really",
		HTML: "<p>ü</p>", MessageID: "<m@example.com>", DateUnix: 1700000000, DateOffset: 3600},
	{Name: "comma_name", FeedbackName: "Doe, Feedback", FeedbackMail: "f@example.com", ReplyTo: "r@example.com",
		To: "t@example.com", Subject: "s", HTML: "h", MessageID: "<id@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "long_unbroken_subject", FeedbackMail: "f@example.com", To: "t@example.com",
		Subject: strings.Repeat("A", 100) + " " + strings.Repeat("b", 10) + " " + strings.Repeat("c", 58) + " d",
		HTML:    "x", MessageID: "<id@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "fold_boundaries", FeedbackMail: "f@example.com", To: "t@example.com",
		Subject: strings.Repeat("word ", 30) + strings.Repeat("y", 60) + " " + strings.Repeat("z", 61) + " " + strings.Repeat("w", 62),
		HTML:    "x", MessageID: "<id@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "in_reply_to_only", FeedbackMail: "f@example.com", To: "t@example.com", Subject: "Re: thread",
		HTML: "<b>b</b>", MessageID: "<id@h>", InReplyTo: "<p@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "references_only", FeedbackMail: "f@example.com", To: "t@example.com", Subject: "Re: thread",
		HTML: "<b>b</b>", MessageID: "<id@h>", References: "<r1@h> <r2@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "category_quotes", FeedbackMail: "f@example.com", To: "t@example.com", Subject: "c",
		HTML: "c", MessageID: "<id@h>", Category: "Weird \"cat\"\tégory", DateUnix: 1700000000, DateOffset: 0},
	{Name: "one_embed", FeedbackName: "MM", FeedbackMail: "f@example.com", ReplyTo: "r@example.com", To: "t@example.com",
		Subject: "With avatar", HTML: "<img src=\"cid:user-avatar.png\">", MessageID: "<id@h>",
		Embedded: []embeddedOut{{"user-avatar.png", pngBytes(200)}}, DateUnix: 1700000000, DateOffset: 19800},
	{Name: "three_embeds", FeedbackMail: "f@example.com", To: "t@example.com", Subject: "batch",
		HTML: "<p>batch</p>", MessageID: "<id@h>",
		Embedded: []embeddedOut{{"user-avatar-0.png", pngBytes(57)}, {"weird:na\"me/<x>?.PNG", pngBytes(1)}, {"noext", ""}},
		DateUnix: 1700000000, DateOffset: 19800},
	{Name: "embed_exact_line", FeedbackMail: "f@example.com", To: "t@example.com", Subject: "b64",
		HTML: "<p>b64</p>", MessageID: "<id@h>", Embedded: []embeddedOut{{"a.png", pngBytes(57)}, {"b.jpg", pngBytes(114)}, {"c.gif", pngBytes(58)}},
		DateUnix: 1700000000, DateOffset: 19800},
	{Name: "unicode_msgid_and_group_to", FeedbackMail: "f@example.com", To: "undisclosed: t@example.com;",
		Subject: "g", HTML: "g", MessageID: "<ü@exämple.com>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "quoted_local_to", FeedbackMail: "f@example.com", To: "\"john doe\"@example.com",
		Cc: "x@[127.0.0.1]", Subject: "q", HTML: "q", MessageID: "<id@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "crlf_in_subject", FeedbackMail: "f@example.com", To: "t@example.com",
		Subject: "line1\r\nInjected: header", HTML: "q", MessageID: "<id@h>", DateUnix: 1700000000, DateOffset: 0},
	{Name: "long_address_list_name", FeedbackName: strings.Repeat("Name ", 20), FeedbackMail: "f@example.com",
		ReplyTo: "r@example.com", To: strings.Repeat("n", 70) + "@example.com", Subject: "q", HTML: "q",
		MessageID: "<id@h>", DateUnix: 1700000000, DateOffset: 0},
}

func messageRows() ([]map[string]any, error) {
	orig := rand.Reader
	defer func() { rand.Reader = orig }()

	var rows []map[string]any
	for _, in := range messageCorpus {
		text, terr := html2text.FromString(in.HTML)
		if terr != nil {
			text = ""
		}
		var out string
		var boundaries []string
		// In-Reply-To and References are go-mail's *preformatted* headers, written in Go map order.
		// Render until the order is the sorted one the Rust port always writes; see
		// gomail::msg for why a reader cannot tell the two apart.
		for attempt := 0; ; attempt++ {
			cr := &countingRand{next: 1}
			rand.Reader = cr
			m, err := buildLikeSendMail(in, text)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", in.Name, err)
			}
			var buf bytes.Buffer
			if _, err := m.WriteTo(&buf); err != nil {
				return nil, fmt.Errorf("%s: WriteTo: %w", in.Name, err)
			}
			out = buf.String()
			boundaries = cr.log
			irt := strings.Index(out, "\r\nIn-Reply-To: ")
			ref := strings.Index(out, "\r\nReferences: ")
			if irt < 0 || ref < 0 || irt < ref {
				break
			}
			if attempt > 1000 {
				return nil, fmt.Errorf("%s: preformatted header order never sorted", in.Name)
			}
		}
		rows = append(rows, map[string]any{"input": in, "text": text, "boundaries": boundaries, "output": out})
	}
	return rows, nil
}

// ---------------------------------------------------------------------------------------------
// the SMTP sink

// sinkScript is the whole behaviour of one sink session. The Rust sink implements the same
// semantics; keep the two in step.
//
//   - Greeting is written on accept ("" writes nothing and waits for the client to hang up).
//   - Each client line is dispatched on its verb: the first space-separated word, upper-cased.
//     A line that is exactly "*" is verb "*". A line read while an AUTH exchange is in progress
//     (the previous reply began "334") is a continuation and is answered from AuthReplies, as is
//     the AUTH line itself; everything else is answered from Replies[verb], defaulting to
//     "502 5.5.2 not implemented\r\n".
//   - CloseOn: when that verb arrives the sink records the line and hangs up without replying.
//     "GREETING" hangs up straight after writing the greeting.
//   - DATA: a reply beginning "354" makes the sink read the body up to and including
//     "\r\n.\r\n", then answer Replies["DATA_END"] (default "250 2.0.0 queued\r\n").
//   - STARTTLS: a reply beginning "220" upgrades the connection with the pinned pair; the
//     transcript gets the marker "[STARTTLS]".
//   - TLS: the listener itself is TLS (implicit TLS / ConnectionSecurity "TLS").
type sinkScript struct {
	TLS         bool              `json:"tls"`
	Greeting    string            `json:"greeting"`
	Replies     map[string]string `json:"replies"`
	AuthReplies []string          `json:"auth_replies"`
	CloseOn     string            `json:"close_on"`
}

func (s sinkScript) reply(verb string) string {
	if r, ok := s.Replies[verb]; ok {
		return r
	}
	if verb == "DATA_END" {
		return "250 2.0.0 queued\r\n"
	}
	return "502 5.5.2 not implemented\r\n"
}

func sinkTLSConfig() *tls.Config {
	cert, err := tls.X509KeyPair([]byte(sinkCertPEM), []byte(sinkKeyPEM))
	if err != nil {
		panic(err)
	}
	return &tls.Config{Certificates: []tls.Certificate{cert}}
}

// runSink serves exactly one connection and returns the client's bytes.
func runSink(ln net.Listener, s sinkScript, done chan<- string) {
	var transcript bytes.Buffer
	defer func() { done <- transcript.String() }()
	conn, err := ln.Accept()
	if err != nil {
		return
	}
	defer conn.Close()
	var c net.Conn = conn
	if s.TLS {
		tc := tls.Server(conn, sinkTLSConfig())
		if err := tc.Handshake(); err != nil {
			transcript.WriteString("[TLS HANDSHAKE FAILED]")
			return
		}
		c = tc
	}
	r := bufio.NewReader(c)
	if s.Greeting == "" {
		io.Copy(io.Discard, r)
		return
	}
	io.WriteString(c, s.Greeting)
	if s.CloseOn == "GREETING" {
		return
	}
	authQueue := append([]string(nil), s.AuthReplies...)
	inAuth := false
	nextAuth := func() string {
		if len(authQueue) == 0 {
			return "535 5.7.8 no more auth replies\r\n"
		}
		r := authQueue[0]
		authQueue = authQueue[1:]
		return r
	}
	for {
		line, err := r.ReadString('\n')
		transcript.WriteString(line)
		if err != nil {
			return
		}
		trimmed := strings.TrimRight(line, "\r\n")
		verb := strings.ToUpper(strings.SplitN(trimmed, " ", 2)[0])
		if trimmed == "*" {
			verb = "*"
		}
		if verb == s.CloseOn && s.CloseOn != "" {
			return
		}
		var reply string
		switch {
		case verb == "*":
			inAuth = false
			reply = s.reply("*")
		case inAuth || verb == "AUTH":
			reply = nextAuth()
			inAuth = strings.HasPrefix(reply, "334")
		default:
			reply = s.reply(verb)
		}
		io.WriteString(c, reply)
		if verb == "DATA" && strings.HasPrefix(reply, "354") {
			var body bytes.Buffer
			for !bytes.HasSuffix(body.Bytes(), []byte("\r\n.\r\n")) {
				l, err := r.ReadString('\n')
				body.WriteString(l)
				if err != nil {
					transcript.Write(body.Bytes())
					return
				}
			}
			transcript.Write(body.Bytes())
			io.WriteString(c, s.reply("DATA_END"))
		}
		if verb == "STARTTLS" && strings.HasPrefix(reply, "220") {
			tc := tls.Server(c, sinkTLSConfig())
			if err := tc.Handshake(); err != nil {
				transcript.WriteString("[STARTTLS HANDSHAKE FAILED]")
				return
			}
			transcript.WriteString("[STARTTLS]")
			c = tc
			r = bufio.NewReader(c)
		}
	}
}

var (
	boundaryRe = regexp.MustCompile(`[0-9a-f]{60}`)
	dateRe     = regexp.MustCompile(`(?m)^Date: [^\r\n]*\r$`)
	msgIDRe    = regexp.MustCompile(`<[ybndrfg8ejkmcpqxot1uwisza345h769]{16}-[0-9]+@`)
)

// normaliseTranscript is the documented normalisation rule for the real-thing transcripts:
//
//  1. every run of exactly 60 lower-case hex digits (a multipart boundary drawn from
//     crypto/rand) becomes BOUNDARY<n>, numbered by first appearance from 1;
//  2. every line "Date: …" becomes "Date: DATE";
//  3. a generated message id "<{16 z-base-32}-{unix}@" becomes "<RANDOM-UNIX@";
//  4. a transcript that starts with a TLS handshake record (0x16 0x03: a client that spoke TLS
//     to a plaintext sink) becomes "[TLS CLIENT HELLO]" — the hello is random.
//
// Nothing else in the transcript is random.
func normaliseTranscript(s string) string {
	if strings.HasPrefix(s, "\x16\x03") {
		return "[TLS CLIENT HELLO]"
	}
	seen := map[string]string{}
	s = boundaryRe.ReplaceAllStringFunc(s, func(b string) string {
		if n, ok := seen[b]; ok {
			return n
		}
		n := fmt.Sprintf("BOUNDARY%d", len(seen)+1)
		seen[b] = n
		return n
	})
	s = dateRe.ReplaceAllString(s, "Date: DATE\r")
	return msgIDRe.ReplaceAllString(s, "<RANDOM-UNIX@")
}

type smtpConfigOut struct {
	ConnectionSecurity                string `json:"connection_security"`
	SkipServerCertificateVerification bool   `json:"skip_server_certificate_verification"`
	Hostname                          string `json:"hostname"`
	ServerName                        string `json:"server_name"`
	Server                            string `json:"server"`
	Port                              string `json:"port"`
	ServerTimeout                     int    `json:"server_timeout"`
	Username                          string `json:"username"`
	Password                          string `json:"password"`
	EnableSMTPAuth                    bool   `json:"enable_smtp_auth"`
	FeedbackName                      string `json:"feedback_name"`
	FeedbackEmail                     string `json:"feedback_email"`
	ReplyToAddress                    string `json:"reply_to_address"`
}

func (c smtpConfigOut) toGo() *mmmail.SMTPConfig {
	return &mmmail.SMTPConfig{
		ConnectionSecurity:                c.ConnectionSecurity,
		SkipServerCertificateVerification: c.SkipServerCertificateVerification,
		Hostname:                          c.Hostname,
		ServerName:                        c.ServerName,
		Server:                            c.Server,
		Port:                              c.Port,
		ServerTimeout:                     c.ServerTimeout,
		Username:                          c.Username,
		Password:                          c.Password,
		EnableSMTPAuth:                    c.EnableSMTPAuth,
		SendEmailNotifications:            true,
		FeedbackName:                      c.FeedbackName,
		FeedbackEmail:                     c.FeedbackEmail,
		ReplyToAddress:                    c.ReplyToAddress,
	}
}

type smtpArgs struct {
	To         string        `json:"to"`
	Subject    string        `json:"subject"`
	HTML       string        `json:"html_body"`
	MessageID  string        `json:"message_id"`
	InReplyTo  string        `json:"in_reply_to"`
	References string        `json:"references"`
	Cc         string        `json:"cc"`
	Category   string        `json:"category"`
	Embedded   []embeddedOut `json:"embedded"`
}

type smtpCase struct {
	Name string `json:"name"`
	// Call is "send" (SendMailUsingConfig), "send_embedded" (SendMailWithEmbeddedFilesUsingConfig)
	// or "test" (TestConnection).
	Call   string        `json:"call"`
	Config smtpConfigOut `json:"config"`
	Args   smtpArgs      `json:"args"`
	Script sinkScript    `json:"script"`
	// NoSink: dial the config's Server:Port as given; no listener is started. Otherwise Server is
	// 127.0.0.1 and Port is the sink's.
	NoSink bool `json:"no_sink"`
}

const ehloFull = "250-sink.example\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-STARTTLS\r\n250 AUTH PLAIN LOGIN\r\n"

func baseConfig() smtpConfigOut {
	return smtpConfigOut{
		Hostname: "chat.example.com", ServerName: "127.0.0.1", Server: "127.0.0.1", ServerTimeout: 5,
		Username: "user", Password: "pass", FeedbackName: "Mattermost", FeedbackEmail: "feedback@example.com",
		ReplyToAddress: "reply@example.com",
	}
}

func baseArgs() smtpArgs {
	return smtpArgs{
		To: "user@example.com", Subject: "Test subject", HTML: "<h1>Hello</h1><p>World &amp; more</p>",
		MessageID: "<fixed-1@chat.example.com>", Category: "TestEmail",
	}
}

func smtpCases() []smtpCase {
	cfg := baseConfig
	args := baseArgs
	ok := func(extra map[string]string) sinkScript {
		r := map[string]string{
			"EHLO": ehloFull, "HELO": "250 sink.example\r\n", "MAIL": "250 2.1.0 ok\r\n",
			"RCPT": "250 2.1.5 ok\r\n", "DATA": "354 go ahead\r\n", "QUIT": "221 bye\r\n",
			"STARTTLS": "220 2.0.0 ready\r\n", "*": "501 5.0.0 cancelled\r\n",
		}
		for k, v := range extra {
			r[k] = v
		}
		return sinkScript{Greeting: "220 sink.example ESMTP\r\n", Replies: r}
	}
	with := func(c smtpConfigOut, f func(*smtpConfigOut)) smtpConfigOut { f(&c); return c }
	withA := func(a smtpArgs, f func(*smtpArgs)) smtpArgs { f(&a); return a }
	withS := func(s sinkScript, f func(*sinkScript)) sinkScript { f(&s); return s }

	return []smtpCase{
		{Name: "send_plain_full_ext", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) {
			a.Cc = "Jane <jane@example.com>"
			a.InReplyTo = "<parent@chat.example.com>"
		}), Script: ok(nil)},
		{Name: "send_references_only_generated_msgid", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) {
			a.MessageID = ""
			a.References = "<r1@x> <r2@x>"
			a.Category = ""
		}), Script: ok(nil)},
		{Name: "send_no_extensions", Call: "send", Config: cfg(), Args: args(), Script: ok(map[string]string{"EHLO": "250 sink.example\r\n"})},
		{Name: "send_only_8bitmime_lowercase_smtputf8", Call: "send", Config: cfg(), Args: args(),
			Script: ok(map[string]string{"EHLO": "250-sink.example\r\n250-8BITMIME\r\n250 smtputf8\r\n"})},
		{Name: "send_ehlo_rejected_helo_fallback", Call: "send", Config: cfg(), Args: args(),
			Script: ok(map[string]string{"EHLO": "502 5.5.2 no ehlo\r\n"})},
		{Name: "send_no_hostname_hello_in_mail", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.Hostname = "" }), Args: args(), Script: ok(nil)},
		{Name: "send_hello_both_rejected", Call: "send", Config: cfg(), Args: args(),
			Script: ok(map[string]string{"EHLO": "502 5.5.2 no ehlo\r\n", "HELO": "554 5.7.1 go away\r\n"})},
		{Name: "send_no_hostname_hello_both_rejected", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.Hostname = "" }), Args: args(),
			Script: ok(map[string]string{"EHLO": "502 5.5.2 no ehlo\r\n", "HELO": "554 5.7.1 go away\r\n"})},
		{Name: "send_greeting_rejected", Call: "send", Config: cfg(), Args: args(),
			Script: withS(ok(nil), func(s *sinkScript) { s.Greeting = "554 5.3.2 not accepting\r\n" })},
		{Name: "send_greeting_multiline_rejected", Call: "send", Config: cfg(), Args: args(),
			Script: withS(ok(nil), func(s *sinkScript) { s.Greeting = "554-first line\r\n554 second line\r\n" })},
		{Name: "send_greeting_garbage", Call: "send", Config: cfg(), Args: args(),
			Script: withS(ok(nil), func(s *sinkScript) { s.Greeting = "hello\r\n" })},
		{Name: "send_mail_rejected", Call: "send", Config: cfg(), Args: args(), Script: ok(map[string]string{"MAIL": "550 5.7.1 sender rejected\r\n"})},
		{Name: "send_rcpt_rejected_multiline", Call: "send", Config: cfg(), Args: args(),
			Script: ok(map[string]string{"RCPT": "550-5.1.1 no such user\r\n550 5.1.1 really\r\n"})},
		{Name: "send_rcpt_251_accepted", Call: "send", Config: cfg(), Args: args(), Script: ok(map[string]string{"RCPT": "251 2.1.5 forwarded\r\n"})},
		{Name: "send_data_rejected", Call: "send", Config: cfg(), Args: args(), Script: ok(map[string]string{"DATA": "554 5.5.1 no data\r\n"})},
		{Name: "send_data_end_rejected", Call: "send", Config: cfg(), Args: args(), Script: ok(map[string]string{"DATA_END": "552 5.3.4 too big\r\n"})},
		{Name: "send_server_hangs_up_at_mail", Call: "send", Config: cfg(), Args: args(), Script: withS(ok(nil), func(s *sinkScript) { s.CloseOn = "MAIL" })},
		{Name: "send_to_two_addresses", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.To = "a@example.com, b@example.com" }), Script: ok(nil)},
		{Name: "send_to_unparseable", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.To = "not an address" }), Script: ok(nil)},
		{Name: "send_to_trailing_comma", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.To = "a@example.com," }), Script: ok(nil)},
		{Name: "send_to_empty_group", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.To = "g:;" }), Script: ok(nil)},
		{Name: "send_cc_two_addresses", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.Cc = "a@example.com, b@example.com" }), Script: ok(nil)},
		{Name: "send_cc_unparseable", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.Cc = "<broken" }), Script: ok(nil)},
		{Name: "send_cc_trailing_comma", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.Cc = "c@example.com," }), Script: ok(nil)},
		{Name: "send_to_with_name_smtp_to_raw", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.To = "John <john@example.com>" }), Script: ok(nil)},
		{Name: "send_from_with_crlf", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.FeedbackEmail = "f@example.com\r\nRCPT TO:<x@y>" }), Args: args(), Script: ok(nil)},
		{Name: "send_hostname_with_bare_cr", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.Hostname = "a\rb" }), Args: args(), Script: ok(nil)},
		{Name: "send_hostname_with_crlf", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.Hostname = "a\nb" }), Args: args(), Script: ok(nil)},
		{Name: "send_no_reply_to_empty_feedback", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ReplyToAddress = ""
			c.FeedbackName = ""
		}), Args: args(), Script: ok(nil)},
		{Name: "send_empty_html", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) { a.HTML = "" }), Script: ok(nil)},
		{Name: "send_long_unicode_subject", Call: "send", Config: cfg(), Args: withA(args(), func(a *smtpArgs) {
			a.Subject = "[Mattermost] Ünïcödé notification from Jörg in «Tëam Nàme» — a subject long enough to fold"
			a.HTML = longHTML
		}), Script: ok(nil)},
		{Name: "send_embedded_one", Call: "send_embedded", Config: cfg(), Args: withA(args(), func(a *smtpArgs) {
			a.HTML = "<img src=\"cid:user-avatar.png\"><p>hi</p>"
			a.Embedded = []embeddedOut{{"user-avatar.png", pngBytes(300)}}
		}), Script: ok(nil)},
		{Name: "send_auth_over_plaintext", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.EnableSMTPAuth = true }), Args: args(), Script: ok(nil)},
		{Name: "send_auth_plaintext_no_hostname", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.EnableSMTPAuth = true
			c.Hostname = ""
		}), Args: args(), Script: ok(nil)},
		{Name: "send_tls_plain_auth", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(nil), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"235 2.7.0 accepted\r\n"}
		})},
		{Name: "send_tls_login_auth", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(map[string]string{"EHLO": "250-sink.example\r\n250 AUTH LOGIN CRAM-MD5\r\n"}), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"334 VXNlcm5hbWU6\r\n", "334 UGFzc3dvcmQ6\r\n", "235 2.7.0 accepted\r\n"}
		})},
		{Name: "send_tls_login_unknown_challenge", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(map[string]string{"EHLO": "250-sink.example\r\n250 AUTH LOGIN\r\n"}), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"334 V2hvIGFyZSB5b3U/\r\n"}
		})},
		{Name: "send_tls_login_bad_base64", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(map[string]string{"EHLO": "250-sink.example\r\n250 AUTH LOGIN\r\n"}), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"334 VXNl!m5hbWU6\r\n"}
		})},
		{Name: "send_tls_auth_rejected", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(nil), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"535 5.7.8 Authentication credentials invalid\r\n"}
		})},
		{Name: "send_tls_plain_auth_challenge", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(nil), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"334 \r\n"}
		})},
		{Name: "send_tls_auth_wrong_host_name", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
			c.ServerName = "localhost"
		}), Args: args(), Script: withS(ok(nil), func(s *sinkScript) { s.TLS = true })},
		{Name: "send_tls_unknown_authority", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.ConnectionSecurity = "TLS" }),
			Args: args(), Script: withS(ok(nil), func(s *sinkScript) { s.TLS = true })},
		{Name: "send_tls_wrong_name", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.ServerName = "wrong.example"
		}), Args: args(), Script: withS(ok(nil), func(s *sinkScript) { s.TLS = true })},
		{Name: "send_tls_wrong_ip", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.ServerName = "10.1.2.3"
		}), Args: args(), Script: withS(ok(nil), func(s *sinkScript) { s.TLS = true })},
		{Name: "send_tls_to_plaintext_server", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
		}), Args: args(), Script: ok(nil)},
		{Name: "send_starttls_login", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "STARTTLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Args: args(), Script: withS(ok(map[string]string{"EHLO": "250-sink.example\r\n250-STARTTLS\r\n250 AUTH LOGIN\r\n"}), func(s *sinkScript) {
			s.AuthReplies = []string{"334 VXNlcm5hbWU6\r\n", "334 UGFzc3dvcmQ6\r\n", "235 2.7.0 accepted\r\n"}
		})},
		{Name: "send_starttls_no_hostname", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "STARTTLS"
			c.SkipServerCertificateVerification = true
			c.Hostname = ""
		}), Args: args(), Script: ok(nil)},
		{Name: "send_starttls_rejected_continues_plain", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "STARTTLS"
			c.SkipServerCertificateVerification = true
		}), Args: args(), Script: ok(map[string]string{"STARTTLS": "454 4.7.0 TLS not available\r\n"})},
		{Name: "send_starttls_unknown_authority", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "STARTTLS"
		}), Args: args(), Script: ok(nil)},
		{Name: "test_ok", Call: "test", Config: cfg(), Script: ok(nil)},
		{Name: "test_no_hostname", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.Hostname = "" }), Script: ok(nil)},
		{Name: "test_hello_rejected", Call: "test", Config: cfg(), Script: ok(map[string]string{"EHLO": "500 no\r\n", "HELO": "501 still no\r\n"})},
		{Name: "test_greeting_rejected", Call: "test", Config: cfg(), Script: withS(ok(nil), func(s *sinkScript) { s.Greeting = "421 4.3.2 busy\r\n" })},
		{Name: "test_greeting_eof", Call: "test", Config: cfg(), Script: withS(ok(nil), func(s *sinkScript) { s.CloseOn = "GREETING"; s.Greeting = "220-partial\r\n" })},
		{Name: "test_auth_plaintext", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.EnableSMTPAuth = true }), Script: ok(nil)},
		{Name: "test_tls_auth_ok", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Script: withS(ok(nil), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"235 2.7.0 accepted\r\n"}
		})},
		{Name: "test_tls_auth_rejected", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "TLS"
			c.SkipServerCertificateVerification = true
			c.EnableSMTPAuth = true
		}), Script: withS(ok(nil), func(s *sinkScript) {
			s.TLS = true
			s.AuthReplies = []string{"535 5.7.8 bad credentials\r\n"}
		})},
		{Name: "test_tls_unknown_authority", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.ConnectionSecurity = "TLS" }),
			Script: withS(ok(nil), func(s *sinkScript) { s.TLS = true })},
		{Name: "test_starttls_ok", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ConnectionSecurity = "STARTTLS"
			c.SkipServerCertificateVerification = true
		}), Script: ok(nil)},
		{Name: "test_greeting_timeout", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.ServerTimeout = 1 }),
			Script: withS(ok(nil), func(s *sinkScript) { s.Greeting = "" })},
		{Name: "send_zero_timeout", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.ServerTimeout = 0 }), Args: args(),
			Script: withS(ok(nil), func(s *sinkScript) { s.Greeting = "" })},
		// A prompt greeting still loses to a zero timeout: the context is done before the read,
		// and success only cancels it — ctx.Err() stays DeadlineExceeded. Deterministic.
		{Name: "send_zero_timeout_prompt_greeting", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.ServerTimeout = 0 }), Args: args(), Script: ok(nil)},
		// A negative timeout is a dial deadline already in the past.
		{Name: "test_negative_timeout", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.ServerTimeout = -1
			c.Port = "1"
		}), NoSink: true},
		{Name: "send_empty_server", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.Server = "" }), Args: args(), NoSink: true},
		{Name: "test_empty_server", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = ""
			c.Port = "1"
		}), NoSink: true},
		{Name: "send_refused", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) { c.Port = "1" }), Args: args(), NoSink: true},
		{Name: "send_tls_refused", Call: "send", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Port = "1"
			c.ConnectionSecurity = "TLS"
		}), Args: args(), NoSink: true},
		{Name: "test_refused", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.Port = "1" }), NoSink: true},
		{Name: "test_refused_localhost", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "localhost"
			c.Port = "1"
		}), NoSink: true},
		{Name: "test_refused_ipv6", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "[::1]"
			c.Port = "1"
		}), NoSink: true},
		{Name: "test_empty_port", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.Port = "" }), NoSink: true},
		{Name: "test_port_out_of_range", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.Port = "99999" }), NoSink: true},
		{Name: "test_port_negative", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.Port = "-1" }), NoSink: true},
		{Name: "test_port_name_unknown", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) { c.Port = "nosuchservice" }), NoSink: true},
		{Name: "test_too_many_colons", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "::1"
			c.Port = "1"
		}), NoSink: true},
		{Name: "test_missing_bracket", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "[::1"
			c.Port = "1"
		}), NoSink: true},
		{Name: "test_empty_host", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = ""
			c.Port = "1"
		}), NoSink: true},
		{Name: "test_numeric_nonhost", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "256.1.1.1"
			c.Port = "25"
		}), NoSink: true},
		{Name: "test_space_host", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "exa mple.com"
			c.Port = "25"
		}), NoSink: true},
		{Name: "test_nxdomain", Call: "test", Config: with(cfg(), func(c *smtpConfigOut) {
			c.Server = "nonexistent.invalid"
			c.Port = "25"
		}), NoSink: true},
	}
}

// hostDependent names the rows whose error text depends on the machine's resolver.
var hostDependent = map[string]bool{"test_nxdomain": true, "test_refused_localhost": true}

func runSMTPCase(c smtpCase) (map[string]any, error) {
	cfg := c.Config
	done := make(chan string, 1)
	if !c.NoSink {
		var ln net.Listener
		var err error
		ln, err = net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			return nil, err
		}
		defer ln.Close()
		_, port, _ := net.SplitHostPort(ln.Addr().String())
		cfg.Port = port
		go runSink(ln, c.Script, done)
	}
	var err error
	goCfg := cfg.toGo()
	switch c.Call {
	case "send":
		a := c.Args
		err = mmmail.SendMailUsingConfig(a.To, a.Subject, a.HTML, goCfg, true, a.MessageID, a.InReplyTo, a.References, a.Cc, a.Category)
	case "send_embedded":
		a := c.Args
		files := map[string]io.Reader{}
		for _, e := range a.Embedded {
			b, _ := base64.StdEncoding.DecodeString(e.Content)
			files[e.Name] = bytes.NewReader(b)
		}
		err = mmmail.SendMailWithEmbeddedFilesUsingConfig(a.To, a.Subject, a.HTML, files, goCfg, true, a.MessageID, a.InReplyTo, a.References, a.Cc, a.Category)
	case "test":
		err = mmmail.TestConnection(goCfg)
	}
	transcript := ""
	if !c.NoSink {
		select {
		case transcript = <-done:
		case <-time.After(10 * time.Second):
			return nil, fmt.Errorf("%s: sink never finished", c.Name)
		}
	}
	text, terr := html2text.FromString(c.Args.HTML)
	if terr != nil {
		text = ""
	}
	return map[string]any{
		"name": c.Name, "call": c.Call, "config": c.Config, "args": c.Args, "script": c.Script,
		"no_sink": c.NoSink, "error": mailErr(err), "transcript": normaliseTranscript(transcript),
		"html2text": text, "host_dependent": hostDependent[c.Name],
	}, nil
}

// ---------------------------------------------------------------------------------------------

// foldRows sweeps writeHeader's budget: three-word subjects whose lengths straddle the fold
// points, rendered through go-mail and cut down to the Subject header's lines.
func foldRows() ([]map[string]any, error) {
	var rows []map[string]any
	for _, a := range []int{1, 60, 65, 68, 70, 71, 72} {
		for b := 55; b <= 76; b++ {
			for _, c := range []int{1, 2, 3, 70, 71, 72, 73} {
				subject := strings.Repeat("w", a) + " " + strings.Repeat("v", b) + " " + strings.Repeat("u", c)
				m := gomail.NewMsg()
				m.SetGenHeader(gomail.HeaderSubject, subject)
				m.SetGenHeader(gomail.HeaderDate, "x")
				m.SetGenHeader(gomail.HeaderMessageID, "x")
				var buf bytes.Buffer
				if _, err := m.WriteTo(&buf); err != nil {
					return nil, err
				}
				out := buf.String()
				start := strings.Index(out, "\r\nSubject:") + 2
				end := strings.Index(out[start:], "\r\nUser-Agent: ")
				rows = append(rows, map[string]any{"subject": subject, "header": out[start : start+end+2]})
			}
		}
	}
	return rows, nil
}

func writeMailBehaviourFixture(outDir string) error {
	folds, err := foldRows()
	if err != nil {
		return err
	}
	messages, err := messageRows()
	if err != nil {
		return err
	}
	var smtpRows []map[string]any
	var wg sync.WaitGroup
	results := make([]map[string]any, len(smtpCases()))
	errs := make([]error, len(results))
	for i, c := range smtpCases() {
		wg.Add(1)
		go func(i int, c smtpCase) {
			defer wg.Done()
			results[i], errs[i] = runSMTPCase(c)
		}(i, c)
	}
	wg.Wait()
	for i := range results {
		if errs[i] != nil {
			return errs[i]
		}
		smtpRows = append(smtpRows, results[i])
	}
	sort.SliceStable(smtpRows, func(i, j int) bool { return smtpRows[i]["name"].(string) < smtpRows[j]["name"].(string) })

	fixture := map[string]any{
		"address_parse":      mailParseCorpus(),
		"address_string":     addressStringRows(),
		"word_encode":        wordEncodeRows(),
		"quoted_printable":   qpRows(),
		"multipart":          multipartRows(),
		"base64_decode":      base64Rows(),
		"read_response":      readResponseRows(),
		"dot_writer":         dotWriterRows(),
		"messages":           messages,
		"header_fold":        folds,
		"smtp":               smtpRows,
		"sink_cert_pem":      sinkCertPEM,
		"sink_key_pem":       sinkKeyPEM,
		"type_by_extension":  map[string]string{".png": mime.TypeByExtension(".png"), ".PNG": mime.TypeByExtension(".PNG"), ".jpg": mime.TypeByExtension(".jpg"), ".gif": mime.TypeByExtension(".gif"), "": mime.TypeByExtension("")},
		"go_mail_user_agent": "go-mail v" + gomail.VERSION + " // https://github.com/wneessen/go-mail",
	}
	blob, err := json.MarshalIndent(fixture, "", "  ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_mail.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}
