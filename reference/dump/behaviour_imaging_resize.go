package main

// The resampling stage of the imaging oracle: `boxes-ltd/imaging`'s Resize (Lanczos), Fit and
// Fill, and Mattermost's wrappers over them (GenerateThumbnail, GeneratePreview, FillCenter,
// MakeImageUpright). See behaviour_imaging.go for the conventions.
//
// The arithmetic is float64 throughout and runs on arm64, where Go fuses `a + x*y` into FMADD —
// including inside `math.Sin`, which Lanczos calls through `sinc`. The corpus is built to make a
// wrong fusion *visible*: every result is hashed over its exact bytes, the inputs include hard
// alpha edges (where `clamp(r * aInv)` sits on a .5 boundary most often) and flat fields (where a
// rounding difference in a weight shows up everywhere at once).

import (
	"encoding/binary"
	"fmt"
	"image"
	"math"

	bimaging "github.com/boxes-ltd/imaging"
	mmimaging "github.com/mattermost/mattermost/server/v8/channels/app/imaging"
)

type resizeOp struct {
	Op string `json:"op"` // resize fit fill thumbnail preview mini upright
	W  int    `json:"w,omitempty"`
	H  int    `json:"h,omitempty"`
	// upright only.
	Orientation int `json:"orientation,omitempty"`
}

type resizeCase struct {
	Spec  imgSpec        `json:"spec"`
	Input map[string]any `json:"input"`
	Op    resizeOp       `json:"op"`
	// "same" when Go returned the input image itself rather than a new one.
	Same   bool           `json:"same"`
	Output map[string]any `json:"output,omitempty"`
}

func applyResizeOp(m image.Image, op resizeOp) image.Image {
	switch op.Op {
	case "resize":
		return bimaging.Resize(m, op.W, op.H, bimaging.Lanczos)
	case "fit":
		return mmimaging.Fit(m, op.W, op.H)
	case "fill":
		return mmimaging.FillCenter(m, op.W, op.H)
	case "thumbnail":
		return mmimaging.GenerateThumbnail(m, 120, 100)
	case "preview":
		return mmimaging.GeneratePreview(m, 1920)
	case "mini":
		return bimaging.Resize(m, 16, 16, bimaging.Lanczos)
	case "upright":
		return mmimaging.MakeImageUpright(m, op.Orientation)
	}
	panic("unknown op " + op.Op)
}

func imagingResizeStage() (map[string]any, error) {
	var cases []resizeCase
	seed := uint64(3000)
	run := func(spec imgSpec, ops []resizeOp) {
		spec.Seed = seed
		seed++
		m := spec.build()
		in := describe(m)
		for _, op := range ops {
			out := applyResizeOp(m, op)
			c := resizeCase{Spec: spec, Input: in, Op: op}
			if out == m {
				c.Same = true
			} else {
				c.Output = describe(out)
			}
			cases = append(cases, c)
		}
	}

	// The Mattermost call sites, over every source type the decoders produce.
	mm := []resizeOp{{Op: "thumbnail"}, {Op: "preview"}, {Op: "mini"}, {Op: "fill", W: 128, H: 128}, {Op: "fit", W: 128, H: 128}}
	kinds := []imgSpec{
		{Kind: "nrgba", Pattern: "smooth", Alpha: "opaque"},
		{Kind: "nrgba", Pattern: "blocks", Alpha: "mixed"},
		{Kind: "nrgba", Pattern: "noise", Alpha: "binary"},
		{Kind: "rgba", Pattern: "smooth", Alpha: "mixed"},
		{Kind: "rgba", Pattern: "blocks", Alpha: "opaque"},
		{Kind: "nrgba64", Pattern: "noise", Alpha: "mixed"},
		{Kind: "rgba64", Pattern: "smooth", Alpha: "mixed"},
		{Kind: "gray", Pattern: "smooth", Alpha: "opaque"},
		{Kind: "gray16", Pattern: "gradient", Alpha: "opaque"},
		{Kind: "paletted", Pattern: "noise", Alpha: "mixed", Palette: 37},
		{Kind: "paletted", Pattern: "blocks", Alpha: "opaque", Palette: 256},
		{Kind: "ycbcr", Pattern: "smooth", Alpha: "opaque", Ratio: "444"},
		{Kind: "ycbcr", Pattern: "blocks", Alpha: "opaque", Ratio: "422"},
		{Kind: "ycbcr", Pattern: "noise", Alpha: "opaque", Ratio: "420"},
		{Kind: "ycbcr", Pattern: "smooth", Alpha: "opaque", Ratio: "440"},
		{Kind: "ycbcr", Pattern: "smooth", Alpha: "opaque", Ratio: "411"},
		{Kind: "ycbcr", Pattern: "smooth", Alpha: "opaque", Ratio: "410"},
		{Kind: "cmyk", Pattern: "blocks", Alpha: "opaque"},
	}
	sizes := [][2]int{
		{1, 1}, {1, 7}, {7, 1}, {2, 2}, {16, 16}, {17, 9}, {99, 120}, {100, 100}, {120, 100},
		{121, 100}, {120, 101}, {128, 128}, {129, 64}, {250, 90}, {90, 250}, {640, 480},
	}
	for _, k := range kinds {
		for _, sz := range sizes {
			spec := k
			spec.W, spec.H = sz[0], sz[1]
			run(spec, mm)
		}
	}
	// The preview boundary and a real photo size, on the types a photo decodes to.
	for _, k := range []imgSpec{kinds[0], kinds[13], kinds[1]} {
		for _, sz := range [][2]int{{1920, 20}, {1921, 20}, {2400, 1600}} {
			spec := k
			spec.W, spec.H = sz[0], sz[1]
			run(spec, mm)
		}
	}
	// Resize's own branches: aspect-preserving zero, identity, one axis only, upscaling, 1px.
	resizes := []resizeOp{
		{Op: "resize", W: 0, H: 0}, {Op: "resize", W: -1, H: 5}, {Op: "resize", W: 50, H: 0},
		{Op: "resize", W: 0, H: 50}, {Op: "resize", W: 1, H: 1}, {Op: "resize", W: 3, H: 200},
		{Op: "resize", W: 300, H: 7}, {Op: "resize", W: 45, H: 45}, {Op: "resize", W: 45, H: 33},
		{Op: "resize", W: 33, H: 45}, {Op: "fit", W: 0, H: 10}, {Op: "fit", W: 10, H: 10},
		{Op: "fit", W: 40, H: 5}, {Op: "fit", W: 5, H: 40}, {Op: "fill", W: 10, H: 3},
		{Op: "fill", W: 3, H: 10}, {Op: "fill", W: 64, H: 64}, {Op: "fill", W: 0, H: 1},
	}
	for _, k := range []imgSpec{kinds[0], kinds[1], kinds[3], kinds[9], kinds[14]} {
		for _, sz := range [][2]int{{45, 33}, {101, 150}, {150, 101}, {3, 3}} {
			spec := k
			spec.W, spec.H = sz[0], sz[1]
			run(spec, resizes)
		}
	}
	// Every EXIF orientation, over every source type.
	var uprights []resizeOp
	for o := 0; o <= 9; o++ {
		uprights = append(uprights, resizeOp{Op: "upright", Orientation: o})
	}
	for _, k := range kinds {
		for _, sz := range [][2]int{{1, 1}, {5, 3}, {4, 9}} {
			spec := k
			spec.W, spec.H = sz[0], sz[1]
			run(spec, uprights)
		}
	}
	// Branches the Mattermost call sites cannot reach: a non-integral crop in cropAndResize
	// (`int(math.Max(1, cropH) + 0.5)`) and Fit on an exact aspect tie.
	run(imgSpec{Kind: "nrgba", W: 200, H: 150, Pattern: "smooth", Alpha: "mixed"}, []resizeOp{{Op: "fill", W: 7, H: 5}})
	run(imgSpec{Kind: "nrgba", W: 150, H: 200, Pattern: "smooth", Alpha: "mixed"}, []resizeOp{{Op: "fill", W: 5, H: 7}})
	run(imgSpec{Kind: "nrgba", W: 300, H: 210, Pattern: "blocks", Alpha: "soft"}, []resizeOp{{Op: "fit", W: 130, H: 91}})
	return map[string]any{"cases": cases, "sin": imagingSinCases()}, nil
}

// sinArgs are the arguments whose math.Sin the port is checked against bit for bit: zero and
// signed zero, the Lanczos range at every step Lanczos takes for small scales, the reduction
// boundaries, huge values (trigReduce), infinities and NaN, and one argument
// (0x412a9b706fa52b67) whose result differs between the fused and the unfused reduction.
var sinArgs = []uint64{
	0x0, 0x0, 0x1a56e1fc2f8f359, 0x3fb999999999999a, 0x3fe0000000000000, 0x3fe921fb54442d18,
	0x3ff0000000000000, 0x3ff8000000000000, 0x4000000000000000, 0x4002d97c7f3321d2, 0x4008000000000000, 0x400921fb54442d18,
	0x400c000000000000, 0x4010000000000000, 0x4014000000000000, 0x4018000000000000, 0x401c000000000000, 0x4020000000000000,
	0x4022000000000000, 0x4022d97c7f3321d2, 0x4024000000000000, 0xbff4cccccccccccd, 0xc01ecccccccccccd, 0x408f400000000000,
	0x4197d78400000000, 0x41bfffffff800000, 0x41c0000000000000, 0x41cdcd6500000000, 0x430c6bf526340000, 0x7e37e43c8800759c,
	0x7ff0000000000000, 0x7ff8000000000001, 0x0, 0xc00921fb54442d18, 0x3fd8d28b94fe748c, 0xc007ef9a071c5bb5,
	0x3fe8d28b94fe748c, 0xc006bd38b9f48a53, 0x3ff29de8afbed769, 0xc0058ad76cccb8f0, 0x3ff8d28b94fe748c, 0xc00458761fa4e78e,
	0x3fff072e7a3e11af, 0xc0032614d27d162b, 0x40029de8afbed769, 0xc001f3b3855544c8, 0x4005b83a225ea5fb, 0xc000c152382d7365,
	0x4008d28b94fe748c, 0xbfff1de1d60b4405, 0x400becdd079e431e, 0xbffcb91f3bbba140, 0x400f072e7a3e11af, 0xbffa545ca16bfe7b,
	0x401110bff66ef020, 0xbff7ef9a071c5bb5, 0x40129de8afbed769, 0xbff58ad76cccb8f0, 0x40142b11690ebeb2, 0xbff32614d27d162b,
	0x4015b83a225ea5fb, 0xbff0c152382d7365, 0x40174562dbae8d43, 0xbfecb91f3bbba140, 0x4018d28b94fe748c, 0xbfe7ef9a071c5bb7,
	0x401a5fb44e4e5bd5, 0xbfe32614d27d162d, 0x401becdd079e431e, 0xbfdcb91f3bbba13c, 0x401d7a05c0ee2a66, 0xbfd32614d27d1628,
	0x401f072e7a3e11af, 0xbfc32614d27d1628, 0x40204a2b99c6fc7c, 0x0, 0x402110bff66ef020, 0x3fc32614d27d1628,
	0x4021d7545316e3c5, 0x3fd32614d27d1628, 0x40229de8afbed769, 0x3fdcb91f3bbba13c, 0x4023647d0c66cb0e, 0x3fe32614d27d162d,
	0x40242b11690ebeb2, 0x3fe7ef9a071c5bb7, 0x4024f1a5c5b6b256, 0x3fecb91f3bbba140, 0x4025b83a225ea5fb, 0x3ff0c152382d7365,
	0x40267ece7f06999f, 0x3ff32614d27d162d, 0x40274562dbae8d43, 0x3ff58ad76cccb8f0, 0x40280bf7385680e8, 0x3ff7ef9a071c5bb7,
	0x4028d28b94fe748c, 0x3ffa545ca16bfe79, 0x4029991ff1a66831, 0x3ffcb91f3bbba140, 0x402a5fb44e4e5bd5, 0x3fff1de1d60b4404,
	0x402b2648aaf64f79, 0x4000c152382d7365, 0x402becdd079e431e, 0x4001f3b3855544c9, 0x402cb371644636c2, 0x40032614d27d162b,
	0x402d7a05c0ee2a66, 0x400458761fa4e78e, 0x402e409a1d961e0b, 0x40058ad76cccb8f0, 0x402f072e7a3e11af, 0x4006bd38b9f48a53,
	0x402fcdc2d6e60553, 0x4007ef9a071c5bb4, 0x40304a2b99c6fc7c, 0x400921fb54442d18, 0x4030ad75c81af64e, 0x400a545ca16bfe7c,
	0x403110bff66ef020, 0x400b86bdee93cfdd, 0x4031740a24c2e9f3, 0x400cb91f3bbba140, 0x4031d7545316e3c5, 0x400deb8088e372a3,
	0x40323a9e816add97, 0x400f1de1d60b4405, 0x40329de8afbed769, 0x4010282191998ab3, 0x40330132de12d13b, 0x4010c152382d7365,
	0x4033647d0c66cb0e, 0x40115a82dec15c17, 0x4033c7c73abac4e0, 0x4011f3b3855544c8, 0x40342b11690ebeb2, 0x40128ce42be92d79,
	0x40348e5b9762b884, 0x40132614d27d162b, 0x4034f1a5c5b6b256, 0x4013bf457910fedc, 0x403554eff40aac29, 0x401458761fa4e78d,
	0x4035b83a225ea5fb, 0x4014f1a6c638d03f, 0x40361b8450b29fcd, 0x40158ad76cccb8f0, 0x40367ece7f06999f, 0x401624081360a1a3,
	0x4036e218ad5a9371, 0x4016bd38b9f48a53, 0x412a9b706fa52b67,
}

// imagingSinCases records math.Sin — which Lanczos calls through sinc — for sinArgs, and the
// SHA-256 of dense runs (each result's little-endian bits), which catch a single unfused step
// that sparse points cannot.
func imagingSinCases() map[string]any {
	points := make([][2]string, len(sinArgs))
	for i, a := range sinArgs {
		points[i] = [2]string{fmt.Sprintf("%016x", a), fmt.Sprintf("%016x", math.Float64bits(math.Sin(math.Float64frombits(a))))}
	}
	dense := func(n int, f func(i int) float64) string {
		b := make([]byte, 0, 8*n)
		for i := 0; i < n; i++ {
			b = binary.LittleEndian.AppendUint64(b, math.Float64bits(math.Sin(f(i))))
		}
		return sha(b)
	}
	return map[string]any{
		"points": points,
		"dense": map[string]string{
			// Each generator's arithmetic is part of the contract; the Rust mirror says which
			// of them Go fuses (lanczos: an FNMSUBD; large: an FMADDD).
			"linear":  dense(400000, func(i int) float64 { return float64(i) * 0.0000712345 }),
			"lanczos": dense(200000, func(i int) float64 { return math.Pi * (float64(i)/200000*6 - 3) }),
			"neg":     dense(100000, func(i int) float64 { return -float64(i) * 0.000311 }),
			"large":   dense(20000, func(i int) float64 { return 536870912.0 + float64(i)*977.123 }),
			"huge": dense(20000, func(i int) float64 {
				return (1.0 + float64(i)/20000) * math.Pow(2, float64(30+(i%900)))
			}),
		},
	}
}
