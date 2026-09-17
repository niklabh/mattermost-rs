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
		if m.Excluded {
			continue
		}
		method, _ := apiT.MethodByName(m.Name)
		anything := make([]any, method.Type.NumIn())
		for i := range anything {
			anything[i] = mock.Anything
		}
		returns := c.fixture("Z_" + m.Name + "Returns")
		name := m.Name
		fn := reflect.MakeFunc(method.Type, func(args []reflect.Value) []reflect.Value {
			out, sent := answer(method.Type, returns)
			record(map[string]any{
				"api":     name,
				"args":    structOf("Z_"+name+"Args", args),
				"returns": structOf("Z_"+name+"Returns", sent),
			})
			return out
		})
		call := api.On(name, anything...)
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
	return api
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
		if m.Excluded {
			continue
		}
		args := c.fixture("Z_" + m.Name + "Args")
		in := make([]reflect.Value, args.NumField())
		for i := range in {
			in[i] = args.Field(i)
		}
		out := hooksV.MethodByName(m.Name).Call(in)
		record(map[string]any{"hook": m.Name, "returns": structOf("Z_"+m.Name+"Returns", out)})
	}
	env.Shutdown()
	record(map[string]any{"shutdown": true})
	return nil
}
