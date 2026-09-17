// Command gob is the parity oracle for crates/gobwire: Go's own encoding/gob, asked the
// questions the Rust implementation has to answer the same way.
//
//	gob gen <dir>                          write <dir>/<case>.gob streams and <dir>/cases.json
//	gob echo <decodeAs> [base.gob] < in    decode a stream Rust wrote, print what Go got
//
// cases.json holds, per case, the stream file(s), the Go type the stream is decoded into, and
// what Go's decoder produced: either the rendered values or the error. Rendering follows gob's
// own transmission rules (encoding/gob/encode.go), so "what the wire carries" and "what Go
// decoded" are comparable as JSON:
//
//   - struct: an object of the fields gob would send — zero scalars, empty strings/slices, nil
//     pointers, nil maps and nil interfaces are left out; struct-kind fields and arrays are kept,
//     and an empty non-nil map is kept;
//   - map: {"$map": {fmt.Sprint(key): value}}; slice and array: a list; []byte: {"$bytes": b64};
//   - float: {"$f64": IEEE bits}; complex: {"$c128": [re bits, im bits]};
//   - interface: {"$iface": registered name, "value": ...}, nil as {"$iface": ""};
//   - GobEncoder / BinaryMarshaler: {"$gob"|"$bin": b64 of the bytes}. TextMarshaler is not used
//     by gob at all (type.go:85-89), whatever its package documentation says.
//
// Every value is fully populated with distinctive values, per CLAUDE.md: a zero field proves
// nothing about the fields most likely to drift.
package main

import (
	"bytes"
	"encoding"
	"encoding/base64"
	"encoding/gob"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"math/big"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"time"
)

// ─── the types ─────────────────────────────────────────────────────────────────────────────

type Scalars struct {
	B       bool
	I       int
	I8      int8
	I16     int16
	I32     int32
	I64     int64
	U       uint
	U8      uint8
	U16     uint16
	U32     uint32
	U64     uint64
	Uptr    uintptr
	F32     float32
	F64     float64
	C128    complex128
	S       string
	Bytes   []byte
	private int // never sent
}

type Inner struct {
	N int
	S string
}

// Val is registered by value, Inner by pointer: gob refuses one base type under two names.
type Val struct {
	Label string
	Count uint16
}

type Zeros struct {
	B     bool
	I     int
	F     float64
	S     string
	Bytes []byte
	P     *int
	PZ    *int
	PS    *Inner
	E     Inner
	A     [2]int
	M     map[string]int
	ME    map[string]int
	SL    []int
	IF    any
	Last  string
}

type Nested struct {
	Ptr      *Inner
	Val      Inner
	Slice    []Inner
	PtrSlice []*Inner
	Arr      [3]Inner
	Map      map[string]Inner
	IntMap   map[int64]string
	PtrMap   map[string]*Inner
	Matrix   [][]int
	Blobs    [][]byte
	Strings  []string
	Floats   []float64
	Bools    [2]bool
}

type Ifaces struct {
	S    any
	F    any
	I    any
	M    any
	L    any
	P    any
	V    any
	Err  error
	Nest any
	T    any
	Nil  any
}

type AppError struct {
	Id            string
	Message       string
	DetailedError string
	StatusCode    int
	Where         string
}

func (e *AppError) Error() string { return e.Message }

// Celsius is a TextMarshaler, which gob ignores: it is sent as a float64.
type Celsius float64

func (c Celsius) MarshalText() ([]byte, error) { return []byte(fmt.Sprintf("%.2fC", float64(c))), nil }
func (c *Celsius) UnmarshalText(b []byte) error {
	_, err := fmt.Sscanf(string(b), "%fC", (*float64)(c))
	return err
}

type Marshalers struct {
	T     time.Time
	TZ    time.Time
	TL    time.Time
	TNeg  time.Time
	U     url.URL
	P     *url.URL
	Txt   Celsius
	Big   *big.Int
	ZeroT time.Time
	// The zero instant in a fixed zone is not the zero Time: its location is non-nil, so gob
	// sends it.
	ZeroFixed time.Time
	After     string
}

type Recursive struct {
	Name string
	Kids []*Recursive
	Next *Recursive
}

// Wide carries every kind the decoder has to skip; Narrow keeps two of its fields.
type Wide struct {
	Keep      string
	Nums      []int64
	Blob      []byte
	Tree      *Recursive
	Map       map[string][]Inner
	Iface     any
	NilsIface []any
	Time      time.Time
	Arr       [2]Inner
	C         complex128
	Last      int
}

type Narrow struct {
	Keep string
	Last int
}

// NarrowNoNils is Wide minus the []any that carries nil elements.
type WideNoNils struct {
	Keep  string
	Nums  []int64
	Blob  []byte
	Tree  *Recursive
	Map   map[string][]Inner
	Iface any
	Time  time.Time
	Arr   [2]Inner
	C     complex128
	Last  int
}

type Small struct {
	I8  int8
	U8  uint8
	F32 float32
}

type Big struct {
	I8  int64
	U8  uint64
	F32 float64
}

type U16s struct{ N uint16 }
type U64s struct{ N uint64 }

type ArrSmall struct{ A [2]int }
type ArrBig struct{ A [3]int }

type Unrelated struct {
	Nothing  string
	InCommon int
}

type Ints struct{ N int }
type Uints struct{ N uint }
type Strs struct{ N string }
type Floats struct{ N float64 }
type IntSlice struct{ N []int }
type IntArr struct{ N [1]int }
type StrMap struct{ N map[string]int }
type IntMap struct{ N map[int]int }
type IfaceField struct{ N any }
type StructField struct{ N Inner }

// Post mirrors the fields of model.Post that exercise MessageWillBePosted's decode-into-original.
type Post struct {
	Id       string
	CreateAt int64
	Message  string
	Props    map[string]any
	FileIds  []string
	RemoteId *string
	Pinned   bool
	Metadata *Meta
	ReplyTo  []Inner
}

type Meta struct {
	Emojis []Inner
	Count  int
}

// Merge exercises decode-into-existing for every container.
type Merge struct {
	Keep   string
	Change string
	Ptr    *Inner
	Slice  []Inner
	Short  []Inner
	Arr    [2]Inner
	Map    map[string]Inner
	Iface  any
	Bytes  []byte
	Nested Inner
}

func init() {
	gob.Register(&Inner{})
	gob.Register(Val{})
	gob.Register(&AppError{})
	gob.Register(map[string]any{})
	gob.Register([]any{})
	gob.Register(time.Time{})
	gob.RegisterName("[]*main.LegacyName", []*Val{})
}

// ─── rendering ─────────────────────────────────────────────────────────────────────────────

var (
	gobEncoderT = reflect.TypeFor[gob.GobEncoder]()
	binaryT     = reflect.TypeFor[encoding.BinaryMarshaler]()
)

// renamed mirrors RegisterName calls whose name differs from what Register computes; gob keeps
// its registry private.
var renamed = map[string]string{"[]*main.Val": "[]*main.LegacyName"}

// registeredName is type.go, Register.
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
		if v.Type().Implements(gobEncoderT) || v.Type().Implements(binaryT) {
			break
		}
		v = v.Elem()
	}
	try := func(t reflect.Type, key string, call func(any) ([]byte, error)) (any, bool) {
		var target reflect.Value
		switch {
		case v.Type().Implements(t):
			target = v
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
	// No TextMarshaler: gob's support for it is commented out (type.go:85-89), so a type that
	// only marshals text is sent as its underlying kind.
	return try(binaryT, "$bin", func(x any) ([]byte, error) { return x.(encoding.BinaryMarshaler).MarshalBinary() })
}

// render returns the value tree and whether a struct field holding it is transmitted.
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

func renderTop(x any) any {
	val, _ := render(reflect.ValueOf(x))
	return val
}

// ─── the corpus ────────────────────────────────────────────────────────────────────────────

func ptr[T any](v T) *T { return &v }

func mustURL(s string) url.URL {
	u, err := url.Parse(s)
	if err != nil {
		panic(err)
	}
	return *u
}

// decodeAs names every Go type a stream can be decoded into.
var decodeAs = map[string]func() any{
	"Scalars":        func() any { return new(Scalars) },
	"Inner":          func() any { return new(Inner) },
	"*Inner":         func() any { return new(*Inner) },
	"Zeros":          func() any { return new(Zeros) },
	"Nested":         func() any { return new(Nested) },
	"Ifaces":         func() any { return new(Ifaces) },
	"Marshalers":     func() any { return new(Marshalers) },
	"Recursive":      func() any { return new(Recursive) },
	"Wide":           func() any { return new(Wide) },
	"WideNoNils":     func() any { return new(WideNoNils) },
	"Narrow":         func() any { return new(Narrow) },
	"Small":          func() any { return new(Small) },
	"Big":            func() any { return new(Big) },
	"ArrSmall":       func() any { return new(ArrSmall) },
	"U16s":           func() any { return new(U16s) },
	"U64s":           func() any { return new(U64s) },
	"[]Unrelated":    func() any { return new([]Unrelated) },
	"ArrBig":         func() any { return new(ArrBig) },
	"Unrelated":      func() any { return new(Unrelated) },
	"Ints":           func() any { return new(Ints) },
	"Uints":          func() any { return new(Uints) },
	"Strs":           func() any { return new(Strs) },
	"Floats":         func() any { return new(Floats) },
	"IntSlice":       func() any { return new(IntSlice) },
	"IntArr":         func() any { return new(IntArr) },
	"StrMap":         func() any { return new(StrMap) },
	"IntMap":         func() any { return new(IntMap) },
	"IfaceField":     func() any { return new(IfaceField) },
	"StructField":    func() any { return new(StructField) },
	"Post":           func() any { return new(Post) },
	"Merge":          func() any { return new(Merge) },
	"string":         func() any { return new(string) },
	"int":            func() any { return new(int) },
	"[]string":       func() any { return new([]string) },
	"map[string]int": func() any { return new(map[string]int) },
	"any":            func() any { return new(any) },
	"time.Time":      func() any { return new(time.Time) },
}

type streamCase struct {
	Name     string
	Values   []any  // encoded in order onto one stream
	DecodeAs string // decoded into this type, once per value
	Base     []any  // merge cases: first decode this stream into the destination
}

func corpus() []streamCase {
	ist := time.FixedZone("IST", 5*3600+1800)
	lmt := time.FixedZone("LMT", 3601)
	neg := time.FixedZone("NEG", -(3600 + 1))
	when := time.Date(2026, 9, 17, 11, 42, 7, 123456789, ist)
	scalars := Scalars{
		B: true, I: -42, I8: math.MinInt8, I16: math.MaxInt16, I32: math.MinInt32, I64: math.MinInt64,
		U: 42, U8: math.MaxUint8, U16: 65000, U32: math.MaxUint32, U64: math.MaxUint64, Uptr: 0xdeadbeef,
		F32: math.MaxFloat32, F64: -17.25, C128: complex(1.5, -2), S: "héllo ✓\x00end", Bytes: []byte{0, 1, 254, 255},
		private: 7,
	}
	inner := func(n int) Inner { return Inner{N: n, S: fmt.Sprintf("inner-%d", n)} }
	nested := Nested{
		Ptr: ptr(inner(1)), Val: inner(2), Slice: []Inner{inner(3), {}, inner(4)},
		PtrSlice: []*Inner{ptr(inner(5)), ptr(Inner{})}, Arr: [3]Inner{inner(6), {}, inner(7)},
		Map: map[string]Inner{"a": inner(8), "": {}}, IntMap: map[int64]string{-1: "minus", 1 << 40: "big", 0: ""},
		PtrMap:  map[string]*Inner{"p": ptr(inner(9))},
		Matrix:  [][]int{{1, 2}, {}, {-3}},
		Blobs:   [][]byte{{1}, {}, {2, 3}},
		Strings: []string{"x", "", "z"},
		Floats:  []float64{0, math.Inf(-1), 1e-310},
		Bools:   [2]bool{false, true},
	}
	ifaces := Ifaces{
		S: "str", F: 2.5, I: 7,
		M:    map[string]any{"k": "v", "n": 1.5, "deep": map[string]any{"list": []any{"a", nil, true}}},
		L:    []any{nil, "x", 3.0, []any{}},
		P:    &Inner{N: 11, S: "ptr"},
		V:    Val{Label: "val", Count: 3},
		Err:  &AppError{Id: "app.err", Message: "boom", DetailedError: "detail", StatusCode: 418, Where: "Where"},
		Nest: []any{map[string]any{"legacy": []*Val{{Label: "legacy", Count: 1}}}, &Inner{N: 12}},
		T:    when,
	}
	marshalers := Marshalers{
		T: time.Date(2026, 1, 2, 3, 4, 5, 6, time.UTC), TZ: when, TL: time.Date(1900, 1, 1, 0, 0, 0, 0, lmt),
		TNeg:      time.Date(2000, 6, 1, 12, 0, 0, 999, neg),
		U:         mustURL("https://user:pass@example.test:8065/p/a%20th?q=1&r=2#frag"),
		P:         ptr(mustURL("/relative?x=y")),
		Txt:       Celsius(-40.5),
		Big:       new(big.Int).Lsh(big.NewInt(-3), 100),
		ZeroFixed: time.Time{}.In(time.FixedZone("", 0)),
		After:     "after",
	}
	tree := &Recursive{Name: "root", Kids: []*Recursive{{Name: "k1", Next: &Recursive{Name: "k1-next"}}, {Name: "k2", Kids: []*Recursive{{Name: "k2a"}}}}}
	remote := "remote-1"
	base := Merge{
		Keep: "keep", Change: "old", Ptr: ptr(Inner{N: 1, S: "ptr-old"}),
		Slice: []Inner{{N: 1, S: "s1"}, {N: 2, S: "s2"}}, Short: []Inner{{N: 9, S: "short"}},
		Arr: [2]Inner{{N: 1, S: "a1"}, {N: 2, S: "a2"}}, Map: map[string]Inner{"x": {N: 1, S: "mx"}, "y": {N: 2, S: "my"}},
		Iface: &Inner{N: 1, S: "iface-old"}, Bytes: []byte("old-bytes"), Nested: Inner{N: 5, S: "nested-old"},
	}
	delta := Merge{
		Change: "new", Ptr: &Inner{N: 7}, Slice: []Inner{{S: "s1-new"}, {N: 20}},
		Short: []Inner{{S: "grow1"}, {N: 2}}, Arr: [2]Inner{{S: "a1-new"}, {}},
		Map: map[string]Inner{"x": {S: "mx-new"}}, Iface: Val{Label: "iface-new"}, Bytes: []byte("n"),
		Nested: Inner{S: "nested-new"},
	}
	postBase := Post{
		Id: "post1", CreateAt: 1758000000123, Message: "original", Props: map[string]any{"a": "1", "b": 2.0},
		FileIds: []string{"f1", "f2"}, RemoteId: &remote, Pinned: true, Metadata: &Meta{Emojis: []Inner{{N: 1, S: "smile"}}, Count: 3},
		ReplyTo: []Inner{{N: 1, S: "r1"}},
	}
	postDelta := Post{Message: "edited by plugin", Props: map[string]any{"b": "changed"}, FileIds: []string{"f9"}, Metadata: &Meta{Count: 4}}

	return []streamCase{
		{Name: "scalars", Values: []any{scalars}, DecodeAs: "Scalars"},
		{Name: "zeros", Values: []any{Zeros{PZ: ptr(0), PS: &Inner{}, ME: map[string]int{}, SL: []int{}, Last: "last"}}, DecodeAs: "Zeros"},
		{Name: "nested", Values: []any{nested}, DecodeAs: "Nested"},
		{Name: "ifaces", Values: []any{ifaces}, DecodeAs: "Ifaces"},
		// A url.URL field marshals through a pointer receiver, which needs an addressable value.
		{Name: "marshalers", Values: []any{&marshalers}, DecodeAs: "Marshalers"},
		{Name: "recursive", Values: []any{tree}, DecodeAs: "Recursive"},
		// One stream, many values: types are defined once and reused, including types first met
		// inside an interface; singletons of every framing.
		{Name: "stream_inner", Values: []any{inner(1), inner(2), &Inner{N: 3}}, DecodeAs: "Inner"},
		{Name: "stream_ifaces", Values: []any{ifaces, ifaces, Ifaces{S: "again"}}, DecodeAs: "Ifaces"},
		{Name: "singleton_string", Values: []any{"just a string", ""}, DecodeAs: "string"},
		{Name: "singleton_int", Values: []any{-7, 0, math.MaxInt64}, DecodeAs: "int"},
		{Name: "singleton_strings", Values: []any{[]string{"a", ""}, []string{}}, DecodeAs: "[]string"},
		{Name: "singleton_map", Values: []any{map[string]int{"one": 1}, map[string]int{}}, DecodeAs: "map[string]int"},
		{Name: "singleton_time", Values: []any{when, time.Time{}}, DecodeAs: "time.Time"},
		{Name: "top_pointer", Values: []any{&Inner{N: 4, S: "p"}}, DecodeAs: "*Inner"},
		// Skipping: the receiver lacks most fields.
		{Name: "skip_wide", Values: []any{WideNoNils{Keep: "kept", Nums: []int64{1, -2}, Blob: []byte("blob"), Tree: tree,
			Map: map[string][]Inner{"m": {inner(1)}}, Iface: "iface", Time: when,
			Arr: [2]Inner{inner(2), {}}, C: complex(0, 1), Last: 99}}, DecodeAs: "Narrow"},
		// Go bug: skipping an interface that defines a type inline fails ("field numbers out of
		// bounds"). The same value decodes when the receiver has the field.
		{Name: "skip_iface_inline_type", Values: []any{WideNoNils{Keep: "kept", Iface: map[string]any{"k": []any{"v"}}, Last: 99}}, DecodeAs: "Narrow"},
		{Name: "skip_iface_inline_type_decoded", Values: []any{WideNoNils{Keep: "kept", Iface: map[string]any{"k": []any{"v"}}, Last: 99}}, DecodeAs: "WideNoNils"},
		// Go bug: ignoreInterface reads a type sequence even for a nil interface.
		{Name: "skip_nil_iface", Values: []any{Wide{Keep: "kept", NilsIface: []any{nil, "x"}, Last: 99}}, DecodeAs: "Narrow"},
		{Name: "skip_nil_iface_decoded", Values: []any{Wide{Keep: "kept", NilsIface: []any{nil, "x"}, Last: 99}}, DecodeAs: "Wide"},
		// Range: every value fits, then each field overflows on its own.
		{Name: "fits_small", Values: []any{Big{I8: -128, U8: 255, F32: math.MaxFloat32}}, DecodeAs: "Small"},
		{Name: "overflow_int8", Values: []any{Big{I8: 128}}, DecodeAs: "Small"},
		{Name: "overflow_int8_negative", Values: []any{Big{I8: -129}}, DecodeAs: "Small"},
		{Name: "overflow_uint8", Values: []any{Big{U8: 256}}, DecodeAs: "Small"},
		{Name: "overflow_uint16", Values: []any{U64s{N: 65536}}, DecodeAs: "U16s"},
		{Name: "fits_uint16", Values: []any{U64s{N: 65535}}, DecodeAs: "U16s"},
		{Name: "overflow_float32", Values: []any{Big{F32: math.MaxFloat64}}, DecodeAs: "Small"},
		{Name: "float32_inf_ok", Values: []any{Big{F32: math.Inf(-1), I8: 1}}, DecodeAs: "Small"},
		{Name: "float32_underflow_ok", Values: []any{Big{F32: 5e-324}}, DecodeAs: "Small"},
		{Name: "array_len_mismatch", Values: []any{ArrBig{A: [3]int{1, 2, 3}}}, DecodeAs: "ArrSmall"},
		{Name: "no_fields_matched", Values: []any{inner(1)}, DecodeAs: "Unrelated"},
		// …but only for the value decoded itself: a slice of such structs decodes to zero values.
		{Name: "slice_of_unrelated", Values: []any{[]Inner{inner(1), inner(2)}}, DecodeAs: "[]Unrelated"},
		{Name: "int_into_uint", Values: []any{Ints{N: 1}}, DecodeAs: "Uints"},
		{Name: "uint_into_int", Values: []any{Uints{N: 1}}, DecodeAs: "Ints"},
		{Name: "int_into_string", Values: []any{Ints{N: 1}}, DecodeAs: "Strs"},
		{Name: "int_into_float", Values: []any{Ints{N: 1}}, DecodeAs: "Floats"},
		{Name: "slice_into_array", Values: []any{IntSlice{N: []int{1}}}, DecodeAs: "IntArr"},
		{Name: "map_key_mismatch", Values: []any{StrMap{N: map[string]int{"a": 1}}}, DecodeAs: "IntMap"},
		// A mismatch in a field the value does not carry still fails: Go checks at compile time.
		{Name: "absent_field_mismatch", Values: []any{Ints{}}, DecodeAs: "Strs"},
		{Name: "concrete_into_iface", Values: []any{Ints{N: 1}}, DecodeAs: "IfaceField"},
		{Name: "struct_into_scalar", Values: []any{StructField{N: inner(1)}}, DecodeAs: "Ints"},
		{Name: "top_string_into_int", Values: []any{"s"}, DecodeAs: "int"},
		{Name: "top_struct_into_string", Values: []any{inner(1)}, DecodeAs: "string"},
		{Name: "top_concrete_into_any", Values: []any{"s"}, DecodeAs: "any"},
		// Decode into an existing value.
		{Name: "merge", Base: []any{base}, Values: []any{delta}, DecodeAs: "Merge"},
		{Name: "merge_post", Base: []any{postBase}, Values: []any{postDelta}, DecodeAs: "Post"},
	}
}

// ─── gen / echo ────────────────────────────────────────────────────────────────────────────

type caseOut struct {
	Name     string `json:"name"`
	Stream   string `json:"stream"`
	Base     string `json:"base,omitempty"`
	DecodeAs string `json:"decode_as"`
	// Wire: each value as sent, rendered from the Go value.
	Wire []any `json:"wire"`
	// Decoded: what Go's decoder produced for each value, or the error that stopped it.
	Decoded []any  `json:"decoded"`
	Error   string `json:"error,omitempty"`
}

func encodeAll(values []any) ([]byte, error) {
	var buf bytes.Buffer
	enc := gob.NewEncoder(&buf)
	for _, v := range values {
		if err := enc.Encode(v); err != nil {
			return nil, err
		}
	}
	return buf.Bytes(), nil
}

// decodeStream decodes every value of stream into (a fresh or the merged) decodeAs destination.
func decodeStream(stream []byte, base []byte, as string, count int) ([]any, error) {
	mk, ok := decodeAs[as]
	if !ok {
		return nil, fmt.Errorf("unknown decode type %q", as)
	}
	var dest any
	if base != nil {
		dest = mk()
		if err := gob.NewDecoder(bytes.NewReader(base)).Decode(dest); err != nil {
			return nil, fmt.Errorf("base: %w", err)
		}
	}
	dec := gob.NewDecoder(bytes.NewReader(stream))
	var out []any
	for i := 0; count < 0 || i < count; i++ {
		if base == nil {
			dest = mk()
		}
		err := dec.Decode(dest)
		if errors.Is(err, io.EOF) && count < 0 {
			break
		}
		if err != nil {
			return out, err
		}
		out = append(out, renderTop(reflect.ValueOf(dest).Elem().Interface()))
	}
	return out, nil
}

// renderTop of an `any` holding a pointer destination's element.
func init() {
	_ = strings.TrimSpace
}

func gen(dir string) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	var outs []caseOut
	for _, c := range corpus() {
		stream, err := encodeAll(c.Values)
		if err != nil {
			return fmt.Errorf("%s: encode: %w", c.Name, err)
		}
		o := caseOut{Name: c.Name, Stream: c.Name + ".gob", DecodeAs: c.DecodeAs}
		if err := writeStable(filepath.Join(dir, o.Stream), stream, c.Values); err != nil {
			return err
		}
		for _, v := range c.Values {
			o.Wire = append(o.Wire, renderTop(v))
		}
		var baseBytes []byte
		if c.Base != nil {
			baseBytes, err = encodeAll(c.Base)
			if err != nil {
				return fmt.Errorf("%s: encode base: %w", c.Name, err)
			}
			o.Base = c.Name + ".base.gob"
			if err := writeStable(filepath.Join(dir, o.Base), baseBytes, c.Base); err != nil {
				return err
			}
			if baseBytes, err = os.ReadFile(filepath.Join(dir, o.Base)); err != nil {
				return err
			}
		}
		if stream, err = os.ReadFile(filepath.Join(dir, o.Stream)); err != nil {
			return err
		}
		decoded, err := decodeStream(stream, baseBytes, c.DecodeAs, len(c.Values))
		o.Decoded = decoded
		if o.Decoded == nil {
			o.Decoded = []any{}
		}
		if err != nil {
			o.Error = err.Error()
		}
		outs = append(outs, o)
	}
	b, err := json.MarshalIndent(outs, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(dir, "cases.json"), append(b, '\n'), 0o644)
}

// writeStable writes a stream unless the file already holds an encoding of the same values.
//
// Go encodes a map in its randomised iteration order, so a stream with a multi-key map comes out
// different on every run. Keeping the existing file when it decodes to the same rendering keeps
// the generator deterministic in git terms without restricting the corpus to one-key maps.
func writeStable(path string, stream []byte, values []any) error {
	if old, err := os.ReadFile(path); err == nil && !bytes.Equal(old, stream) {
		want, _ := json.Marshal(renderAll(values))
		if got, err := decodeRendered(old, values); err == nil && bytes.Equal(got, want) {
			return nil
		}
	}
	return os.WriteFile(path, stream, 0o644)
}

func renderAll(values []any) []any {
	out := []any{}
	for _, v := range values {
		out = append(out, renderTop(v))
	}
	return out
}

// decodeRendered decodes an existing stream into fresh values of the same Go types.
func decodeRendered(stream []byte, like []any) ([]byte, error) {
	dec := gob.NewDecoder(bytes.NewReader(stream))
	out := []any{}
	for _, v := range like {
		dest := reflect.New(reflect.TypeOf(v))
		if err := dec.Decode(dest.Interface()); err != nil {
			return nil, err
		}
		out = append(out, renderTop(dest.Elem().Interface()))
	}
	return json.Marshal(out)
}

// echo decodes a Rust-written stream from stdin and prints {"decoded": [...], "error": "..."}.
func echo(as string, basePath string) error {
	stream, err := io.ReadAll(os.Stdin)
	if err != nil {
		return err
	}
	var base []byte
	if basePath != "" {
		if base, err = os.ReadFile(basePath); err != nil {
			return err
		}
	}
	decoded, derr := decodeStream(stream, base, as, -1)
	out := map[string]any{"decoded": decoded}
	if decoded == nil {
		out["decoded"] = []any{}
	}
	if derr != nil {
		out["error"] = derr.Error()
	}
	return json.NewEncoder(os.Stdout).Encode(out)
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: gob gen <dir> | gob echo <decodeAs> [base.gob] < stream")
		os.Exit(2)
	}
	var err error
	switch os.Args[1] {
	case "gen":
		err = gen(os.Args[2])
	case "echo":
		base := ""
		if len(os.Args) > 3 {
			base = os.Args[3]
		}
		err = echo(os.Args[2], base)
	default:
		err = fmt.Errorf("unknown mode %q", os.Args[1])
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "gob:", err)
		os.Exit(1)
	}
}
