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
	"image"

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
	return map[string]any{"cases": cases}, nil
}
