// Command goplugin is the parity oracle for crates/goplugin's host and plugin sides: HashiCorp
// go-plugin v1.8.0's net/rpc protocol, as a plugin and as a host.
//
//	goplugin plugin                     serve the "kv" plugin (run by a host, never directly)
//	goplugin host <plugin> [args...]    launch <plugin>, run the scenario, print the transcript
//
// The Go host's transcript against the Go plugin (`goplugin host <this binary> plugin`) is the
// reference. The Rust host must produce it against this plugin, and this host must produce it
// against the Rust plugin (crates/goplugin/examples/kv_plugin.rs), which implements the same
// methods.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"net/rpc"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/go-plugin"
)

var handshake = plugin.HandshakeConfig{
	ProtocolVersion:  1,
	MagicCookieKey:   "GOPLUGIN_ORACLE",
	MagicCookieValue: "hello",
}

// ─── plugin side ───────────────────────────────────────────────────────────────────────────

type PutArgs struct {
	Key, Value string
}

type AddArgs struct {
	A, B int
}

type AddViaArgs struct {
	BrokerID uint32
	A, B     int
}

type KV struct {
	mu     sync.Mutex
	m      map[string]string
	broker *plugin.MuxBroker
	logger hclog.Logger
}

func (k *KV) Put(args PutArgs, _ *struct{}) error {
	k.mu.Lock()
	defer k.mu.Unlock()
	k.m[args.Key] = args.Value
	return nil
}

func (k *KV) Get(key string, reply *string) error {
	k.mu.Lock()
	defer k.mu.Unlock()
	v, ok := k.m[key]
	if !ok {
		return fmt.Errorf("not found: %s", key)
	}
	*reply = v
	return nil
}

// AddVia dials a server the host is serving on the broker, the way a Mattermost plugin reaches
// the API server the host opens in OnActivate.
func (k *KV) AddVia(args AddViaArgs, reply *int) error {
	conn, err := k.broker.Dial(args.BrokerID)
	if err != nil {
		return err
	}
	c := rpc.NewClient(conn)
	defer c.Close()
	return c.Call("Plugin.Add", AddArgs{args.A, args.B}, reply)
}

type Mul struct{ factor int }

func (m *Mul) Mul(x int, reply *int) error {
	*reply = x * m.factor
	return nil
}

// Offer serves a new object on the broker and returns its id for the host to dial.
func (k *KV) Offer(factor int, reply *uint32) error {
	id := k.broker.NextId()
	go k.broker.AcceptAndServe(id, &Mul{factor})
	*reply = id
	return nil
}

// Print writes to the plugin's stdout and stderr, which go-plugin carries over yamux streams.
func (k *KV) Print(s string, _ *struct{}) error {
	fmt.Fprintf(os.Stdout, "out:%s\n", s)
	fmt.Fprintf(os.Stderr, "err:%s\n", s)
	return nil
}

// Log writes an hclog JSON line to the process's own stderr, which the host parses.
func (k *KV) Log(s string, _ *struct{}) error {
	k.logger.Info(s, "key", "value")
	return nil
}

type KVPlugin struct {
	logger hclog.Logger
}

func (p *KVPlugin) Server(b *plugin.MuxBroker) (interface{}, error) {
	return &KV{m: map[string]string{}, broker: b, logger: p.logger}, nil
}

func (p *KVPlugin) Client(b *plugin.MuxBroker, c *rpc.Client) (interface{}, error) {
	return &KVClient{client: c, broker: b}, nil
}

type KVClient struct {
	client *rpc.Client
	broker *plugin.MuxBroker
}

func servePlugin() {
	// Created before Serve swaps os.Stderr for the stderr stream, so logs go to the process's
	// real stderr.
	logger := hclog.New(&hclog.LoggerOptions{Output: os.Stderr, Level: hclog.Trace, JSONFormat: true, Name: "kv"})
	plugin.Serve(&plugin.ServeConfig{
		HandshakeConfig: handshake,
		Plugins:         plugin.PluginSet{"kv": &KVPlugin{logger: logger}},
		Logger:          hclog.NewNullLogger(),
	})
}

// ─── host side ─────────────────────────────────────────────────────────────────────────────

type Adder struct{}

func (Adder) Add(args AddArgs, reply *int) error {
	*reply = args.A + args.B
	return nil
}

type step struct {
	Step   string `json:"step"`
	Result string `json:"result"`
}

// lockedBuffer is written by go-plugin's stream-copying goroutines.
type lockedBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (l *lockedBuffer) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.b.Write(p)
}

func (l *lockedBuffer) String() string {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.b.String()
}

func checksum(path string) []byte {
	b, err := os.ReadFile(path)
	if err != nil {
		panic(err)
	}
	s := sha256.Sum256(b)
	return s[:]
}

func command(argv []string) *exec.Cmd {
	return exec.Command(argv[0], argv[1:]...)
}

func errText(err error) string {
	if err == nil {
		return "ok"
	}
	var se rpc.ServerError
	if errors.As(err, &se) {
		return "server: " + string(se)
	}
	return "error: " + err.Error()
}

func host(argv []string) []step {
	var out []step
	add := func(name, result string) { out = append(out, step{name, result}) }
	null := hclog.NewNullLogger()
	plugins := plugin.PluginSet{"kv": &KVPlugin{}, "nope": &KVPlugin{}}

	stdout, stderr, raw := &lockedBuffer{}, &lockedBuffer{}, &lockedBuffer{}
	client := plugin.NewClient(&plugin.ClientConfig{
		HandshakeConfig: handshake,
		Plugins:         plugins,
		Cmd:             command(argv),
		SecureConfig:    &plugin.SecureConfig{Checksum: checksum(argv[0]), Hash: sha256.New()},
		SyncStdout:      stdout,
		SyncStderr:      stderr,
		Stderr:          raw,
		Logger:          null,
	})
	rpcClient, err := client.Client()
	if err != nil {
		add("start", errText(err))
		return out
	}
	add("start", fmt.Sprintf("protocol=%s version=%d", client.Protocol(), client.NegotiatedVersion()))

	_, err = rpcClient.Dispense("nope")
	add("dispense unknown", errText(err))

	rawKV, err := rpcClient.Dispense("kv")
	if err != nil {
		add("dispense kv", errText(err))
		return out
	}
	add("dispense kv", "ok")
	kv := rawKV.(*KVClient)

	add("put a=1", errText(kv.client.Call("Plugin.Put", PutArgs{"a", "1"}, &struct{}{})))
	var got string
	err = kv.client.Call("Plugin.Get", "a", &got)
	add("get a", fmt.Sprintf("%s %q", errText(err), got))
	var missing string
	add("get zz", errText(kv.client.Call("Plugin.Get", "zz", &missing)))

	id := kv.broker.NextId()
	go kv.broker.AcceptAndServe(id, Adder{})
	var sum int
	err = kv.client.Call("Plugin.AddVia", AddViaArgs{BrokerID: id, A: 2, B: 40}, &sum)
	add("add via host broker", fmt.Sprintf("%s %d", errText(err), sum))

	var offered uint32
	err = kv.client.Call("Plugin.Offer", 7, &offered)
	if err != nil {
		add("mul via plugin broker", errText(err))
	} else {
		conn, derr := kv.broker.Dial(offered)
		if derr != nil {
			add("mul via plugin broker", errText(derr))
		} else {
			var product int
			c := rpc.NewClient(conn)
			err = c.Call("Plugin.Mul", 6, &product)
			c.Close()
			add("mul via plugin broker", fmt.Sprintf("%s %d", errText(err), product))
		}
	}

	add("print", errText(kv.client.Call("Plugin.Print", "hello-stdio", &struct{}{})))
	add("log", errText(kv.client.Call("Plugin.Log", "logged-line", &struct{}{})))
	time.Sleep(300 * time.Millisecond)
	add("stdout stream", strings.TrimSpace(stdout.String()))
	add("stderr stream", strings.TrimSpace(stderr.String()))
	var logged string
	for _, line := range strings.Split(raw.String(), "\n") {
		var entry map[string]any
		if json.Unmarshal([]byte(line), &entry) == nil && entry["@message"] == "logged-line" {
			logged = fmt.Sprintf("%v %v %v", entry["@level"], entry["@module"], entry["key"])
		}
	}
	add("stderr log line", logged)

	add("ping", errText(rpcClient.Ping()))

	reattach := client.ReattachConfig()
	client2 := plugin.NewClient(&plugin.ClientConfig{
		HandshakeConfig: handshake,
		Plugins:         plugins,
		Reattach:        reattach,
		Logger:          null,
	})
	if rpc2, err := client2.Client(); err != nil {
		add("reattach", errText(err))
	} else if raw2, err := rpc2.Dispense("kv"); err != nil {
		add("reattach", errText(err))
	} else {
		// A new connection is a new plugin instance: its map starts empty.
		var v string
		add("reattach", errText(raw2.(*KVClient).client.Call("Plugin.Get", "a", &v)))
	}

	client.Kill()
	add("kill", fmt.Sprintf("exited=%v", client.Exited()))

	bad := plugin.NewClient(&plugin.ClientConfig{
		HandshakeConfig: handshake,
		Plugins:         plugins,
		Cmd:             command(argv),
		SecureConfig:    &plugin.SecureConfig{Checksum: make([]byte, 32), Hash: sha256.New()},
		Logger:          null,
	})
	_, err = bad.Client()
	add("checksum mismatch", errText(err))
	bad.Kill()

	cookie := handshake
	cookie.MagicCookieValue = "wrong"
	wrong := plugin.NewClient(&plugin.ClientConfig{
		HandshakeConfig: cookie,
		Plugins:         plugins,
		Cmd:             command(argv),
		Logger:          null,
		StartTimeout:    5 * time.Second,
	})
	_, err = wrong.Client()
	add("wrong cookie", fmt.Sprintf("failed=%v", err != nil))
	wrong.Kill()

	versioned := plugin.NewClient(&plugin.ClientConfig{
		HandshakeConfig:  plugin.HandshakeConfig{ProtocolVersion: 2, MagicCookieKey: handshake.MagicCookieKey, MagicCookieValue: handshake.MagicCookieValue},
		VersionedPlugins: map[int]plugin.PluginSet{2: plugins},
		Cmd:              command(argv),
		Logger:           null,
	})
	_, err = versioned.Client()
	add("version mismatch", errText(err))
	versioned.Kill()
	return out
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: goplugin plugin | goplugin host <plugin> [args...]")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "plugin":
		servePlugin()
	case "host":
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		_ = enc.Encode(host(os.Args[2:]))
	default:
		fmt.Fprintln(os.Stderr, "unknown mode", os.Args[1])
		os.Exit(2)
	}
}
