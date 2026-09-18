// Command gobgen writes real plugin RPC payloads as one gob stream, plus what a dynamic decoder
// must see in them.
//
//	gobgen <outdir>   writes <outdir>/stream.gob and <outdir>/expected.json
//
// expected.json is derived by walking each Go value with reflection under gob's own
// transmission rules (encoding/gob/encode.go): zero scalars, empty strings/slices/maps, nil
// pointers and nil interfaces are omitted from a struct, while struct-kind fields and arrays are
// always sent; container elements are always sent. GobEncoder, BinaryMarshaler and TextMarshaler
// values become their marshalled bytes, preferred in that order. Interface values carry the name
// gob.Register gives the type, including its "*model.AppError" quirk (type.go, Register).
package main

import (
	"encoding"
	"encoding/base64"
	"encoding/gob"
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
)

var (
	gobEncoderT = reflect.TypeFor[gob.GobEncoder]()
	binaryT     = reflect.TypeFor[encoding.BinaryMarshaler]()
	textT       = reflect.TypeFor[encoding.TextMarshaler]()
)

// renamed mirrors the gob.RegisterName calls in public/plugin/client_rpc.go:219, whose names
// differ from what gob.Register would compute. gob keeps its registry private.
var renamed = map[string]string{"[]*model.MessageAttachment": "[]*model.SlackAttachment"}

func registeredName(rt reflect.Type) string {
	if name, ok := renamed[rt.String()]; ok {
		return name
	}
	name := rt.String()
	star := ""
	if rt.Name() == "" && rt.Kind() == reflect.Pointer {
		star = "*"
	}
	if rt.Name() != "" {
		if rt.PkgPath() == "" {
			name = star + rt.Name()
		} else {
			name = star + rt.PkgPath() + "." + rt.Name()
		}
	}
	return name
}

func marshalled(v reflect.Value) (any, bool) {
	for v.Kind() == reflect.Pointer {
		if v.IsNil() {
			return nil, false
		}
		if v.Type().Implements(gobEncoderT) || v.Type().Implements(binaryT) || v.Type().Implements(textT) {
			break
		}
		v = v.Elem()
	}
	try := func(t reflect.Type, key string, call func(any) ([]byte, error)) (any, bool) {
		var target reflect.Value
		switch {
		case v.Type().Implements(t):
			target = v
		case v.CanAddr() && v.Addr().Type().Implements(t):
			target = v.Addr()
		case reflect.PointerTo(v.Type()).Implements(t):
			p := reflect.New(v.Type())
			p.Elem().Set(v)
			target = p
		default:
			return nil, false
		}
		b, err := call(target.Interface())
		if err != nil {
			panic(err)
		}
		return map[string]any{key: base64.StdEncoding.EncodeToString(b)}, true
	}
	if r, ok := try(gobEncoderT, "$gob", func(x any) ([]byte, error) { return x.(gob.GobEncoder).GobEncode() }); ok {
		return r, true
	}
	if r, ok := try(binaryT, "$bin", func(x any) ([]byte, error) { return x.(encoding.BinaryMarshaler).MarshalBinary() }); ok {
		return r, true
	}
	return try(textT, "$text", func(x any) ([]byte, error) { return x.(encoding.TextMarshaler).MarshalText() })
}

// render returns the value tree and whether a struct would transmit the field.
func render(v reflect.Value) (any, bool) {
	if !v.IsValid() {
		return nil, false
	}
	if m, ok := marshalled(v); ok {
		return m, !v.IsZero()
	}
	switch v.Kind() {
	case reflect.Pointer:
		if v.IsNil() {
			return nil, false
		}
		return render(v.Elem())
	case reflect.Interface:
		if v.IsNil() {
			return nil, false
		}
		inner := v.Elem()
		val, _ := renderElem(inner)
		return map[string]any{"$iface": registeredName(inner.Type()), "value": val}, true
	case reflect.Struct:
		out := map[string]any{}
		for i := 0; i < v.NumField(); i++ {
			f := v.Type().Field(i)
			if !f.IsExported() || f.Type.Kind() == reflect.Chan || f.Type.Kind() == reflect.Func {
				continue
			}
			if val, ok := render(v.Field(i)); ok {
				out[f.Name] = val
			}
		}
		return out, true
	case reflect.Slice:
		if v.Type().Elem().Kind() == reflect.Uint8 {
			return map[string]any{"$bytes": base64.StdEncoding.EncodeToString(v.Bytes())}, v.Len() > 0
		}
		fallthrough
	case reflect.Array:
		out := []any{}
		for i := 0; i < v.Len(); i++ {
			val, _ := renderElem(v.Index(i))
			out = append(out, val)
		}
		return out, v.Kind() == reflect.Array || v.Len() > 0
	case reflect.Map:
		keys := v.MapKeys()
		sort.Slice(keys, func(i, j int) bool { return fmt.Sprint(keys[i]) < fmt.Sprint(keys[j]) })
		out := map[string]any{}
		for _, k := range keys {
			val, _ := renderElem(v.MapIndex(k))
			out[fmt.Sprint(k)] = val
		}
		return map[string]any{"$map": out}, v.Len() > 0
	case reflect.String:
		return v.String(), v.Len() > 0
	case reflect.Bool:
		return v.Bool(), v.Bool()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return v.Int(), v.Int() != 0
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return v.Uint(), v.Uint() != 0
	case reflect.Float32, reflect.Float64:
		return map[string]any{"$f64": math.Float64bits(v.Float())}, v.Float() != 0
	case reflect.Complex64, reflect.Complex128:
		c := v.Complex()
		return map[string]any{"$c128": []uint64{math.Float64bits(real(c)), math.Float64bits(imag(c))}}, c != 0
	}
	panic("unhandled kind " + v.Kind().String())
}

// renderElem renders a container element or top-level value, which gob always transmits.
func renderElem(v reflect.Value) (any, bool) {
	if v.Kind() == reflect.Interface && v.IsNil() {
		return map[string]any{"$iface": ""}, true
	}
	val, _ := render(v)
	return val, true
}

func main() {
	out := os.Args[1]
	remote := "remote-cluster-7"
	following := true
	u, err := url.Parse("https://example.test:8065/plugins/com.example.hello/api/v1/thing?x=1&y=two#frag")
	if err != nil {
		panic(err)
	}
	when := time.Date(2026, 9, 17, 11, 42, 7, 123456789, time.FixedZone("IST", 5*3600+1800))

	values := []struct {
		Name  string
		Value any
	}{
		{"Z_ServeHTTPArgs", &plugin.Z_ServeHTTPArgs{
			ResponseWriterStream: 7,
			RequestBodyStream:    9,
			Context:              &plugin.Context{SessionId: "sess", ConnectionId: "conn", RequestId: "req-1", IPAddress: "10.1.2.3", AcceptLanguage: "hi-IN", UserAgent: "ua/1"},
			Request: &plugin.HTTPRequestSubset{
				Method: "POST", URL: u, Proto: "HTTP/1.1", ProtoMajor: 1, ProtoMinor: 1,
				Header: http.Header{"Content-Type": {"application/json"}, "X-Multi": {"a", "b"}},
				Host:   "example.test:8065", RemoteAddr: "10.1.2.3:5555", RequestURI: "/plugins/com.example.hello/api/v1/thing?x=1&y=two",
			},
		}},
		{"Z_MessageWillBePostedArgs", &plugin.Z_MessageWillBePostedArgs{
			A: &plugin.Context{RequestId: "req-2"},
			B: &model.Post{
				Id: "postid00000000000000000001", CreateAt: 1758000000123, UpdateAt: -5, IsPinned: true,
				UserId: "user", ChannelId: "chan", Message: "héllo ✓", Type: "custom_spike",
				Props: model.StringInterface{
					"from_plugin": true,
					"count":       float64(3.5),
					"nested":      map[string]any{"list": []any{"a", float64(1), nil, map[string]any{}}},
					"attachments": []*model.MessageAttachment{{Title: "t", Color: "#ff0000", Fields: []*model.MessageAttachmentField{{Title: "f", Value: "v", Short: true}}}},
				},
				Filenames:    model.StringArray{"legacy.png"},
				FileIds:      model.StringArray{"file1", "file2"},
				RemoteId:     &remote,
				ReplyCount:   42,
				Participants: []*model.User{{Id: "p1", Username: "alice", NotifyProps: model.StringMap{"email": "true"}}},
				IsFollowing:  &following,
			},
		}},
		{"Z_OnDeactivateReturns/AppError", &plugin.Z_OnDeactivateReturns{A: &model.AppError{Id: "app.err", Message: "boom", DetailedError: "detail", StatusCode: 418, Where: "Spike.Where", SkipTranslation: true}}},
		{"Z_OnDeactivateReturns/ErrorString", &plugin.Z_OnDeactivateReturns{A: &plugin.ErrorString{Code: 2, Err: "sql: no rows in result set"}}},
		{"Z_OnDeactivateReturns/nil", &plugin.Z_OnDeactivateReturns{}},
		{"time.Time", when},
		{"negative-and-big", &struct {
			I  int64
			U  uint64
			F  float64
			C  complex128
			Ar [3]int
			Z  struct{ Q int }
		}{I: math.MinInt64, U: math.MaxUint64, F: -17.25, C: complex(1.5, -2), Ar: [3]int{0, 0, 9}}},
		{"string-singleton", "just a string"},
		{"Z_ServeHTTPArgs/again", &plugin.Z_ServeHTTPArgs{ResponseWriterStream: 11}},
	}

	f, err := os.Create(filepath.Join(out, "stream.gob"))
	if err != nil {
		panic(err)
	}
	enc := gob.NewEncoder(f)
	expected := []any{}
	for _, v := range values {
		if err := enc.Encode(v.Value); err != nil {
			panic(fmt.Sprintf("%s: %v", v.Name, err))
		}
		val, _ := renderElem(reflect.ValueOf(v.Value))
		expected = append(expected, map[string]any{"name": v.Name, "value": val})
	}
	if err := f.Close(); err != nil {
		panic(err)
	}
	b, err := json.MarshalIndent(expected, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(filepath.Join(out, "expected.json"), b, 0o644); err != nil {
		panic(err)
	}
}
