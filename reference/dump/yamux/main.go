// Command yamux is the parity oracle for crates/goplugin's yamux: hashicorp/yamux v0.1.2 (the
// version go-plugin pins), as an acceptor that executes commands and as a driver that runs a
// scenario and prints a transcript.
//
//	yamux listen                   accept one connection as the yamux server; execute commands
//	yamux dial <addr>              connect as the yamux client; execute commands
//	yamux listen-drive             accept one connection as the server; run the scenario
//	yamux dial-drive <addr>        connect as the client; run the scenario
//	yamux selftest                 both ends in one process: the reference transcript
//
// Listening modes print the address on the first line. Every mode ends when the session does, or
// when stdin closes. Environment: YAMUX_KEEPALIVE_MS (default 250), so a short idle crosses the
// keepalive.
//
// A command is the first line a driver writes on a stream it opens; the acceptor answers on the
// same stream. The Rust test implements the same acceptor and the same driver.
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/hashicorp/yamux"
)

func config() *yamux.Config {
	c := yamux.DefaultConfig()
	ms := 250
	if v, err := strconv.Atoi(os.Getenv("YAMUX_KEEPALIVE_MS")); err == nil {
		ms = v
	}
	c.KeepAliveInterval = time.Duration(ms) * time.Millisecond
	c.LogOutput = io.Discard
	return c
}

func pattern(i int) byte { return byte((i*31 + 7) % 251) }

// ─── acceptor ──────────────────────────────────────────────────────────────────────────────

type acceptor struct {
	sess      *yamux.Session
	lastCount atomic.Int64
}

func readLine(s net.Conn) (string, error) {
	var b []byte
	one := make([]byte, 1)
	for {
		if _, err := io.ReadFull(s, one); err != nil {
			return "", err
		}
		if one[0] == '\n' {
			return string(b), nil
		}
		b = append(b, one[0])
	}
}

func (a *acceptor) run() {
	for {
		s, err := a.sess.Accept()
		if err != nil {
			return
		}
		// The command is read here, in the accept loop, so that `pause` really stops accepting.
		line, err := readLine(s)
		if err != nil {
			s.Close()
			continue
		}
		fields := strings.Fields(line)
		if len(fields) > 0 && fields[0] == "pause" {
			ms, _ := strconv.Atoi(fields[1])
			fmt.Fprintf(s, "paused\n")
			s.Close()
			time.Sleep(time.Duration(ms) * time.Millisecond)
			continue
		}
		go a.handle(s, fields)
	}
}

func (a *acceptor) handle(s net.Conn, cmd []string) {
	defer s.Close()
	if len(cmd) == 0 {
		return
	}
	arg := 0
	if len(cmd) > 1 {
		arg, _ = strconv.Atoi(cmd[1])
	}
	switch cmd[0] {
	case "echo":
		_, _ = io.Copy(s, s)
	case "sink":
		buf := make([]byte, 32*1024)
		count, sum := 0, 0
		for {
			n, err := s.Read(buf)
			for _, b := range buf[:n] {
				sum += int(b)
			}
			count += n
			if err != nil {
				break
			}
		}
		fmt.Fprintf(s, "%d %d\n", count, sum)
	case "send":
		out := make([]byte, arg)
		for i := range out {
			out[i] = pattern(i)
		}
		_, _ = s.Write(out)
	case "closefirst":
		s.Close()
		n, _ := io.Copy(io.Discard, s)
		a.lastCount.Store(n)
	case "lastcount":
		fmt.Fprintf(s, "%d\n", a.lastCount.Load())
	case "ping":
		if _, err := a.sess.Ping(); err != nil {
			fmt.Fprintf(s, "err %v\n", err)
		} else {
			fmt.Fprintf(s, "pong\n")
		}
	case "open":
		var wg sync.WaitGroup
		var ok, errs atomic.Int64
		for i := 0; i < arg; i++ {
			wg.Add(1)
			go func(i int) {
				defer wg.Done()
				o, err := a.sess.Open()
				if err != nil {
					errs.Add(1)
					return
				}
				fmt.Fprintf(o, "hello %d\n", i)
				o.Close()
				ok.Add(1)
			}(i)
		}
		wg.Wait()
		fmt.Fprintf(s, "opened %d %d\n", ok.Load(), errs.Load())
	case "goaway":
		_ = a.sess.GoAway()
		fmt.Fprintf(s, "ok\n")
	default:
		fmt.Fprintf(s, "unknown %s\n", cmd[0])
	}
}

// ─── driver ────────────────────────────────────────────────────────────────────────────────

type step struct {
	Step   string `json:"step"`
	Result string `json:"result"`
}

func command(sess *yamux.Session, line string) (net.Conn, error) {
	s, err := sess.Open()
	if err != nil {
		return nil, err
	}
	if _, err := fmt.Fprintf(s, "%s\n", line); err != nil {
		return nil, err
	}
	return s, nil
}

func echoMany(sess *yamux.Session, streams, size int) string {
	var wg sync.WaitGroup
	var bad atomic.Int64
	for i := 0; i < streams; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			s, err := command(sess, "echo")
			if err != nil {
				bad.Add(1)
				return
			}
			want := make([]byte, size)
			for j := range want {
				want[j] = byte((j + i) % 256)
			}
			go func() {
				_, _ = s.Write(want)
				s.Close()
			}()
			got, err := io.ReadAll(s)
			if err != nil || string(got) != string(want) {
				bad.Add(1)
			}
		}(i)
	}
	wg.Wait()
	return fmt.Sprintf("%d streams, %d failed", streams, bad.Load())
}

func reply(sess *yamux.Session, line string) string {
	s, err := command(sess, line)
	if err != nil {
		return "open: " + err.Error()
	}
	defer s.Close()
	got, err := io.ReadAll(s)
	if err != nil {
		return "read: " + err.Error()
	}
	return strings.TrimSpace(string(got))
}

func drive(sess *yamux.Session) []step {
	var out []step
	add := func(name, result string) { out = append(out, step{name, result}) }

	add("echo 300x64KiB", echoMany(sess, 300, 64<<10))

	func() {
		s, err := command(sess, "send 8388608")
		if err != nil {
			add("send 8MiB", "open: "+err.Error())
			return
		}
		got, err := io.ReadAll(s)
		bad := 0
		for i, b := range got {
			if b != pattern(i) {
				bad++
			}
		}
		add("send 8MiB", fmt.Sprintf("%d bytes, %d wrong, err=%v", len(got), bad, err))
	}()

	func() {
		s, err := command(sess, "sink")
		if err != nil {
			add("sink 16MiB", "open: "+err.Error())
			return
		}
		data := make([]byte, 16<<20)
		want := 0
		for i := range data {
			data[i] = pattern(i)
			want += int(data[i])
		}
		_, _ = s.Write(data)
		s.Close()
		got, _ := io.ReadAll(s)
		add("sink 16MiB", fmt.Sprintf("%s (want %d %d)", strings.TrimSpace(string(got)), len(data), want))
	}()

	func() {
		s, err := command(sess, "closefirst")
		if err != nil {
			add("closefirst", "open: "+err.Error())
			return
		}
		// The acceptor closes before reading: this side sees EOF but may still write.
		n, _ := io.Copy(io.Discard, s)
		_, werr := s.Write([]byte("abc"))
		s.Close()
		time.Sleep(100 * time.Millisecond)
		add("closefirst", fmt.Sprintf("read %d, write err=%v, peer read %s", n, werr, reply(sess, "lastcount")))
	}()

	add("ping", reply(sess, "ping"))

	func() {
		var wg sync.WaitGroup
		var hellos atomic.Int64
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 20; i++ {
				s, err := sess.Accept()
				if err != nil {
					return
				}
				line, _ := io.ReadAll(s)
				if strings.HasPrefix(string(line), "hello ") {
					hellos.Add(1)
				}
				s.Close()
			}
		}()
		r := reply(sess, "open 20")
		wg.Wait()
		add("open 20", fmt.Sprintf("%s, %d hellos", r, hellos.Load()))
	}()

	time.Sleep(time.Second)
	add("after 1s idle", echoMany(sess, 1, 10))

	// With the acceptor paused, open more streams than its backlog: an opener that limits
	// un-ACKed streams to the backlog loses none of them.
	add("pause", reply(sess, "pause 500"))
	add("echo 300 while paused", echoMany(sess, 300, 16))

	add("goaway", reply(sess, "goaway"))
	time.Sleep(100 * time.Millisecond)
	if _, err := sess.Open(); err != nil {
		add("open after goaway", err.Error())
	} else {
		add("open after goaway", "opened")
	}
	return out
}

// ─── main ──────────────────────────────────────────────────────────────────────────────────

func printJSON(v any) {
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	_ = enc.Encode(v)
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: yamux listen|dial <addr>|listen-drive|dial-drive <addr>|selftest")
		os.Exit(2)
	}
	go func() {
		_, _ = io.Copy(io.Discard, os.Stdin)
		os.Exit(0)
	}()
	mode := os.Args[1]
	var conn net.Conn
	var err error
	switch mode {
	case "listen", "listen-drive":
		l, lerr := net.Listen("tcp", "127.0.0.1:0")
		if lerr != nil {
			panic(lerr)
		}
		fmt.Println(l.Addr().String())
		conn, err = l.Accept()
	case "dial", "dial-drive":
		conn, err = net.Dial("tcp", os.Args[2])
	case "selftest":
		l, lerr := net.Listen("tcp", "127.0.0.1:0")
		if lerr != nil {
			panic(lerr)
		}
		go func() {
			c, aerr := l.Accept()
			if aerr != nil {
				panic(aerr)
			}
			sess, serr := yamux.Server(c, config())
			if serr != nil {
				panic(serr)
			}
			(&acceptor{sess: sess}).run()
		}()
		c, derr := net.Dial("tcp", l.Addr().String())
		if derr != nil {
			panic(derr)
		}
		sess, serr := yamux.Client(c, config())
		if serr != nil {
			panic(serr)
		}
		printJSON(drive(sess))
		return
	default:
		panic("unknown mode " + mode)
	}
	if err != nil {
		panic(err)
	}
	var sess *yamux.Session
	if strings.HasPrefix(mode, "listen") {
		sess, err = yamux.Server(conn, config())
	} else {
		sess, err = yamux.Client(conn, config())
	}
	if err != nil {
		panic(err)
	}
	if strings.HasSuffix(mode, "drive") {
		printJSON(drive(sess))
		sess.Close()
		return
	}
	(&acceptor{sess: sess}).run()
}
