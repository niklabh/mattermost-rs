package main

// Hijacked responses (public/plugin/hijack.go), for crates/mm-plugin's conformance suites.
//
// Both conformance plugins answer HijackURL by taking over the connection and running the same
// script; both hosts serve it twice: once through a recorder, which cannot be hijacked, and once
// through a real HTTP server, whose client records every byte it received. The script is built so
// that the bytes the client sees depend on each Go behaviour a port could get wrong:
//
//   - "pong: ping": the line the client sent in the same packet as the request, read through the
//     buffered reader, where Go's server may already hold it.
//   - "already: ...": a second Hijack fails with ErrAlreadyHijacked.
//   - "timeout: true": a read under a deadline already past fails with "i/o timeout".
//   - "raw: raw": a raw conn.Read, answered after the client has seen the line before it.
//   - HijackPayload: 5000 bytes through the buffered writer, which a write larger than the
//     buffer sends straight through on both sides.
//   - no "tail": 5 bytes through the buffered writer and a Flush that empties only the plugin's
//     buffer. The host's buffer is never flushed, so they are lost when the plugin closes.

import (
	"bytes"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	neturl "net/url"
	"strings"
	"time"

	"github.com/mattermost/mattermost/server/public/plugin"
)

// HijackURL is the request both conformance plugins answer by hijacking it.
const HijackURL = "/plugins/conformance/hijack"

// HijackUpgrade is the response head both plugins write on the hijacked connection.
const HijackUpgrade = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: conformance\r\nConnection: Upgrade\r\n\r\n"

// HijackPayload is what both plugins write through the buffered writer: more than its buffer.
func HijackPayload() []byte {
	b := make([]byte, 5000)
	for i := range b {
		b[i] = byte('a' + i%26)
	}
	return b
}

// hijack is the plugin half: take over the connection and run the script.
func (c *conformance) hijack(w http.ResponseWriter) {
	hj := w.(http.Hijacker)
	conn, rw, err := hj.Hijack()
	if err != nil {
		w.WriteHeader(409)
		fmt.Fprintf(w, "hijack: %v", err)
		record(map[string]any{"hook": "hijack", "error": err.Error()})
		return
	}
	_, _, again := hj.Hijack()

	line, err := rw.ReadString('\n')
	if err != nil {
		panic(err)
	}
	if err := conn.SetReadDeadline(time.Unix(1, 0)); err != nil {
		panic(err)
	}
	_, readErr := conn.Read(make([]byte, 1))
	timedOut := readErr != nil && strings.HasSuffix(readErr.Error(), "i/o timeout")
	for _, clear := range []error{
		conn.SetReadDeadline(time.Time{}),
		conn.SetWriteDeadline(time.Now().Add(time.Hour)),
		conn.SetDeadline(time.Time{}),
	} {
		if clear != nil {
			panic(clear)
		}
	}
	if _, err := fmt.Fprintf(conn, "%spong: %salready: %v\ntimeout: %v\n", HijackUpgrade, line, again, timedOut); err != nil {
		panic(err)
	}

	raw := make([]byte, 64)
	n, err := conn.Read(raw)
	if err != nil {
		panic(err)
	}
	if _, err := fmt.Fprintf(conn, "raw: %s", raw[:n]); err != nil {
		panic(err)
	}

	if _, err := rw.Write(HijackPayload()); err != nil {
		panic(err)
	}
	if _, err := rw.WriteString("tail\n"); err != nil {
		panic(err)
	}
	if err := rw.Flush(); err != nil {
		panic(err)
	}
	if err := conn.Close(); err != nil {
		panic(err)
	}
	closeAgain := conn.Close()
	_, readClosed := conn.Read(raw)
	record(map[string]any{
		"hook":         "hijack",
		"close_again":  closedConn(closeAgain),
		"read_closed":  closedConn(readClosed),
		"read_timeout": timedOut,
	})
}

func closedConn(err error) bool {
	return err != nil && strings.Contains(err.Error(), "use of closed network connection")
}

// HijackClient is the client half: send the request with a line after it, answer the
// "timeout:" line with "raw\n", and read everything until the connection closes.
func HijackClient(addr string) (string, error) {
	conn, err := net.DialTimeout("tcp", addr, 10*time.Second)
	if err != nil {
		return "", err
	}
	defer conn.Close()
	if err := conn.SetDeadline(time.Now().Add(60 * time.Second)); err != nil {
		return "", err
	}
	request := "GET " + HijackURL + " HTTP/1.1\r\nHost: example.test\r\n\r\nping\n"
	if _, err := conn.Write([]byte(request)); err != nil {
		return "", err
	}
	var received []byte
	buf := make([]byte, 4096)
	answered := false
	for {
		n, err := conn.Read(buf)
		received = append(received, buf[:n]...)
		if !answered {
			if i := bytes.Index(received, []byte("timeout: ")); i >= 0 && bytes.IndexByte(received[i:], '\n') >= 0 {
				if _, err := conn.Write([]byte("raw\n")); err != nil {
					return string(received), err
				}
				answered = true
			}
		}
		if err == io.EOF {
			return string(received), nil
		}
		if err != nil {
			return string(received), err
		}
	}
}

// serveHijack is the Go host's half: the plugin's answer through a recorder, then what a real
// client received from a real server.
func serveHijack(hooks plugin.Hooks) {
	url, err := neturl.Parse(HijackURL)
	if err != nil {
		panic(err)
	}
	recorder := httptest.NewRecorder()
	hooks.ServeHTTP(&plugin.Context{}, recorder, &http.Request{
		Method:     "GET",
		URL:        url,
		Proto:      "HTTP/1.1",
		ProtoMajor: 1,
		ProtoMinor: 1,
		Header:     http.Header{},
		Host:       "example.test",
		RequestURI: HijackURL,
		Body:       http.NoBody,
	})
	header := map[string]any{}
	for k, v := range recorder.Result().Header {
		header[k] = v
	}
	record(map[string]any{
		"http":   "hijack-recorder",
		"status": recorder.Code,
		"header": header,
		"body":   recorder.Body.String(),
	})

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hooks.ServeHTTP(&plugin.Context{}, w, r)
	}))
	defer server.Close()
	received, err := HijackClient(server.Listener.Addr().String())
	entry := map[string]any{"http": "hijack", "received": received}
	if err != nil {
		entry["error"] = err.Error()
	}
	record(entry)
}
