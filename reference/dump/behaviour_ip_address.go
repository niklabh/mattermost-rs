package main

// Behavioural oracle for `utils.GetIPAddress` (channels/utils/utils.go:94), written to
// fixtures/behaviour_ip_address.json and asserted by `mm_api::client_ip`'s `go_parity` module.
//
// Derived from the **AGPL** half of the tree, so it feeds `mm-api`'s tests only.
//
// # What a reader could get wrong, and so what the corpus covers
//
//   - The walk order: the first *configured* header holding a parseable address wins, not the
//     first header on the wire.
//   - `r.Header.Get` is the **first** line of a repeated header, and the name is canonicalised
//     on both sides, so a configured `x-forwarded-for` finds `X-Forwarded-For`.
//   - Only the part before the first comma is considered, `strings.TrimSpace`d (Unicode
//     whitespace, so a leading U+00A0 goes), and returned **as written** — never re-formatted.
//   - "Parseable" is `net.ParseIP`: no port, no brackets, no zone, no leading-zero octets.
//   - The fallback is `net.SplitHostPort(r.RemoteAddr)`'s host, its error discarded, so an
//     unsplittable `RemoteAddr` (a unix socket's) is "".
//
// # The request is parsed from bytes, and the peer is a real `net.TCPAddr`
//
// Each case is written as an HTTP/1.1 request and read back with `http.ReadRequest`, so header
// names and values are exactly what `net/http`'s server hands a handler. `RemoteAddr` is what the
// server sets it to — `TCPAddr.String()` of the peer — so the Rust side, which holds a
// `SocketAddr`, is compared against the same peer rather than against a string no listener
// produces. Rows with no peer carry a raw `RemoteAddr` instead (a unix socket's `@`, or none).
//
// The function under test is Go's own: `channels/utils` is imported, not transcribed.

import (
	"bufio"
	"fmt"
	"net"
	"net/http"
	"strings"

	"github.com/mattermost/mattermost/server/v8/channels/utils"
)

type ipAddressCase struct {
	Name    string      `json:"name"`
	Trusted []string    `json:"trusted"`
	Headers [][2]string `json:"headers"`
	// PeerIP/PeerPort build a net.TCPAddr; when PeerIP is empty, RawRemoteAddr is used verbatim.
	PeerIP        string `json:"peer_ip"`
	PeerPort      int    `json:"peer_port"`
	RawRemoteAddr string `json:"raw_remote_addr"`
}

var xff = []string{"X-Forwarded-For"}
var realThenXFF = []string{"X-Real-IP", "X-Forwarded-For"}

func h(pairs ...string) [][2]string {
	out := make([][2]string, 0, len(pairs)/2)
	for i := 0; i+1 < len(pairs); i += 2 {
		out = append(out, [2]string{pairs[i], pairs[i+1]})
	}
	return out
}

var ipAddressCorpus = []ipAddressCase{
	// The fallback alone: Go's default configuration.
	{Name: "no trusted headers, v4 peer", PeerIP: "10.0.0.1", PeerPort: 54321},
	{Name: "no trusted headers ignores XFF", Headers: h("X-Forwarded-For", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 peer", PeerIP: "2001:db8::7", PeerPort: 443},
	{Name: "loopback v6 peer", PeerIP: "::1", PeerPort: 80},
	{Name: "v4-mapped peer prints as v4", PeerIP: "::ffff:10.1.2.3", PeerPort: 80},
	{Name: "unix socket remote addr", RawRemoteAddr: "@"},
	{Name: "no remote addr", RawRemoteAddr: ""},
	{Name: "unix socket, header still walked", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4"), RawRemoteAddr: "@"},

	// One trusted header.
	{Name: "xff single", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff list takes the first", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4, 5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff list without spaces", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4,5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff first element padded", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4 , 5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff first garbage, second good", Trusted: xff, Headers: h("X-Forwarded-For", "garbage, 1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff unknown", Trusted: xff, Headers: h("X-Forwarded-For", "unknown"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff leading comma", Trusted: xff, Headers: h("X-Forwarded-For", ",1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff empty value", Trusted: xff, Headers: h("X-Forwarded-For", ""), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff absent", Trusted: xff, PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff space separated", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4 5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff with port", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4:8080"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff leading nbsp is trimmed", Trusted: xff, Headers: h("X-Forwarded-For", " 1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff trailing nbsp before comma", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4 ,x"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "xff non-ascii after the first element", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4, é"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "repeated xff line reads the first", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4", "X-Forwarded-For", "5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "repeated xff line, first bad", Trusted: xff, Headers: h("X-Forwarded-For", "bad", "X-Forwarded-For", "5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "configured name lower case", Trusted: []string{"x-forwarded-for"}, Headers: h("X-Forwarded-For", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "wire name upper case", Trusted: xff, Headers: h("X-FORWARDED-FOR", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "configured name with underscores", Trusted: []string{"X_Real_Ip"}, Headers: h("x_real_ip", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "configured name empty", Trusted: []string{""}, Headers: h("X-Forwarded-For", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "configured name with a space", Trusted: []string{"X-Forwarded-For "}, Headers: h("X-Forwarded-For", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "forwarded header syntax is not an address", Trusted: []string{"Forwarded"}, Headers: h("Forwarded", "for=1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},

	// Two trusted headers: configured order, not wire order.
	{Name: "real ip wins over xff", Trusted: realThenXFF, Headers: h("X-Forwarded-For", "5.6.7.8", "X-Real-IP", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "real ip absent, xff used", Trusted: realThenXFF, Headers: h("X-Forwarded-For", "5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "real ip bad, xff used", Trusted: realThenXFF, Headers: h("X-Real-IP", "nope", "X-Forwarded-For", "5.6.7.8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "real ip bad, xff absent", Trusted: realThenXFF, Headers: h("X-Real-IP", "nope"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "both bad", Trusted: realThenXFF, Headers: h("X-Real-IP", "nope", "X-Forwarded-For", "also-nope"), PeerIP: "2001:db8::9", PeerPort: 1},
	{Name: "xff first reverses the winner", Trusted: []string{"X-Forwarded-For", "X-Real-IP"}, Headers: h("X-Forwarded-For", "5.6.7.8", "X-Real-IP", "1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "same header configured twice", Trusted: []string{"X-Forwarded-For", "X-Forwarded-For"}, Headers: h("X-Forwarded-For", "bad"), PeerIP: "10.0.0.1", PeerPort: 1},

	// What `net.ParseIP` accepts, returned as written.
	{Name: "v6 address", Trusted: xff, Headers: h("X-Forwarded-For", "2001:db8::1"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 upper case kept as written", Trusted: xff, Headers: h("X-Forwarded-For", "2001:DB8::1"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 uncompressed kept as written", Trusted: xff, Headers: h("X-Forwarded-For", "2001:0db8:0000:0000:0000:0000:0000:0001"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 bracketed", Trusted: xff, Headers: h("X-Forwarded-For", "[2001:db8::1]"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 bracketed with port", Trusted: xff, Headers: h("X-Forwarded-For", "[2001:db8::1]:443"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 with zone", Trusted: xff, Headers: h("X-Forwarded-For", "fe80::1%eth0"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4-mapped v6", Trusted: xff, Headers: h("X-Forwarded-For", "::ffff:1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4-mapped v6 upper case", Trusted: xff, Headers: h("X-Forwarded-For", "::FFFF:1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4-compatible v6", Trusted: xff, Headers: h("X-Forwarded-For", "::1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 with embedded v4 tail", Trusted: xff, Headers: h("X-Forwarded-For", "1:2:3:4:5:6:1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 embedded v4 leading zero", Trusted: xff, Headers: h("X-Forwarded-For", "::ffff:01.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 unspecified", Trusted: xff, Headers: h("X-Forwarded-For", "::"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 two double colons", Trusted: xff, Headers: h("X-Forwarded-For", "1::2::3"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 nine groups", Trusted: xff, Headers: h("X-Forwarded-For", "1:2:3:4:5:6:7:8:9"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 eight groups with double colon", Trusted: xff, Headers: h("X-Forwarded-For", "1:2:3:4:5:6:7::8"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 seven groups with double colon", Trusted: xff, Headers: h("X-Forwarded-For", "1:2:3:4:5:6::7"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 five hex digits", Trusted: xff, Headers: h("X-Forwarded-For", "fffff::1"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 leading zeros in a group", Trusted: xff, Headers: h("X-Forwarded-For", "0001::1"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 trailing single colon", Trusted: xff, Headers: h("X-Forwarded-For", "1::"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v6 leading single colon", Trusted: xff, Headers: h("X-Forwarded-For", ":1::2"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 unspecified", Trusted: xff, Headers: h("X-Forwarded-For", "0.0.0.0"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 broadcast", Trusted: xff, Headers: h("X-Forwarded-For", "255.255.255.255"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 octet 256", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.256"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 three octets", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 five octets", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4.5"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 leading zero", Trusted: xff, Headers: h("X-Forwarded-For", "01.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 zero octet", Trusted: xff, Headers: h("X-Forwarded-For", "1.0.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 trailing dot", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.4."), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 hex", Trusted: xff, Headers: h("X-Forwarded-For", "0x1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 plus sign", Trusted: xff, Headers: h("X-Forwarded-For", "+1.2.3.4"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "v4 long octet", Trusted: xff, Headers: h("X-Forwarded-For", "1.2.3.0004"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "integer", Trusted: xff, Headers: h("X-Forwarded-For", "16909060"), PeerIP: "10.0.0.1", PeerPort: 1},
	{Name: "hostname", Trusted: xff, Headers: h("X-Forwarded-For", "localhost"), PeerIP: "10.0.0.1", PeerPort: 1},
}

func (c ipAddressCase) remoteAddr() string {
	if c.PeerIP == "" {
		return c.RawRemoteAddr
	}
	ip := net.ParseIP(c.PeerIP)
	if ip == nil {
		panic("ip_address corpus: bad peer " + c.PeerIP)
	}
	return (&net.TCPAddr{IP: ip, Port: c.PeerPort}).String()
}

func writeIPAddressBehaviourFixture(outDir string) error {
	rows := make([]map[string]any, 0, len(ipAddressCorpus))
	for _, c := range ipAddressCorpus {
		var raw strings.Builder
		raw.WriteString("GET / HTTP/1.1\r\nHost: example.com\r\n")
		for _, kv := range c.Headers {
			raw.WriteString(kv[0] + ": " + kv[1] + "\r\n")
		}
		raw.WriteString("\r\n")
		r, err := http.ReadRequest(bufio.NewReader(strings.NewReader(raw.String())))
		if err != nil {
			return fmt.Errorf("%s: %w", c.Name, err)
		}
		r.RemoteAddr = c.remoteAddr()
		trusted := c.Trusted
		if trusted == nil {
			// Go's default after SetDefaults (config.go:679).
			trusted = []string{}
		}
		headers := c.Headers
		if headers == nil {
			headers = [][2]string{}
		}
		rows = append(rows, map[string]any{
			"name":            c.Name,
			"trusted":         trusted,
			"headers":         headers,
			"peer_ip":         c.PeerIP,
			"peer_port":       c.PeerPort,
			"raw_remote_addr": c.RawRemoteAddr,
			"remote_addr":     r.RemoteAddr,
			"want":            utils.GetIPAddress(r, trusted),
		})
	}
	return writeJSONFixture(outDir, "behaviour_ip_address.json", map[string]any{"get_ip_address": rows})
}
