package main

// `plugingen host <fixtures> <plugin-dir> <plugin-id>`: a Go Mattermost host driving one plugin
// through the real plugin.Environment. It is the Go half of crates/mm-plugin's SDK test.
//
// The server API is plugintest's mock, set up by reflection:
//   - Every generated API method answers with its `Z_<Method>Returns` fixture.
//   - It records how Go renders the arguments it received.
//   - It records what the API server sends back, which is those returns after `encodableError`.
// The host activates the plugin, and calls every generated hook with its `Z_<Hook>Args` fixture,
// recording the returns Go received. Then it shuts the environment down.
//
// Records go to the JSON-lines file named by $PLUGINGEN_TRANSCRIPT:
//
//	{"api": "<Name>", "args": <render>, "returns": <render>}
//	{"activated": <bool>, "error": "<activation error>"}
//	{"hook": "<Name>", "returns": <render>}
//	{"shutdown": true}

import (
	"errors"
	"os"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
	"github.com/mattermost/mattermost/server/public/plugin/plugintest"
	"github.com/mattermost/mattermost/server/public/shared/mlog"
	"github.com/stretchr/testify/mock"
)

type noMetrics struct{}

func (noMetrics) ObservePluginHookDuration(string, string, bool, float64) {}
func (noMetrics) ObservePluginMultiHookIterationDuration(string, float64) {}
func (noMetrics) ObservePluginMultiHookDuration(float64)                  {}
func (noMetrics) ObservePluginAPIDuration(string, string, bool, float64)  {}

func mockAPI(c *conformance) *plugintest.API {
	api := &plugintest.API{}
	apiT := reflect.TypeFor[plugin.API]()
	for _, m := range c.idl.API {
		if m.Custom || m.Name == "LoadPluginConfiguration" {
			continue // LoadPluginConfiguration writes through its argument: mocked below.
		}
		method, _ := apiT.MethodByName(m.Name)
		returns := c.fixture(m.Returns)
		name, argsName, returnsName := m.Name, m.Args, m.Returns
		fn := reflect.MakeFunc(method.Type, func(args []reflect.Value) []reflect.Value {
			out, sent := answer(method.Type, returns)
			record(map[string]any{
				"api":     name,
				"args":    structOf(argsName, args),
				"returns": structOf(returnsName, sent),
			})
			return out
		})
		// testify matches on the number of arguments, and spreads a variadic call's values, so a
		// variadic method needs an expectation per arity the test can produce.
		arities := []int{method.Type.NumIn()}
		if method.Type.IsVariadic() {
			fixed := method.Type.NumIn() - 1
			arities = []int{fixed, fixed + 1, fixed + 2, fixed + 3, fixed + 4}
		}
		for _, arity := range arities {
			anything := make([]any, arity)
			for i := range anything {
				anything[i] = mock.Anything
			}
			call := api.On(name, anything...)
			if method.Type.NumOut() > 0 {
				call.Return(fn.Interface())
			} else {
				call.Run(func(args mock.Arguments) {
					values := callArgs(method.Type, args)
					// CallSlice, because the last value is already the variadic slice.
					if method.Type.IsVariadic() {
						fn.CallSlice(values)
					} else {
						fn.Call(values)
					}
				})
			}
		}
	}
	// LoadPluginConfiguration hands the plugin whatever the host writes into its argument, as
	// JSON; the RPC server marshals it (client_rpc.go).
	api.On("LoadPluginConfiguration", mock.Anything).Run(func(args mock.Arguments) {
		dest, ok := args.Get(0).(*any)
		if !ok {
			panic("LoadPluginConfiguration was not given a *any")
		}
		*dest = PluginConfiguration
		record(map[string]any{"api": "LoadPluginConfiguration", "config": PluginConfiguration})
	}).Return(nil)
	return api
}

// PluginConfiguration is what the host answers LoadPluginConfiguration with.
var PluginConfiguration = map[string]any{"enabled": true, "name": "conformance"}

// callArgs turns the flat arguments testify reports back into the method's own shape, gathering
// a variadic call's trailing values into the slice the function expects.
func callArgs(method reflect.Type, args mock.Arguments) []reflect.Value {
	fixed := method.NumIn()
	if method.IsVariadic() {
		fixed--
	}
	values := make([]reflect.Value, 0, fixed+1)
	for i := 0; i < fixed; i++ {
		values = append(values, reflect.ValueOf(args[i]))
	}
	if method.IsVariadic() {
		rest := reflect.MakeSlice(method.In(fixed), 0, len(args)-fixed)
		for _, a := range args[fixed:] {
			rest = reflect.Append(rest, reflect.ValueOf(a))
		}
		values = append(values, rest)
	}
	return values
}

func runHost(fixtures, pluginDir, pluginID string, idl *IDL) error {
	path := os.Getenv("PLUGINGEN_TRANSCRIPT")
	if path == "" {
		return errors.New("PLUGINGEN_TRANSCRIPT is not set")
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err != nil {
		return err
	}
	transcript.f = f

	c := &conformance{fixtures: fixtures, idl: idl}
	api := mockAPI(c)
	logger, err := mlog.NewLogger()
	if err != nil {
		return err
	}
	env, err := plugin.NewEnvironment(
		func(*model.Manifest) plugin.API { return api },
		nil, pluginDir, pluginDir+"-webapp", logger, noMetrics{},
	)
	if err != nil {
		return err
	}

	_, activated, err := env.Activate(pluginID)
	entry := map[string]any{"activated": activated}
	if err != nil {
		entry["error"] = err.Error()
	}
	record(entry)
	if err != nil {
		// A refused activation is a result to record, not a failure of the host.
		env.Shutdown()
		record(map[string]any{"shutdown": true})
		return nil
	}

	hooks, err := env.HooksForPlugin(pluginID)
	if err != nil {
		return err
	}
	hooksV := reflect.ValueOf(hooks)
	for _, m := range idl.Hooks {
		if m.Custom {
			continue
		}
		args := c.fixture(m.Args)
		in := make([]reflect.Value, args.NumField())
		for i := range in {
			in[i] = args.Field(i)
		}
		out := hooksV.MethodByName(m.Name).Call(in)
		record(map[string]any{"hook": m.Name, "returns": structOf(m.Returns, out)})
	}
	env.Shutdown()
	record(map[string]any{"shutdown": true})
	return nil
}
