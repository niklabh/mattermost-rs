// Command netrpc is the parity oracle for crates/go-netrpc: Go's own net/rpc, as a server for a
// Rust client and as a client of a Rust server.
//
//	netrpc server                 listen on 127.0.0.1:0, print the address, serve until stdin closes
//	netrpc client <addr>          run the scenario against a server, print the transcript as JSON
//
// The transcript of the Go client against the Go server is the reference; the Rust client must
// produce the same transcript against the Go server, and the Go client the same one against the
// Rust server. Both sides implement the same services (see `Arith` and `Echo` below and their
// mirror in crates/go-netrpc/tests/go_interop.rs).
package main

import (
	"bufio"
	"encoding/gob"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/rpc"
	"os"
	"sort"
	"sync"
	"time"
)

type Args struct {
	A, B int
}

type Quotient struct {
	Quo, Rem int
}

type Arith struct{}

func (Arith) Multiply(args *Args, reply *int) error {
	*reply = args.A * args.B
	return nil
}

func (Arith) Divide(args *Args, quo *Quotient) error {
	if args.B == 0 {
		return errors.New("divide by zero")
	}
	quo.Quo = args.A / args.B
	quo.Rem = args.A % args.B
	return nil
}

// Fail sets part of its reply and then fails: the reply must not reach the client.
func (Arith) Fail(args *Args, quo *Quotient) error {
	quo.Quo = 99
	return fmt.Errorf("failed with %d", args.A)
}

type Echo struct{}

func (Echo) Iface(arg *any, reply *any) error {
	*reply = *arg
	return nil
}

func (Echo) Strings(arg []string, reply *[]string) error {
	*reply = append(arg, "!")
	return nil
}

// Map echoes into a reply map net/rpc has already allocated (server.go, readRequest).
func (Echo) Map(arg map[string]int, reply *map[string]int) error {
	for k, v := range arg {
		(*reply)[k] = v * 2
	}
	return nil
}

func (Echo) Empty(arg struct{}, reply *struct{}) error {
	return nil
}

// Sum returns the length and byte sum of a large payload.
func (Echo) Sum(arg []byte, reply *[2]int) error {
	s := 0
	for _, b := range arg {
		s += int(b)
	}
	*reply = [2]int{len(arg), s}
	return nil
}

// Slow sleeps for the requested milliseconds and returns them, so replies arrive out of order.
func (Echo) Slow(ms int, reply *int) error {
	time.Sleep(time.Duration(ms) * time.Millisecond)
	*reply = ms
	return nil
}

func init() {
	gob.Register(map[string]any{})
	gob.Register([]any{})
}

type entry struct {
	Call  string `json:"call"`
	Reply any    `json:"reply,omitempty"`
	Error string `json:"error,omitempty"`
}

func scenario(c *rpc.Client) []entry {
	var out []entry
	record := func(call string, reply any, err error) {
		e := entry{Call: call}
		if err != nil {
			var se rpc.ServerError
			if errors.As(err, &se) {
				e.Error = "server: " + string(se)
			} else {
				e.Error = "client: " + err.Error()
			}
		} else {
			e.Reply = reply
		}
		out = append(out, e)
	}

	var n int
	record("Arith.Multiply 7x8", &n, c.Call("Arith.Multiply", &Args{7, 8}, &n))
	var q Quotient
	record("Arith.Divide 17/5", &q, c.Call("Arith.Divide", &Args{17, 5}, &q))
	var q0 Quotient
	record("Arith.Divide 1/0", &q0, c.Call("Arith.Divide", &Args{1, 0}, &q0))
	var qf Quotient
	record("Arith.Fail", &qf, c.Call("Arith.Fail", &Args{3, 4}, &qf))
	var x int
	record("Arith.Nope", &x, c.Call("Arith.Nope", &Args{}, &x))
	record("Nope.Method", &x, c.Call("Nope.Method", &Args{}, &x))
	record("NoDot", &x, c.Call("NoDot", &Args{}, &x))
	// The connection survives all of the above.
	var n2 int
	record("Arith.Multiply after errors", &n2, c.Call("Arith.Multiply", &Args{-3, 9}, &n2))

	var iface any = map[string]any{"k": []any{1.5, "x", nil}, "n": "s"}
	var ireply any
	record("Echo.Iface", &ireply, c.Call("Echo.Iface", &iface, &ireply))
	var strs []string
	record("Echo.Strings", &strs, c.Call("Echo.Strings", []string{"a", ""}, &strs))
	m := map[string]int{}
	record("Echo.Map", &m, c.Call("Echo.Map", map[string]int{"one": 1, "two": 2}, &m))
	var empty struct{}
	record("Echo.Empty", &empty, c.Call("Echo.Empty", struct{}{}, &empty))
	big := make([]byte, 3<<20)
	for i := range big {
		big[i] = byte(i * 7)
	}
	var sum [2]int
	record("Echo.Sum 3MiB", &sum, c.Call("Echo.Sum", big, &sum))

	// Fifty concurrent calls whose replies come back in reverse order.
	var mu sync.Mutex
	var wg sync.WaitGroup
	got := map[int]int{}
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func(ms int) {
			defer wg.Done()
			var r int
			if err := c.Call("Echo.Slow", 200-ms*3, &r); err == nil {
				mu.Lock()
				got[200-ms*3] = r
				mu.Unlock()
			}
		}(i)
	}
	wg.Wait()
	keys := make([]int, 0, len(got))
	mismatched := 0
	for k, v := range got {
		keys = append(keys, k)
		if k != v {
			mismatched++
		}
	}
	sort.Ints(keys)
	record("Echo.Slow x50", map[string]int{"answered": len(keys), "mismatched": mismatched}, nil)

	// A body the server cannot decode into Args: an error reply, and the connection lives on.
	var bad int
	err := c.Call("Arith.Multiply", "not args", &bad)
	if err != nil {
		var se rpc.ServerError
		if errors.As(err, &se) {
			err = rpc.ServerError("<decode error>")
		}
	}
	record("Arith.Multiply with a string", &bad, err)
	var n3 int
	record("Arith.Multiply after a bad body", &n3, c.Call("Arith.Multiply", &Args{2, 21}, &n3))
	return out
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: netrpc server | netrpc client <addr>")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "server":
		srv := rpc.NewServer()
		if err := srv.Register(Arith{}); err != nil {
			panic(err)
		}
		if err := srv.Register(Echo{}); err != nil {
			panic(err)
		}
		l, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			panic(err)
		}
		fmt.Println(l.Addr().String())
		go srv.Accept(l)
		// Serve until the parent closes stdin, so a test that dies cannot leak the process.
		_, _ = bufio.NewReader(os.Stdin).ReadString('\n')
	case "client":
		c, err := rpc.Dial("tcp", os.Args[2])
		if err != nil {
			panic(err)
		}
		out := scenario(c)
		_ = c.Close()
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		if err := enc.Encode(out); err != nil {
			panic(err)
		}
	case "selftest":
		// Go client against Go server in one process: the reference transcript.
		srv := rpc.NewServer()
		_ = srv.Register(Arith{})
		_ = srv.Register(Echo{})
		l, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			panic(err)
		}
		go srv.Accept(l)
		c, err := rpc.Dial("tcp", l.Addr().String())
		if err != nil {
			panic(err)
		}
		out := scenario(c)
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		_ = enc.Encode(out)
	}
}
