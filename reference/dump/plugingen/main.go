// Command plugingen describes Mattermost's plugin RPC surface for crates/mm-plugin, and writes
// the gob oracle its generated types are tested against.
//
//	plugingen idl <out.json>   every hook and API method, every wire struct, every type they reach
//	plugingen gob <dir>        two gob streams per wire struct (full, sparse), plus expected.json
//	plugingen echo <dir>       decode every <Z_name>[.sparse].gob in dir as that struct; print renders
//	plugingen plugin <dir>     serve the RPC conformance plugin (conformance.go) from those fixtures
//	plugingen host <dir> <plugins> <id>   drive a plugin through plugin.Environment (host.go)
//
// Types come from reflection over plugin.API and plugin.Hooks, so they are exactly what the
// compiler sees, including instantiated generics and aliases resolved. Parameter names and doc
// comments come from parsing api.go and hooks.go.
//
// Wire structs. For a generated method the net/rpc argument is `Z_<Method>Args` with one field per
// parameter named A, B, C…, and the reply `Z_<Method>Returns` likewise per result
// (interface_generator/main.go). Rather than trust that convention, `idl` checks it against every
// `Z_` struct declared in client_rpc_generated.go and fails on any difference. The methods in
// `excludedPluginHooks` (parsed from interface_generator/main.go, not copied) have hand-written
// structs in client_rpc.go; those are reflected from the real types in `handWritten`, whose
// list is checked against the declarations in client_rpc.go.
package main

import (
	"bytes"
	"crypto/x509"
	"encoding"
	"encoding/base64"
	"encoding/gob"
	"encoding/json"
	"errors"
	"fmt"
	"go/ast"
	"go/parser"
	"go/printer"
	"go/token"
	"math"
	"math/big"
	"os"
	"path/filepath"
	"reflect"
	"regexp"
	"runtime"
	"sort"
	"strings"
	"time"

	"github.com/lib/pq"
	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
)

const (
	pluginDir = "../mattermost/server/public/plugin"
	pluginPkg = "github.com/mattermost/mattermost/server/public/plugin"
)

// handWritten is every wire struct declared in client_rpc.go.
var handWritten = []reflect.Type{
	reflect.TypeFor[plugin.Z_OnActivateArgs](),
	reflect.TypeFor[plugin.Z_OnActivateReturns](),
	reflect.TypeFor[plugin.Z_LoadPluginConfigurationArgsArgs](),
	reflect.TypeFor[plugin.Z_LoadPluginConfigurationArgsReturns](),
	reflect.TypeFor[plugin.Z_ServeHTTPArgs](),
	reflect.TypeFor[plugin.Z_PluginHTTPArgs](),
	reflect.TypeFor[plugin.Z_PluginHTTPReturns](),
	reflect.TypeFor[plugin.Z_PluginHTTPStreamArgs](),
	reflect.TypeFor[plugin.Z_PluginHTTPStreamReturns](),
	reflect.TypeFor[plugin.Z_FileWillBeUploadedArgs](),
	reflect.TypeFor[plugin.Z_FileWillBeUploadedReturns](),
	reflect.TypeFor[plugin.Z_MessageWillBePostedArgs](),
	reflect.TypeFor[plugin.Z_MessageWillBePostedReturns](),
	reflect.TypeFor[plugin.Z_MessageWillBeUpdatedArgs](),
	reflect.TypeFor[plugin.Z_MessageWillBeUpdatedReturns](),
	reflect.TypeFor[plugin.Z_MessagesWillBeConsumedArgs](),
	reflect.TypeFor[plugin.Z_MessagesWillBeConsumedReturns](),
	reflect.TypeFor[plugin.Z_MessagesWillBeConsumedWithContextArgs](),
	reflect.TypeFor[plugin.Z_MessagesWillBeConsumedWithContextReturns](),
	reflect.TypeFor[plugin.Z_LogDebugArgs](),
	reflect.TypeFor[plugin.Z_LogDebugReturns](),
	reflect.TypeFor[plugin.Z_LogInfoArgs](),
	reflect.TypeFor[plugin.Z_LogInfoReturns](),
	reflect.TypeFor[plugin.Z_LogWarnArgs](),
	reflect.TypeFor[plugin.Z_LogWarnReturns](),
	reflect.TypeFor[plugin.Z_LogErrorArgs](),
	reflect.TypeFor[plugin.Z_LogErrorReturns](),
	reflect.TypeFor[plugin.Z_LogAuditRecArgs](),
	reflect.TypeFor[plugin.Z_LogAuditRecReturns](),
	reflect.TypeFor[plugin.Z_LogAuditRecWithLevelArgs](),
	reflect.TypeFor[plugin.Z_LogAuditRecWithLevelReturns](),
	reflect.TypeFor[plugin.Z_InstallPluginArgs](),
	reflect.TypeFor[plugin.Z_InstallPluginReturns](),
	reflect.TypeFor[plugin.Z_ReceiveSharedChannelAttachmentSyncMsgArgs](),
	reflect.TypeFor[plugin.Z_ReceiveSharedChannelAttachmentSyncMsgReturns](),
	reflect.TypeFor[plugin.Z_UploadDataArgs](),
	reflect.TypeFor[plugin.Z_UploadDataReturns](),
	reflect.TypeFor[plugin.Z_ServeMetricsArgs](),
	reflect.TypeFor[plugin.Z_ChannelMemberWillBeAddedArgs](),
	reflect.TypeFor[plugin.Z_ChannelMemberWillBeAddedReturns](),
	reflect.TypeFor[plugin.Z_TeamMemberWillBeAddedArgs](),
	reflect.TypeFor[plugin.Z_TeamMemberWillBeAddedReturns](),
}

// registered mirrors client_rpc.go's init(), keyed by the argument's source text there, which
// `idl` checks. The value is what was registered; its wire name is computed as gob.Register does.
var registered = map[string]struct {
	name  string // RegisterName's explicit name, or "" for Register
	value any
}{
	`[]*model.MessageAttachment{}`:        {"[]*model.SlackAttachment", []*model.MessageAttachment{}},
	`[]any{}`:                             {"", []any{}},
	`map[string]any{}`:                    {"", map[string]any{}},
	`&model.AppError{}`:                   {"", &model.AppError{}},
	`&pq.Error{}`:                         {"", &pq.Error{}},
	`&ErrorString{}`:                      {"", &plugin.ErrorString{}},
	`&model.AutocompleteDynamicListArg{}`: {"", &model.AutocompleteDynamicListArg{}},
	`&model.AutocompleteStaticListArg{}`:  {"", &model.AutocompleteStaticListArg{}},
	`&model.AutocompleteTextArg{}`:        {"", &model.AutocompleteTextArg{}},
	`&model.PreviewPost{}`:                {"", &model.PreviewPost{}},
	`model.PropertyOptions[*model.PluginPropertyOption]{}`: {"", model.PropertyOptions[*model.PluginPropertyOption]{}},
	`[]model.PropertyOwner{}`:                              {"", []model.PropertyOwner{}},
}

// ─── the IDL ───────────────────────────────────────────────────────────────────────────────

type Param struct {
	Name string `json:"name"`
	Type string `json:"type"`
}

type Method struct {
	Name     string  `json:"name"`
	Doc      string  `json:"doc,omitempty"`
	Params   []Param `json:"params"`
	Results  []Param `json:"results"`
	Variadic bool    `json:"variadic,omitempty"`
	// Excluded: the wire structs are hand-written in client_rpc.go, not generated.
	Excluded bool `json:"excluded,omitempty"`
	// Args and Returns name the wire structs, which for a hand-written method are whatever its
	// RPC server declares — `LoadPluginConfiguration` takes a `Z_LoadPluginConfigurationArgsArgs`.
	Args    string `json:"args,omitempty"`
	Returns string `json:"returns,omitempty"`
	// NotImplemented is the error the RPC server answers with when the implementation lacks the
	// method. The hand-written ones differ from the generated ones, and from each other.
	NotImplemented string `json:"not_implemented,omitempty"`
	// Custom: a wire struct whose fields are not the plain A, B, C parameters, so neither side can
	// be generated from the signature. They carry brokered stream ids (ServeHTTP, UploadData) or a
	// reshaped request (PluginHTTP).
	Custom bool `json:"custom,omitempty"`
}

type Field struct {
	Name     string `json:"name"`
	Type     string `json:"type"`
	JSON     string `json:"json,omitempty"`
	Embedded bool   `json:"embedded,omitempty"`
}

// Type describes one Go type by its canonical id.
//
// Kind is one of: bool, int, int8, int16, int32, int64, uint, uint8, uint16, uint32, uint64,
// uintptr, float32, float64, complex64, complex128, string, slice, array, map, pointer, struct,
// interface, gob (a GobEncoder), binary (a BinaryMarshaler; time.Time is one).
type Type struct {
	Kind    string  `json:"kind"`
	Name    string  `json:"name,omitempty"`
	Package string  `json:"package,omitempty"`
	Elem    string  `json:"elem,omitempty"`
	Key     string  `json:"key,omitempty"`
	Len     int     `json:"len,omitempty"`
	Fields  []Field `json:"fields,omitempty"`
	// Methods is set for a non-empty interface.
	Methods bool `json:"methods,omitempty"`
}

type Registered struct {
	Name string `json:"name"`
	Type string `json:"type"`
}

type IDL struct {
	GoVersion string          `json:"go_version"`
	Hooks     []Method        `json:"hooks"`
	API       []Method        `json:"api"`
	Wire      []string        `json:"wire"`
	Register  []Registered    `json:"registered"`
	HookIDs   []HookID        `json:"hook_ids"`
	Types     map[string]Type `json:"types"`
}

// HookID is one `<Name>ID` constant of hooks.go: the index into a plugin's implemented-hooks
// table, part of the wire protocol. The last is `TotalHooks`, the table's size.
type HookID struct {
	Name string `json:"name"`
	ID   int    `json:"id"`
}

// hookIDs parses the constant block in hooks.go that holds OnActivateID.
func hookIDs(hooks []Method) ([]HookID, error) {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, filepath.Join(pluginDir, "hooks.go"), nil, 0)
	if err != nil {
		return nil, err
	}
	var out []HookID
	for _, d := range f.Decls {
		gd, ok := d.(*ast.GenDecl)
		if !ok || gd.Tok != token.CONST {
			continue
		}
		var block []HookID
		for i, spec := range gd.Specs {
			vs := spec.(*ast.ValueSpec)
			if len(vs.Names) != 1 || len(vs.Values) != 1 {
				return nil, fmt.Errorf("hooks.go:%d: unexpected constant shape", fset.Position(vs.Pos()).Line)
			}
			name, ok := strings.CutSuffix(vs.Names[0].Name, "ID")
			if !ok {
				block = nil
				break
			}
			id := i // iota
			switch v := vs.Values[0].(type) {
			case *ast.BasicLit:
				if _, err := fmt.Sscan(v.Value, &id); err != nil {
					return nil, err
				}
			case *ast.Ident:
				if v.Name != "iota" {
					return nil, fmt.Errorf("hooks.go: %sID = %s", name, v.Name)
				}
			default:
				return nil, fmt.Errorf("hooks.go: %sID has an unexpected value", name)
			}
			block = append(block, HookID{Name: name, ID: id})
		}
		if len(block) > 0 && block[0].Name == "OnActivate" {
			out = block
		}
	}
	if len(out) == 0 {
		return nil, errors.New("hooks.go: the hook id constants were not found")
	}
	ids := map[string]bool{}
	for i, h := range out {
		if h.ID != i {
			return nil, fmt.Errorf("hooks.go: %sID is %d, expected %d", h.Name, h.ID, i)
		}
		ids[h.Name] = true
	}
	if last := out[len(out)-1]; last.Name != "TotalHooks" {
		return nil, fmt.Errorf("hooks.go: the last hook id is %s, not TotalHooks", last.Name)
	}
	for _, m := range hooks {
		// `Implemented` is the one hook without an id: it is how the ids get their values.
		if !ids[m.Name] && m.Name != "Implemented" {
			return nil, fmt.Errorf("hook %s has no id in hooks.go", m.Name)
		}
	}
	return out, nil
}

var (
	gobEncoderT = reflect.TypeFor[gob.GobEncoder]()
	binaryT     = reflect.TypeFor[encoding.BinaryMarshaler]()
	errorT      = reflect.TypeFor[error]()
)

// typeID is a stable, unambiguous name for a type.
func typeID(t reflect.Type) string {
	if t.Name() != "" {
		if t.PkgPath() == "" {
			return t.Name() // predeclared, or `error`
		}
		return t.PkgPath() + "." + t.Name()
	}
	switch t.Kind() {
	case reflect.Pointer:
		return "*" + typeID(t.Elem())
	case reflect.Slice:
		return "[]" + typeID(t.Elem())
	case reflect.Array:
		return fmt.Sprintf("[%d]%s", t.Len(), typeID(t.Elem()))
	case reflect.Map:
		return "map[" + typeID(t.Key()) + "]" + typeID(t.Elem())
	case reflect.Interface:
		if t.NumMethod() == 0 {
			return "interface{}"
		}
		return t.String()
	default:
		return t.String()
	}
}

// sentField reports whether gob transmits a struct field (type.go, isSent).
func sentField(f reflect.StructField) bool {
	if !f.IsExported() {
		return false
	}
	t := f.Type
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	return t.Kind() != reflect.Chan && t.Kind() != reflect.Func
}

type walker struct {
	types map[string]Type
}

func (w *walker) add(t reflect.Type) string {
	id := typeID(t)
	if _, done := w.types[id]; done {
		return id
	}
	desc := Type{Name: t.Name(), Package: t.PkgPath()}
	// Reserve the id first: types may be recursive.
	w.types[id] = desc

	switch {
	// gob's userType: a GobEncoder wins over a BinaryMarshaler, on the value or its pointer.
	case t.Kind() != reflect.Interface && t.Kind() != reflect.Pointer &&
		(t.Implements(gobEncoderT) || reflect.PointerTo(t).Implements(gobEncoderT)):
		desc.Kind = "gob"
	case t.Kind() != reflect.Interface && t.Kind() != reflect.Pointer &&
		(t.Implements(binaryT) || reflect.PointerTo(t).Implements(binaryT)):
		desc.Kind = "binary"
	default:
		switch t.Kind() {
		case reflect.Pointer:
			desc.Kind = "pointer"
			desc.Elem = w.add(t.Elem())
		case reflect.Slice:
			desc.Kind = "slice"
			desc.Elem = w.add(t.Elem())
		case reflect.Array:
			desc.Kind = "array"
			desc.Len = t.Len()
			desc.Elem = w.add(t.Elem())
		case reflect.Map:
			desc.Kind = "map"
			desc.Key = w.add(t.Key())
			desc.Elem = w.add(t.Elem())
		case reflect.Struct:
			desc.Kind = "struct"
			desc.Fields = []Field{}
			for i := 0; i < t.NumField(); i++ {
				f := t.Field(i)
				if !sentField(f) {
					continue
				}
				desc.Fields = append(desc.Fields, Field{
					Name:     f.Name,
					Type:     w.add(f.Type),
					JSON:     f.Tag.Get("json"),
					Embedded: f.Anonymous,
				})
			}
		case reflect.Interface:
			desc.Kind = "interface"
			desc.Methods = t.NumMethod() > 0
		default:
			desc.Kind = t.Kind().String()
		}
	}
	w.types[id] = desc
	return id
}

// astInfo gives each method's AST, from parsing an interface's source.
func astInfo(file, iface string) (map[string]*ast.Field, error) {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, filepath.Join(pluginDir, file), nil, parser.ParseComments)
	if err != nil {
		return nil, err
	}
	out := map[string]*ast.Field{}
	ast.Inspect(f, func(n ast.Node) bool {
		ts, ok := n.(*ast.TypeSpec)
		if !ok || ts.Name.Name != iface {
			return true
		}
		for _, m := range ts.Type.(*ast.InterfaceType).Methods.List {
			if len(m.Names) == 1 {
				out[m.Names[0].Name] = m
			}
		}
		return false
	})
	return out, nil
}

func excludedList() (map[string]bool, error) {
	src, err := os.ReadFile(filepath.Join(pluginDir, "interface_generator", "main.go"))
	if err != nil {
		return nil, err
	}
	block := regexp.MustCompile(`(?s)var excludedPluginHooks = \[\]string\{(.*?)\}`).FindSubmatch(src)
	if block == nil {
		return nil, errors.New("excludedPluginHooks not found in interface_generator/main.go")
	}
	out := map[string]bool{}
	for _, m := range regexp.MustCompile(`"([A-Za-z]+)"`).FindAllSubmatch(block[1], -1) {
		out[string(m[1])] = true
	}
	return out, nil
}

func methods(w *walker, iface reflect.Type, file string, excluded map[string]bool) ([]Method, error) {
	info, err := astInfo(file, iface.Name())
	if err != nil {
		return nil, err
	}
	names := func(list *ast.FieldList) []string {
		var n []string
		if list == nil {
			return n
		}
		for _, f := range list.List {
			if len(f.Names) == 0 {
				n = append(n, "")
			}
			for _, id := range f.Names {
				n = append(n, id.Name)
			}
		}
		return n
	}
	var out []Method
	for i := 0; i < iface.NumMethod(); i++ {
		m := iface.Method(i)
		field := info[m.Name]
		if field == nil {
			return nil, fmt.Errorf("%s.%s: not found in %s", iface.Name(), m.Name, file)
		}
		ft := field.Type.(*ast.FuncType)
		pn, rn := names(ft.Params), names(ft.Results)
		method := Method{
			Name:     m.Name,
			Doc:      strings.TrimSpace(field.Doc.Text()),
			Variadic: m.Type.IsVariadic(),
			Excluded: excluded[m.Name],
			Params:   []Param{},
			Results:  []Param{},
		}
		for j := 0; j < m.Type.NumIn(); j++ {
			name := ""
			if j < len(pn) {
				name = pn[j]
			}
			method.Params = append(method.Params, Param{Name: name, Type: w.add(m.Type.In(j))})
		}
		for j := 0; j < m.Type.NumOut(); j++ {
			name := ""
			if j < len(rn) {
				name = rn[j]
			}
			method.Results = append(method.Results, Param{Name: name, Type: w.add(m.Type.Out(j))})
		}
		out = append(out, method)
	}
	return out, nil
}

// zStructs parses the Z_ struct declarations of a file: name → "Field Type" lines.
func zStructs(file string) (map[string][]string, error) {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, filepath.Join(pluginDir, file), nil, 0)
	if err != nil {
		return nil, err
	}
	declared := map[string][]string{}
	for _, d := range f.Decls {
		gd, ok := d.(*ast.GenDecl)
		if !ok {
			continue
		}
		for _, s := range gd.Specs {
			ts, ok := s.(*ast.TypeSpec)
			if !ok || !strings.HasPrefix(ts.Name.Name, "Z_") {
				continue
			}
			st, ok := ts.Type.(*ast.StructType)
			if !ok {
				continue
			}
			fields := []string{}
			for _, fl := range st.Fields.List {
				var b bytes.Buffer
				_ = printer.Fprint(&b, fset, fl.Type)
				for _, n := range fl.Names {
					fields = append(fields, n.Name+" "+b.String())
				}
			}
			declared[ts.Name.Name] = fields
		}
	}
	return declared, nil
}

// serverMethods parses client_rpc.go's hand-written RPC servers: the wire structs each method
// takes, and the error it answers when the implementation lacks it.
func serverMethods() (map[string]Method, error) {
	src, err := os.ReadFile(filepath.Join(pluginDir, "client_rpc.go"))
	if err != nil {
		return nil, err
	}
	out := map[string]Method{}
	sig := regexp.MustCompile(`func \(s \*(?:hooks|api)RPCServer\) (\w+)\(args \*(\w+), returns \*(\w+)\) error \{`)
	for _, m := range sig.FindAllSubmatchIndex(src, -1) {
		name := string(src[m[2]:m[3]])
		method := Method{
			Args:    string(src[m[4]:m[5]]),
			Returns: string(src[m[6]:m[7]]),
		}
		// The message lives in this function: search from its start to the next one.
		body := src[m[1]:]
		if next := sig.FindIndex(body); next != nil {
			body = body[:next[0]]
		}
		if msg := regexp.MustCompile(`fmt\.Errorf\("([^"]*called but not implemented[^"]*)"\)`).FindSubmatch(body); msg != nil {
			method.NotImplemented = string(msg[1])
		}
		out[name] = method
	}
	return out, nil
}

// wireTypes maps a wire struct's name to the Go type gob sees for it.
var wireTypes = map[string]reflect.Type{}

// methodWire builds the struct with the layout of a generated Z_ struct: fields A, B, C….
func methodWire(m reflect.Method, args bool) reflect.Type {
	fields := []reflect.StructField{}
	n := m.Type.NumOut()
	if args {
		n = m.Type.NumIn()
	}
	for i := 0; i < n; i++ {
		var t reflect.Type
		if args {
			t = m.Type.In(i)
		} else {
			t = m.Type.Out(i)
		}
		fields = append(fields, reflect.StructField{Name: string(rune('A' + i)), Type: t})
	}
	return reflect.StructOf(fields)
}

func checkWire(all []Method) error {
	var problems []string
	generated, err := zStructs("client_rpc_generated.go")
	if err != nil {
		return err
	}
	seen := map[string]bool{}
	for _, m := range all {
		if m.Excluded {
			continue
		}
		for _, side := range []struct {
			suffix string
			params []Param
		}{{"Args", m.Params}, {"Returns", m.Results}} {
			name := "Z_" + m.Name + side.suffix
			seen[name] = true
			fields, ok := generated[name]
			if !ok {
				problems = append(problems, name+": not declared in client_rpc_generated.go")
				continue
			}
			if len(fields) != len(side.params) {
				problems = append(problems, fmt.Sprintf("%s: %d fields declared, %d expected", name, len(fields), len(side.params)))
				continue
			}
			for i, fl := range fields {
				if want := string(rune('A' + i)); !strings.HasPrefix(fl, want+" ") {
					problems = append(problems, fmt.Sprintf("%s: field %d is %q, expected %s", name, i, fl, want))
				}
			}
		}
	}
	for name := range generated {
		if !seen[name] {
			problems = append(problems, name+": declared but matches no generated method")
		}
	}
	written, err := zStructs("client_rpc.go")
	if err != nil {
		return err
	}
	listed := map[string]bool{}
	for _, t := range handWritten {
		listed[t.Name()] = true
		fields, ok := written[t.Name()]
		if !ok {
			problems = append(problems, t.Name()+": in handWritten but not declared in client_rpc.go")
			continue
		}
		if len(fields) != t.NumField() {
			problems = append(problems, fmt.Sprintf("%s: %d fields declared, reflection sees %d", t.Name(), len(fields), t.NumField()))
		}
	}
	for name := range written {
		if !listed[name] {
			problems = append(problems, name+": declared in client_rpc.go but missing from handWritten")
		}
	}
	sort.Strings(problems)
	if len(problems) > 0 {
		return fmt.Errorf("wire structs do not match the Go source:\n  %s", strings.Join(problems, "\n  "))
	}
	return nil
}

// checkRegistered compares `registered` with the gob.Register calls in client_rpc.go's init().
func checkRegistered() error {
	src, err := os.ReadFile(filepath.Join(pluginDir, "client_rpc.go"))
	if err != nil {
		return err
	}
	calls := regexp.MustCompile(`(?m)^\s*gob\.Register(Name)?\((.*)\)\s*$`).FindAllSubmatch(src, -1)
	found := map[string]bool{}
	for _, c := range calls {
		arg := string(c[2])
		if c[1] != nil {
			parts := strings.SplitN(arg, ", ", 2)
			want, ok := registered[parts[1]]
			if !ok || fmt.Sprintf("%q", want.name) != parts[0] {
				return fmt.Errorf("client_rpc.go registers %s as %s; plugingen does not", parts[1], parts[0])
			}
			arg = parts[1]
		} else if want, ok := registered[arg]; !ok || want.name != "" {
			return fmt.Errorf("client_rpc.go registers %s; plugingen does not", arg)
		}
		found[arg] = true
	}
	for arg := range registered {
		if !found[arg] {
			return fmt.Errorf("plugingen registers %s; client_rpc.go does not", arg)
		}
	}
	return nil
}

// registeredName is the name gob.Register gives a type (type.go, Register, bug included: a
// pointer to a named type is named by its package name, not its import path).
func registeredName(rt reflect.Type) string {
	for _, r := range registered {
		if r.name != "" && reflect.TypeOf(r.value) == rt {
			return r.name
		}
	}
	if rt.Name() != "" {
		if rt.PkgPath() == "" {
			return rt.Name()
		}
		return rt.PkgPath() + "." + rt.Name()
	}
	return rt.String()
}

func buildIDL() (*IDL, error) {
	excluded, err := excludedList()
	if err != nil {
		return nil, err
	}
	if err := checkRegistered(); err != nil {
		return nil, err
	}
	w := &walker{types: map[string]Type{}}
	hooksT, apiT := reflect.TypeFor[plugin.Hooks](), reflect.TypeFor[plugin.API]()
	hooks, err := methods(w, hooksT, "hooks.go", excluded)
	if err != nil {
		return nil, err
	}
	api, err := methods(w, apiT, "api.go", excluded)
	if err != nil {
		return nil, err
	}
	all := append(append([]Method{}, hooks...), api...)
	if err := checkWire(all); err != nil {
		return nil, err
	}

	ids, err := hookIDs(hooks)
	if err != nil {
		return nil, err
	}
	idl := &IDL{GoVersion: runtime.Version(), Hooks: hooks, API: api, HookIDs: ids, Types: w.types}
	written, err := serverMethods()
	if err != nil {
		return nil, err
	}
	// A wire struct whose fields are not the plain A, B, C parameters is not generatable.
	custom := func(name string) bool {
		t, ok := wireTypes[name]
		if !ok {
			return false
		}
		for i := 0; i < t.NumField(); i++ {
			if t.Field(i).Name != string(rune('A'+i)) {
				return true
			}
		}
		return false
	}
	for _, m := range all {
		if m.Excluded {
			continue
		}
		rm, ok := hooksT.MethodByName(m.Name)
		if !ok {
			rm, _ = apiT.MethodByName(m.Name)
		}
		for _, side := range []struct {
			suffix string
			args   bool
			params []Param
		}{{"Args", true, m.Params}, {"Returns", false, m.Results}} {
			name := "Z_" + m.Name + side.suffix
			wireTypes[name] = methodWire(rm, side.args)
			fields := []Field{}
			for i, p := range side.params {
				fields = append(fields, Field{Name: string(rune('A' + i)), Type: p.Type})
			}
			w.types[pluginPkg+"."+name] = Type{Kind: "struct", Name: name, Package: pluginPkg, Fields: fields}
			idl.Wire = append(idl.Wire, pluginPkg+"."+name)
		}
	}
	for _, t := range handWritten {
		wireTypes[t.Name()] = t
		idl.Wire = append(idl.Wire, w.add(t))
	}
	for _, side := range []struct {
		kind    string
		methods []Method
	}{{"Hook", idl.Hooks}, {"API", idl.API}} {
		for i := range side.methods {
			m := &side.methods[i]
			if m.Excluded {
				hand, ok := written[m.Name]
				if !ok {
					// ServeHTTP, ServeMetrics and Implemented: their RPC servers do not take the
					// (args, returns) pair at all.
					m.Custom = true
					continue
				}
				m.Args, m.Returns, m.NotImplemented = hand.Args, hand.Returns, hand.NotImplemented
			} else {
				m.Args, m.Returns = "Z_"+m.Name+"Args", "Z_"+m.Name+"Returns"
				// interface_generator/main.go's template.
				m.NotImplemented = fmt.Sprintf("%s %s called but not implemented.", side.kind, m.Name)
			}
			m.Custom = custom(m.Args) || custom(m.Returns)
		}
	}
	for _, r := range registered {
		rt := reflect.TypeOf(r.value)
		idl.Register = append(idl.Register, Registered{Name: registeredName(rt), Type: w.add(rt)})
	}
	sort.Strings(idl.Wire)
	sort.Slice(idl.Register, func(i, j int) bool { return idl.Register[i].Name < idl.Register[j].Name })
	return idl, nil
}

// ─── the gob oracle ────────────────────────────────────────────────────────────────────────

func seed(path string) uint64 {
	var h uint64 = 1469598103934665603
	for i := 0; i < len(path); i++ {
		h ^= uint64(path[i])
		h *= 1099511628211
	}
	return h
}

// special builds values of types whose zero value does not marshal, or whose state is unexported.
var special = map[reflect.Type]func(s uint64) any{
	reflect.TypeFor[x509.OID](): func(s uint64) any {
		oid, err := x509.OIDFromInts([]uint64{1, 3, 6, 1 + s%1000})
		if err != nil {
			panic(err)
		}
		return oid
	},
	reflect.TypeFor[big.Int](): func(s uint64) any { return *new(big.Int).SetUint64(1 + s%1_000_000) },
}

// populator fills every field gob sends with a distinctive non-zero value derived from its path.
// A recursive type is filled one level deep: the struct inside itself is filled, and that one's
// recursive fields are left zero.
//
// Integers sit near the top of their type's range, and signed ones are negative, so a Rust field
// one size too narrow, or unsigned, fails to decode rather than passing on a small value.
//
// A sparse populator leaves about half the pointers, interfaces, slices and maps nil, chosen by
// path. The full one proves every field crosses; the sparse one proves a nil stays nil, which is
// where `Option<T>` and `T` differ on the wire.
type populator struct {
	active map[reflect.Type]int
	sparse bool
}

// base is the type a type leads to through pointers, slices, arrays and map elements.
func base(t reflect.Type) reflect.Type {
	for {
		switch t.Kind() {
		case reflect.Pointer, reflect.Slice, reflect.Array, reflect.Map:
			t = t.Elem()
		default:
			return t
		}
	}
}

func (p *populator) fill(v reflect.Value, path string, depth int) {
	t := v.Type()
	s := seed(path)
	if t == reflect.TypeFor[time.Time]() {
		// Zone offsets vary by path: east, west, and IST's half hour.
		zone := time.FixedZone("", []int{19800, -18000, 3600}[s%3])
		v.Set(reflect.ValueOf(time.UnixMilli(1_700_000_000_000 + int64(s%1_000_000_000)).In(zone)))
		return
	}
	if make, ok := special[t]; ok {
		v.Set(reflect.ValueOf(make(s)))
		return
	}
	if depth > 16 {
		return
	}
	if b := base(t); b.Kind() == reflect.Struct && t.Kind() != reflect.Struct && p.active[b] > 1 {
		return
	}
	switch t.Kind() {
	case reflect.Pointer, reflect.Interface, reflect.Slice, reflect.Map:
		if p.sparse && s%2 == 1 {
			return
		}
	}
	switch t.Kind() {
	case reflect.Pointer:
		v.Set(reflect.New(t.Elem()))
		p.fill(v.Elem(), path, depth+1)
	case reflect.Struct:
		p.active[t]++
		defer func() { p.active[t]-- }()
		for i := 0; i < t.NumField(); i++ {
			if f := t.Field(i); sentField(f) {
				p.fill(v.Field(i), path+"."+f.Name, depth+1)
			}
		}
		if t == reflect.TypeFor[model.AuditRecord]() {
			fillAuditMaps(v)
		}
	case reflect.String:
		v.SetString(fmt.Sprintf("s%06x", s%0xffffff))
	case reflect.Bool:
		v.SetBool(true)
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		v.SetInt(-(int64(1)<<(t.Bits()-2) + int64(s%100)))
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		v.SetUint(uint64(1)<<(t.Bits()-1) + s%100)
	case reflect.Float32, reflect.Float64:
		v.SetFloat(float64(1+s%1000) / 8)
	case reflect.Complex64, reflect.Complex128:
		v.SetComplex(complex(float64(1+s%10), 2))
	case reflect.Slice:
		if t.Elem().Kind() == reflect.Uint8 {
			v.SetBytes([]byte(fmt.Sprintf("b%04x", s%0xffff)))
			return
		}
		sl := reflect.MakeSlice(t, 1, 1)
		p.fill(sl.Index(0), path+"[0]", depth+1)
		v.Set(sl)
	case reflect.Array:
		for i := 0; i < v.Len(); i++ {
			p.fill(v.Index(i), fmt.Sprintf("%s[%d]", path, i), depth+1)
		}
	case reflect.Map:
		m := reflect.MakeMap(t)
		k := reflect.New(t.Key()).Elem()
		p.fill(k, path+"{k}", depth+1)
		e := reflect.New(t.Elem()).Elem()
		p.fill(e, path+"{v}", depth+1)
		m.SetMapIndex(k, e)
		v.Set(m)
	case reflect.Interface:
		p.fillInterface(v, path, depth, s)
	}
}

// fillInterface puts a concrete value of a type the plugin RPC registers into an interface.
func (p *populator) fillInterface(v reflect.Value, path string, depth int, s uint64) {
	t := v.Type()
	var candidates []reflect.Type
	switch {
	case t == errorT:
		candidates = []reflect.Type{
			reflect.TypeFor[*model.AppError](),
			reflect.TypeFor[*plugin.ErrorString](),
			reflect.TypeFor[*pq.Error](),
		}
	case t.NumMethod() == 0:
		// What map[string]any holds after JSON, plus the RPC's own registrations.
		candidates = []reflect.Type{
			reflect.TypeFor[string](),
			reflect.TypeFor[float64](),
			reflect.TypeFor[bool](),
			reflect.TypeFor[[]any](),
			reflect.TypeFor[map[string]any](),
			reflect.TypeFor[int64](),
			reflect.TypeFor[*model.AutocompleteDynamicListArg](),
			reflect.TypeFor[*model.AutocompleteStaticListArg](),
			reflect.TypeFor[*model.AutocompleteTextArg](),
			reflect.TypeFor[*model.PreviewPost](),
			reflect.TypeFor[[]*model.MessageAttachment](),
			reflect.TypeFor[model.PropertyOptions[*model.PluginPropertyOption]](),
			reflect.TypeFor[[]model.PropertyOwner](),
		}
		if depth > 6 {
			candidates = candidates[:3]
		}
	default:
		return // io.ReadCloser and the like: never sent with a value.
	}
	ct := candidates[s%uint64(len(candidates))]
	cv := reflect.New(ct).Elem()
	p.fill(cv, path+"(i)", depth+1)
	v.Set(cv)
}

// fillAuditMaps puts values in an audit record's `map[string]any` fields that the JSON round trip
// of `makeAuditRecordGobSafe` visibly changes: an integer becomes a float, a struct becomes an
// object keyed by its `json:` tags, a nil stays nil. Filled by kind, they would all survive it
// unchanged and the oracle would prove nothing.
func fillAuditMaps(record reflect.Value) {
	maps := []reflect.Value{
		record.FieldByName("EventData").FieldByName("Parameters"),
		record.FieldByName("EventData").FieldByName("PriorState"),
		record.FieldByName("EventData").FieldByName("ResultState"),
		record.FieldByName("Meta"),
	}
	for i, m := range maps {
		m.Set(reflect.ValueOf(map[string]any{
			"int":    int64(1<<40) + int64(i),
			"float":  1.5,
			"string": fmt.Sprintf("value %d", i),
			"bool":   i%2 == 0,
			"nil":    nil,
			"app_error": &model.AppError{
				Id:            "api.audit.error",
				Message:       "audit message",
				DetailedError: "detail",
				StatusCode:    500,
				Where:         "Audit",
			},
			"attachments": []*model.MessageAttachment{{
				Text:   "attached",
				Fields: []*model.SlackAttachmentField{{Title: "title", Value: "field value"}},
			}},
			"list":   []any{int64(7), "two", false},
			"nested": map[string]any{"inner": int64(9)},
		}))
	}
}

// render is the gob oracle's canonical form (reference/dump/gob, render): what a value transmits.
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
			return map[string]any{"$iface": ""}, false
		}
		inner := v.Elem()
		val, _ := render(inner)
		return map[string]any{"$iface": registeredName(inner.Type()), "value": val}, true
	case reflect.Struct:
		out := map[string]any{}
		for i := 0; i < v.NumField(); i++ {
			if !sentField(v.Type().Field(i)) {
				continue
			}
			if val, ok := render(v.Field(i)); ok {
				out[v.Type().Field(i).Name] = val
			}
		}
		return out, true
	case reflect.Slice:
		if v.Type().Elem().Kind() == reflect.Uint8 {
			return map[string]any{"$bytes": base64.StdEncoding.EncodeToString(v.Bytes())}, v.Len() > 0
		}
		out := []any{}
		for i := 0; i < v.Len(); i++ {
			val, _ := render(v.Index(i))
			out = append(out, val)
		}
		return out, v.Len() > 0
	case reflect.Array:
		out := []any{}
		for i := 0; i < v.Len(); i++ {
			val, _ := render(v.Index(i))
			out = append(out, val)
		}
		return out, true
	case reflect.Map:
		keys := v.MapKeys()
		sort.Slice(keys, func(i, j int) bool { return fmt.Sprint(keys[i]) < fmt.Sprint(keys[j]) })
		out := map[string]any{}
		for _, k := range keys {
			val, _ := render(v.MapIndex(k))
			out[fmt.Sprint(k)] = val
		}
		return map[string]any{"$map": out}, !v.IsNil()
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

// marshalled renders a GobEncoder or BinaryMarshaler as the bytes it sends.
func marshalled(v reflect.Value) (any, bool) {
	t := v.Type()
	if t.Kind() == reflect.Interface || t.Kind() == reflect.Pointer {
		return nil, false
	}
	ptr := reflect.New(t)
	ptr.Elem().Set(v)
	switch {
	case ptr.Type().Implements(gobEncoderT):
		b, err := ptr.Interface().(gob.GobEncoder).GobEncode()
		if err != nil {
			panic(err)
		}
		return map[string]any{"$gob": base64.StdEncoding.EncodeToString(b)}, true
	case ptr.Type().Implements(binaryT):
		b, err := ptr.Interface().(encoding.BinaryMarshaler).MarshalBinary()
		if err != nil {
			panic(err)
		}
		return map[string]any{"$bin": base64.StdEncoding.EncodeToString(b)}, true
	}
	return nil, false
}

func writeJSON(path string, v any) error {
	out, err := json.MarshalIndent(v, "", " ")
	if err != nil {
		return err
	}
	if path == "-" {
		_, err = os.Stdout.Write(append(out, '\n'))
		return err
	}
	return os.WriteFile(path, append(out, '\n'), 0o644)
}

func decodeRender(name string, stream []byte) (any, error) {
	t, ok := wireTypes[name]
	if !ok {
		return nil, fmt.Errorf("no wire struct %s", name)
	}
	v := reflect.New(t)
	if err := gob.NewDecoder(bytes.NewReader(stream)).Decode(v.Interface()); err != nil {
		return nil, fmt.Errorf("%s: %w", name, err)
	}
	out, _ := render(v.Elem())
	return out, nil
}

// writeStable writes a stream unless the file already holds one that decodes the same way.
//
// Go encodes a map in its randomised iteration order, so a stream carrying a map with more than
// one key differs on every run. Keeping a file that still renders identically leaves the
// generator deterministic in git terms (reference/dump/gob does the same).
func writeStable(path, name string, stream []byte) error {
	if old, err := os.ReadFile(path); err == nil && !bytes.Equal(old, stream) {
		want, errWant := decodeRender(name, stream)
		got, errGot := decodeRender(name, old)
		if errWant == nil && errGot == nil {
			a, _ := json.Marshal(want)
			b, _ := json.Marshal(got)
			if bytes.Equal(a, b) {
				return nil
			}
		}
	}
	return os.WriteFile(path, stream, 0o644)
}

// gobOracle writes <dir>/<Z_name>.gob and <dir>/<Z_name>.sparse.gob for every wire struct, and
// <dir>/expected.json with how Go renders each after decoding it back.
func gobOracle(dir string) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	names := make([]string, 0, len(wireTypes))
	for name := range wireTypes {
		names = append(names, name)
	}
	sort.Strings(names)
	expected := map[string]any{}
	for _, name := range names {
		for _, sparse := range []bool{false, true} {
			key := name
			if sparse {
				key += ".sparse"
			}
			v := reflect.New(wireTypes[name]).Elem()
			(&populator{active: map[reflect.Type]int{}, sparse: sparse}).fill(v, name, 0)
			var buf bytes.Buffer
			if err := gob.NewEncoder(&buf).Encode(v.Addr().Interface()); err != nil {
				return fmt.Errorf("%s: %w", key, err)
			}
			if err := writeStable(filepath.Join(dir, key+".gob"), name, buf.Bytes()); err != nil {
				return err
			}
			written, err := os.ReadFile(filepath.Join(dir, key+".gob"))
			if err != nil {
				return err
			}
			r, err := decodeRender(name, written)
			if err != nil {
				return err
			}
			expected[key] = r
		}

		// The audit arguments also get the form LogAuditRec sends: the record after its JSON
		// round trip (audit.go, `makeAuditRecordGobSafe`).
		if field, ok := wireTypes[name].FieldByName("A"); ok && field.Type == reflect.TypeFor[*model.AuditRecord]() {
			v := reflect.New(wireTypes[name]).Elem()
			(&populator{active: map[reflect.Type]int{}}).fill(v, name, 0)
			rec, ok := v.Field(0).Interface().(*model.AuditRecord)
			if !ok || rec == nil {
				return fmt.Errorf("%s: no audit record to make gob-safe", name)
			}
			safe := makeAuditRecordGobSafe(*rec)
			v.Field(0).Set(reflect.ValueOf(&safe))
			var buf bytes.Buffer
			if err := gob.NewEncoder(&buf).Encode(v.Addr().Interface()); err != nil {
				return fmt.Errorf("%s.safe: %w", name, err)
			}
			if err := writeStable(filepath.Join(dir, name+".safe.gob"), name, buf.Bytes()); err != nil {
				return err
			}
			written, err := os.ReadFile(filepath.Join(dir, name+".safe.gob"))
			if err != nil {
				return err
			}
			r, err := decodeRender(name, written)
			if err != nil {
				return err
			}
			expected[name+".safe"] = r
		}
	}
	return writeJSON(filepath.Join(dir, "expected.json"), expected)
}

// echo decodes every <Z_name>.gob in dir (written by Rust) and prints Go's renders, or the error
// for each stream Go refused.
func echo(dir string) error {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return err
	}
	out := map[string]any{}
	for _, e := range entries {
		name, ok := strings.CutSuffix(e.Name(), ".gob")
		if !ok {
			continue
		}
		stream, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			return err
		}
		r, err := decodeRender(strings.TrimSuffix(strings.TrimSuffix(name, ".sparse"), ".safe"), stream)
		if err != nil {
			out[name] = map[string]any{"$error": err.Error()}
			continue
		}
		out[name] = r
	}
	return writeJSON("-", out)
}

func main() {
	if len(os.Args) < 3 || (os.Args[1] == "host") != (len(os.Args) == 5) {
		fmt.Fprintln(os.Stderr, "usage: plugingen idl <out.json> | gob <dir> | echo <dir> | plugin <fixtures> | host <fixtures> <plugin-dir> <plugin-id>")
		os.Exit(2)
	}
	idl, err := buildIDL()
	if err == nil {
		switch os.Args[1] {
		case "idl":
			err = writeJSON(os.Args[2], idl)
		case "gob":
			err = gobOracle(os.Args[2])
		case "echo":
			err = echo(os.Args[2])
		case "plugin":
			err = servePlugin(os.Args[2], idl)
		case "host":
			err = runHost(os.Args[2], os.Args[3], os.Args[4], idl)
		default:
			err = fmt.Errorf("unknown mode %q", os.Args[1])
		}
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "plugingen:", err)
		os.Exit(1)
	}
}
