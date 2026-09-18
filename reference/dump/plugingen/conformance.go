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
	"crypto/sha256"
	"database/sql"
	"database/sql/driver"
	"encoding/gob"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	neturl "net/url"
	"os"
	"path/filepath"
	"reflect"
	"sync"

	"github.com/lib/pq"
	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
	"github.com/mattermost/mattermost/server/public/plugin/plugintest"
	"github.com/mattermost/mattermost/server/public/shared/mlog"
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

// makeAuditRecordGobSafe and makeMapGobSafe are audit.go's, copied: the JSON round trip a
// record goes through before LogAuditRec sends it. The copy is the oracle crates/mm-plugin
// checks against; the Go plugin's own calls go through the real one, so a wrong copy shows up
// as a disagreement rather than as agreement on the wrong answer.
func makeAuditRecordGobSafe(record model.AuditRecord) model.AuditRecord {
	record.EventData.Parameters = makeMapGobSafe(record.EventData.Parameters)
	record.EventData.PriorState = makeMapGobSafe(record.EventData.PriorState)
	record.EventData.ResultState = makeMapGobSafe(record.EventData.ResultState)
	record.Meta = makeMapGobSafe(record.Meta)
	return record
}

func makeMapGobSafe(m map[string]any) map[string]any {
	jsonBytes, err := json.Marshal(m)
	if err != nil {
		return map[string]any{"error": "failed to serialize audit data"}
	}
	var gobSafe map[string]any
	if err := json.Unmarshal(jsonBytes, &gobSafe); err != nil {
		return map[string]any{"error": "failed to deserialize audit data"}
	}
	return gobSafe
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
	// Each method sends its own fixture's record, which is what the test expects of it.
	rec := c.auditRecord("Z_LogAuditRecArgs")
	c.API.LogAuditRec(&rec)
	withLevel := c.auditRecord("Z_LogAuditRecWithLevelArgs")
	c.API.LogAuditRecWithLevel(&withLevel, c.auditLevel())

	c.tourStreams()
	c.tourDriver()
	c.tourOutwardHTTP()

	var config any
	err := c.API.LoadPluginConfiguration(&config)
	entry := map[string]any{"api": "LoadPluginConfiguration", "config": config}
	if err != nil {
		entry["error"] = err.Error()
	}
	record(entry)
}

// ServeHTTP and ServeMetrics answer by the rule crates/mm-plugin's tests expect: one header
// naming the request and the digest of its body, status 203, and a body naming the byte count.
func (c *conformance) ServeHTTP(_ *plugin.Context, w http.ResponseWriter, r *http.Request) {
	c.echoHTTP("ServeHTTP", w, r)
}

func (c *conformance) ServeMetrics(_ *plugin.Context, w http.ResponseWriter, r *http.Request) {
	c.echoHTTP("ServeMetrics", w, r)
}

func (c *conformance) echoHTTP(hook string, w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		panic(err)
	}
	digest := fmt.Sprintf("%x", sha256.Sum256(body))
	subset := &plugin.HTTPRequestSubset{
		Method:     r.Method,
		URL:        r.URL,
		Proto:      r.Proto,
		ProtoMajor: r.ProtoMajor,
		ProtoMinor: r.ProtoMinor,
		Header:     r.Header,
		Host:       r.Host,
		RemoteAddr: r.RemoteAddr,
		RequestURI: r.RequestURI,
	}
	rendered, _ := render(reflect.ValueOf(subset))
	record(map[string]any{
		"hook":   hook,
		"args":   rendered,
		"stream": map[string]any{"len": len(body), "sha256": digest},
	})

	w.Header().Set("X-Conformance", fmt.Sprintf("%s %s %s", r.Method, r.URL.String(), digest[:16]))
	w.WriteHeader(203)
	if _, err := fmt.Fprintf(w, "conformance: %d bytes", len(body)); err != nil {
		panic(err)
	}
}

// FileWillBeUploaded rewrites the file with the digest of what it read, as the Rust conformance
// plugin does.
func (c *conformance) FileWillBeUploaded(_ *plugin.Context, info *model.FileInfo, file io.Reader, output io.Writer) (*model.FileInfo, string) {
	uploaded, err := io.ReadAll(file)
	if err != nil {
		panic(err)
	}
	if _, err := fmt.Fprintf(output, "replaced %d bytes: %x", len(uploaded), sha256.Sum256(uploaded)); err != nil {
		panic(err)
	}
	record(map[string]any{
		"hook":   "FileWillBeUploaded",
		"stream": map[string]any{"len": len(uploaded), "sha256": fmt.Sprintf("%x", sha256.Sum256(uploaded))},
	})
	return info, ""
}

// tourDriver asks the host's database the same four questions the Rust conformance plugin does.
func (c *conformance) tourDriver() {
	conn, connErr := c.Driver.Conn(true)
	pingErr := c.Driver.ConnPing(conn)
	rows, _ := c.Driver.ConnQuery(conn, "SELECT 1", []driver.NamedValue{
		{Name: "one", Ordinal: 1, Value: int64(1)},
	})
	columns := c.Driver.RowsColumns(rows)

	entry := map[string]any{
		"driver":  "tour",
		"conn":    conn,
		"rows":    rows,
		"columns": columns,
	}
	if connErr != nil {
		entry["conn_error"] = connErr.Error()
	}
	if pingErr != nil {
		// The sentinel the host answered with, named again by decodableError.
		entry["ping_error"] = pingErr.Error()
		entry["ping_is_bad_conn"] = errors.Is(pingErr, driver.ErrBadConn)
	}
	record(entry)
}

// tourOutwardHTTP calls the host's own HTTP handler through the API.
func (c *conformance) tourOutwardHTTP() {
	url, err := neturl.Parse(ConformanceURL)
	if err != nil {
		panic(err)
	}
	response := c.API.PluginHTTP(&http.Request{
		Method:     "POST",
		URL:        url,
		Proto:      "HTTP/1.1",
		ProtoMajor: 1,
		ProtoMinor: 1,
		Header: http.Header{
			"X-Request":    []string{"one", "two"},
			"Content-Type": []string{"text/plain"},
		},
		Host:       "example.test",
		RemoteAddr: "10.0.0.1:1234",
		RequestURI: ConformanceURL,
		Body:       io.NopCloser(bytes.NewReader(StreamPayload())),
	})
	if response == nil {
		record(map[string]any{"api": "PluginHTTP", "error": "no response"})
		return
	}
	body, err := io.ReadAll(response.Body)
	if err != nil {
		panic(err)
	}
	header := map[string]any{}
	for k, v := range response.Header {
		header[k] = v
	}
	record(map[string]any{
		"api":    "PluginHTTP",
		"status": response.StatusCode,
		"header": header,
		"body":   string(body),
	})
}

// ConformanceURL is the request both conformance plugins send and serve.
const ConformanceURL = "/plugins/conformance/hello?q=1"

// auditRecord and auditLevel are the record both conformance plugins log, from the fixtures.
func (c *conformance) auditRecord(fixture string) model.AuditRecord {
	rec, ok := c.fixture(fixture).Field(0).Interface().(*model.AuditRecord)
	if !ok || rec == nil {
		panic(fixture + " has no record")
	}
	return *rec
}

func (c *conformance) auditLevel() mlog.Level {
	level, ok := c.fixture("Z_LogAuditRecWithLevelArgs").Field(1).Interface().(mlog.Level)
	if !ok {
		panic("Z_LogAuditRecWithLevelArgs has no level")
	}
	return level
}

// tourStreams calls the API methods that lend the host a reader, each with StreamPayload.
func (c *conformance) tourStreams() {
	upload := c.fixture("Z_UploadDataArgs")
	session, _ := upload.Field(0).Interface().(*model.UploadSession)
	fi, err := c.API.UploadData(session, bytes.NewReader(StreamPayload()))
	record(map[string]any{"api": "UploadData", "returns": structOf("Z_UploadDataReturns", values(fi, err))})

	install := c.fixture("Z_InstallPluginArgs")
	manifest, appErr := c.API.InstallPlugin(bytes.NewReader(StreamPayload()), install.Field(1).Bool())
	record(map[string]any{"api": "InstallPlugin", "returns": structOf("Z_InstallPluginReturns", values(manifest, appErr))})

	sync := c.fixture("Z_ReceiveSharedChannelAttachmentSyncMsgArgs")
	info, _ := sync.Field(2).Interface().(*model.FileInfo)
	synced, syncErr := c.API.ReceiveSharedChannelAttachmentSyncMsg(
		sync.Field(0).String(), sync.Field(1).String(), info, bytes.NewReader(StreamPayload()),
	)
	record(map[string]any{
		"api":     "ReceiveSharedChannelAttachmentSyncMsg",
		"returns": structOf("Z_ReceiveSharedChannelAttachmentSyncMsgReturns", values(synced, syncErr)),
	})
}

// values makes reflect.Values of a call's results, keeping a nil interface nil.
func values(results ...any) []reflect.Value {
	out := make([]reflect.Value, len(results))
	for i, r := range results {
		out[i] = reflect.ValueOf(&r).Elem().Elem()
	}
	return out
}

// StreamPayload is what every conformance stream carries: more than one 32 KiB chunk of
// io_rpc.go's copy buffer, so the framing is exercised rather than a single read.
func StreamPayload() []byte {
	payload := make([]byte, 70_000)
	for i := range payload {
		payload[i] = byte((i*31 + 7) % 251)
	}
	return payload
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
