package main

// `plugingen plugin <fixtures>`: a real Mattermost plugin, served by plugin.ClientMain, that
// answers every generated hook and calls every generated API method with the oracle's values.
//
// It is the Go half of the RPC conformance test for crates/mm-plugin. The hooks are the
// mockery mock in plugintest, set up by reflection:
//   - Every generated hook answers with its `Z_<Hook>Returns` fixture, decoded from <fixtures>.
//   - It records how Go renders the arguments it received.
//   - It records what the plugin's RPC server sends back, which is those returns after
//     `encodableError`.
// OnActivate is overridden to tour the API: every generated API method is called with its
// `Z_<Method>Args` fixture, and it records how Go renders the returns that came back.
//
// Records go to the JSON-lines file named by $PLUGINGEN_TRANSCRIPT, one object per line:
//
//	{"hook": "<Name>", "args": <render>, "returns": <render>}
//	{"api": "<Name>", "returns": <render>}
//	{"activated": true}

import (
	"bytes"
	"database/sql"
	"database/sql/driver"
	"encoding/gob"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"sync"

	"github.com/lib/pq"
	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
	"github.com/mattermost/mattermost/server/public/plugin/plugintest"
	"github.com/stretchr/testify/mock"
)

type conformance struct {
	plugin.MattermostPlugin
	*plugintest.Hooks

	fixtures string
	idl      *IDL
}

var transcript struct {
	sync.Mutex
	f *os.File
}

func record(entry map[string]any) {
	b, err := json.Marshal(entry)
	if err != nil {
		panic(err)
	}
	transcript.Lock()
	defer transcript.Unlock()
	if _, err := transcript.f.Write(append(b, '\n')); err != nil {
		panic(err)
	}
}

// fixture decodes the full oracle stream of a wire struct.
func (c *conformance) fixture(name string) reflect.Value {
	stream, err := os.ReadFile(filepath.Join(c.fixtures, name+".gob"))
	if err != nil {
		panic(err)
	}
	v := reflect.New(wireTypes[name])
	if err := gob.NewDecoder(bytes.NewReader(stream)).Decode(v.Interface()); err != nil {
		panic(fmt.Sprintf("%s: %v", name, err))
	}
	return v.Elem()
}

// structOf packs values into a wire struct, so they render as the struct would.
func structOf(name string, values []reflect.Value) any {
	v := reflect.New(wireTypes[name]).Elem()
	for i, x := range values {
		if !x.IsValid() || (x.Kind() == reflect.Interface && x.IsNil()) {
			continue
		}
		v.Field(i).Set(x)
	}
	r, _ := render(v)
	return r
}

// encodableError is client_rpc.go's, copied: the transformation the hooks server applies to
// every `error` a hook returns.
func encodableError(err error) error {
	if err == nil {
		return nil
	}
	if _, ok := err.(*model.AppError); ok {
		return err
	}
	if _, ok := err.(*pq.Error); ok {
		return err
	}
	ret := &plugin.ErrorString{Err: err.Error()}
	switch err {
	case io.EOF:
		ret.Code = 1
	case sql.ErrNoRows:
		ret.Code = 2
	case sql.ErrConnDone:
		ret.Code = 3
	case sql.ErrTxDone:
		ret.Code = 4
	case driver.ErrSkip:
		ret.Code = 5
	case driver.ErrBadConn:
		ret.Code = 6
	case driver.ErrRemoveArgument:
		ret.Code = 7
	}
	return ret
}

// partialPostMessage is the one field the plugin sets in its MessageWillBePosted answer.
const partialPostMessage = "edited by the conformance plugin"

// answer is what a mocked method returns (the fixture's fields), and what Go's RPC server then
// sends: the same, with every `error` passed through `encodableError`.
func answer(method reflect.Type, returns reflect.Value) (out, sent []reflect.Value) {
	out = make([]reflect.Value, method.NumOut())
	sent = make([]reflect.Value, len(out))
	for i := range out {
		out[i] = returns.Field(i)
		sent[i] = out[i]
		if method.Out(i) == errorT {
			err, _ := out[i].Interface().(error)
			sent[i] = reflect.ValueOf(&err).Elem()
			if enc := encodableError(err); enc != nil {
				sent[i] = reflect.ValueOf(&enc).Elem()
			}
		}
	}
	return out, sent
}

func (c *conformance) setUpHooks() {
	hooksT := reflect.TypeFor[plugin.Hooks]()
	for _, m := range c.idl.Hooks {
		if m.Custom {
			continue
		}
		method, _ := hooksT.MethodByName(m.Name)
		argsName, returnsName := m.Args, m.Returns
		anything := make([]any, method.Type.NumIn())
		for i := range anything {
			anything[i] = mock.Anything
		}
		returns := c.fixture(m.Returns)
		if m.Name == "MessageWillBePosted" {
			// Answer with a post that carries one field, to show the host's merge: every other
			// field of its answer must come from the post it sent (client_rpc.go).
			returns = reflect.New(wireTypes[m.Returns]).Elem()
			returns.Field(0).Set(reflect.ValueOf(&model.Post{Message: partialPostMessage}))
		}
		name := m.Name
		fn := reflect.MakeFunc(method.Type, func(args []reflect.Value) []reflect.Value {
			out, sent := answer(method.Type, returns)
			record(map[string]any{
				"hook":    name,
				"args":    structOf(argsName, args),
				"returns": structOf(returnsName, sent),
			})
			return out
		})
		call := c.Hooks.On(name, anything...)
		if method.Type.NumOut() > 0 {
			call.Return(fn.Interface())
		} else {
			call.Run(func(args mock.Arguments) {
				values := make([]reflect.Value, len(args))
				for i, a := range args {
					values[i] = reflect.ValueOf(a)
				}
				fn.Call(values)
			})
		}
	}
}

// OnActivate tours the API, then activates.
func (c *conformance) OnActivate() error {
	apiT := reflect.TypeFor[plugin.API]()
	api := reflect.ValueOf(c.API)
	for _, m := range c.idl.API {
		if m.Custom || m.Excluded {
			continue
		}
		args := c.fixture(m.Args)
		in := make([]reflect.Value, args.NumField())
		for i := range in {
			in[i] = args.Field(i)
		}
		method, _ := apiT.MethodByName(m.Name)
		var out []reflect.Value
		if method.Type.IsVariadic() {
			out = api.MethodByName(m.Name).CallSlice(in)
		} else {
			out = api.MethodByName(m.Name).Call(in)
		}
		record(map[string]any{"api": m.Name, "returns": structOf(m.Returns, out)})
	}
	c.tourHandWrittenAPI()
	record(map[string]any{"activated": true})
	return nil
}

// tourHandWrittenAPI calls the API methods whose clients Go writes by hand, with the values
// crates/mm-plugin's test expects.
func (c *conformance) tourHandWrittenAPI() {
	c.API.LogDebug(LogMessage, LogPairs...)
	c.API.LogInfo(LogMessage, LogPairs...)
	c.API.LogWarn(LogMessage, LogPairs...)
	c.API.LogError(LogMessage, LogPairs...)
	var config any
	err := c.API.LoadPluginConfiguration(&config)
	entry := map[string]any{"api": "LoadPluginConfiguration", "config": config}
	if err != nil {
		entry["error"] = err.Error()
	}
	record(entry)
}

// LogMessage and LogPairs are what both conformance plugins send to the log methods. Go
// stringifies the pairs with %+v before they cross (stringifier.go), so they arrive as strings.
var (
	LogMessage = "a logged line"
	LogPairs   = []any{"key", 42, true}
)

func servePlugin(fixtures string, idl *IDL) error {
	path := os.Getenv("PLUGINGEN_TRANSCRIPT")
	if path == "" {
		return errors.New("PLUGINGEN_TRANSCRIPT is not set")
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err != nil {
		return err
	}
	transcript.f = f
	c := &conformance{Hooks: &plugintest.Hooks{}, fixtures: fixtures, idl: idl}
	c.setUpHooks()
	plugin.ClientMain(c)
	return nil
}
