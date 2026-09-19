package main

// Behavioural oracle for Mattermost's image pipeline: `channels/app/imaging` and everything under
// it that decides a stored byte — `github.com/boxes-ltd/imaging` (Lanczos resampling, the EXIF
// transforms, Fit/Fill), `github.com/bep/imagemeta` (the EXIF orientation walk), and the standard
// library's `image/png`, `image/jpeg`, `compress/flate`, `compress/zlib` and `image/color`.
//
// Why an oracle and not a reading: the upload routes store the *output bytes* of this pipeline
// (a `_thumb`, a `_preview`, a 16×16 `mini_preview` inside the FileInfo row, a 128×128 profile
// PNG), so "close" is a different file. Every one of those bytes is deterministic in Go, and every
// one is at the mercy of a detail a reader gets wrong: which filter the PNG encoder picks per row,
// which lazy match the deflater keeps, which float operations the arm64 compiler fuses into an FMA
// inside the Lanczos kernel. The corpus below records Go's answer for each stage separately — so a
// mismatch names the stage — and then end to end.
//
// The fixtures are split per stage (behaviour_imaging_<stage>.json) because the Rust tests are per
// module and a single multi-megabyte document would be parsed by every one of them.
//
// # Inputs are generated, and the generator is part of the contract
//
// Encoder and resampler inputs are *specs* (type, size, pattern, alpha mode, seed), not pixels. The
// Rust tests rebuild each image from its spec with a mirror of the generator below and check the
// mirror first against `pix_sha256`, so a generator drift is reported as that, not as an encoder
// bug. Decoder inputs are *files* and travel as base64: GOROOT's own image testdata (pngsuite and
// the video-001 JPEG family, which between them cover every PNG colour type and bit depth, every
// JPEG subsampling ratio, progressive scans, restart markers, CMYK and Adobe RGB) plus files this
// program encodes and then corrupts.
//
// Everything here is deterministic: no clock, no map iteration order, no randomness beyond the
// seeded hash.

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"image"
	"image/color"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"sort"
)

// imagingFixtureNote is stamped into every stage's fixture so a reader of the JSON knows the
// arithmetic it records is arm64's. Go fuses `a + x*y` into one FMADD on arm64 (and ppc64, s390x,
// riscv64, loong64) but not on amd64 at the default GOAMD64=v1, so the Lanczos output recorded
// here is a property of the architecture the Go server runs on.
func imagingFixtureNote() map[string]any {
	return map[string]any{
		"goarch":    runtime.GOARCH,
		"goversion": runtime.Version(),
	}
}

func writeImagingBehaviourFixtures(outDir string) error {
	stages := []struct {
		name string
		fn   func() (map[string]any, error)
	}{
		{"flate", imagingFlateStage},
		{"png", imagingPNGStage},
		{"jpeg", imagingJPEGStage},
		{"resize", imagingResizeStage},
		{"exif", imagingEXIFStage},
		{"pipeline", imagingPipelineStage},
	}
	for _, s := range stages {
		out, err := s.fn()
		if err != nil {
			return fmt.Errorf("%s: %w", s.name, err)
		}
		out["arch"] = imagingFixtureNote()
		blob, err := marshalOneCasePerLine(out)
		if err != nil {
			return fmt.Errorf("%s: %w", s.name, err)
		}
		path := filepath.Join(outDir, "behaviour_imaging_"+s.name+".json")
		if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
			return err
		}
		fmt.Printf("wrote %s\n", path)
	}
	return nil
}

// marshalOneCasePerLine writes a stage as a JSON object whose array members hold one compact case
// per line: a tenth of the indented size, and still a readable line diff when a case moves.
func marshalOneCasePerLine(out map[string]any) ([]byte, error) {
	keys := make([]string, 0, len(out))
	for k := range out {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var b bytes.Buffer
	b.WriteString("{\n")
	for i, k := range keys {
		kb, _ := json.Marshal(k)
		b.Write(kb)
		b.WriteString(": ")
		v := reflect.ValueOf(out[k])
		if v.Kind() == reflect.Slice {
			b.WriteString("[\n")
			for j := 0; j < v.Len(); j++ {
				eb, err := json.Marshal(v.Index(j).Interface())
				if err != nil {
					return nil, err
				}
				b.Write(eb)
				if j < v.Len()-1 {
					b.WriteByte(',')
				}
				b.WriteByte('\n')
			}
			b.WriteString("]")
		} else {
			vb, err := json.Marshal(out[k])
			if err != nil {
				return nil, err
			}
			b.Write(vb)
		}
		if i < len(keys)-1 {
			b.WriteByte(',')
		}
		b.WriteByte('\n')
	}
	b.WriteString("}")
	return b.Bytes(), nil
}

// --- the deterministic generator ----------------------------------------------------------------
//
// Mirrored in crates/goimage/tests/common/gen.rs. Wrapping uint64 arithmetic throughout.

// mix64 is splitmix64's finaliser.
func mix64(z uint64) uint64 {
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

// hash4 is a stateless hash of a seed and three coordinates.
func hash4(seed uint64, a, b, c int) uint64 {
	return mix64(seed + uint64(a)*0x9e3779b97f4a7c15 + uint64(b)*0xc2b2ae3d27d4eb4f + uint64(c)*0x165667b19e3779f9)
}

// sample is one byte of pattern at (x, y) for channel-byte k.
func sample(pattern string, seed uint64, x, y, k, w, h int) uint8 {
	switch pattern {
	case "noise":
		return uint8(hash4(seed, x, y, k) >> 56)
	case "gradient":
		var v int
		if k%2 == 0 {
			v = x * 255 / max(w-1, 1)
		} else {
			v = y * 255 / max(h-1, 1)
		}
		return uint8((v + k*37) & 0xff)
	case "blocks":
		base := int(hash4(seed, x/16, y/16, k) >> 56)
		grain := int(hash4(seed+1, x, y, k) >> 60)
		return uint8((base + grain) & 0xff)
	case "smooth":
		v := (x*x+2*y*y)/(w+h+1) + 3*x + 50*k + int(hash4(seed, x, y, k)>>61)
		return uint8(v & 0xff)
	case "flat":
		return uint8(hash4(seed, 0, 0, k) >> 56)
	}
	panic("unknown pattern " + pattern)
}

// alpha8 turns a pattern byte into an alpha byte under a mode.
func alpha8(mode string, seed uint64, x, y int, s uint8) uint8 {
	switch mode {
	case "opaque":
		return 0xff
	case "binary":
		if hash4(seed+2, x, y, 0)>>63 == 0 {
			return 0
		}
		return 0xff
	case "mixed":
		r := hash4(seed+3, x, y, 0) >> 56
		switch {
		case r < 64:
			return 0
		case r < 128:
			return 0xff
		}
		return s
	case "soft":
		return s
	}
	panic("unknown alpha mode " + mode)
}

// alpha16 is alpha8 at sixteen bits: the same modes, with the fully opaque value 0xffff.
func alpha16(mode string, seed uint64, x, y int, s uint32) uint32 {
	switch mode {
	case "opaque":
		return 0xffff
	case "binary":
		if hash4(seed+2, x, y, 0)>>63 == 0 {
			return 0
		}
		return 0xffff
	case "mixed":
		r := hash4(seed+3, x, y, 0) >> 56
		switch {
		case r < 64:
			return 0
		case r < 128:
			return 0xffff
		}
		return s
	case "soft":
		return s
	}
	panic("unknown alpha mode " + mode)
}

// imgSpec describes a generated image. Every field is on the wire so the Rust mirror can rebuild it.
type imgSpec struct {
	Kind    string `json:"kind"` // gray gray16 rgba rgba64 nrgba nrgba64 paletted ycbcr cmyk
	W       int    `json:"w"`
	H       int    `json:"h"`
	Pattern string `json:"pattern"`
	Alpha   string `json:"alpha"`
	Seed    uint64 `json:"seed"`
	// Paletted only: the palette length.
	Palette int `json:"palette,omitempty"`
	// YCbCr only: "444" "422" "420" "440" "411" "410".
	Ratio string `json:"ratio,omitempty"`
}

var ratios = map[string]image.YCbCrSubsampleRatio{
	"444": image.YCbCrSubsampleRatio444,
	"422": image.YCbCrSubsampleRatio422,
	"420": image.YCbCrSubsampleRatio420,
	"440": image.YCbCrSubsampleRatio440,
	"411": image.YCbCrSubsampleRatio411,
	"410": image.YCbCrSubsampleRatio410,
}

func (s imgSpec) build() image.Image {
	w, h := s.W, s.H
	r := image.Rect(0, 0, w, h)
	smp := func(x, y, k int) uint8 { return sample(s.Pattern, s.Seed, x, y, k, w, h) }
	switch s.Kind {
	case "gray":
		m := image.NewGray(r)
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				m.Pix[y*m.Stride+x] = smp(x, y, 0)
			}
		}
		return m
	case "gray16":
		m := image.NewGray16(r)
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				i := y*m.Stride + 2*x
				m.Pix[i] = smp(x, y, 0)
				m.Pix[i+1] = smp(x, y, 1)
			}
		}
		return m
	case "nrgba", "rgba":
		pix := make([]uint8, 4*w*h)
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				i := 4 * (y*w + x)
				a := alpha8(s.Alpha, s.Seed, x, y, smp(x, y, 3))
				for k := 0; k < 3; k++ {
					c := smp(x, y, k)
					if s.Kind == "rgba" {
						c = uint8(int(c) * int(a) / 255)
					}
					pix[i+k] = c
				}
				pix[i+3] = a
			}
		}
		if s.Kind == "rgba" {
			return &image.RGBA{Pix: pix, Stride: 4 * w, Rect: r}
		}
		return &image.NRGBA{Pix: pix, Stride: 4 * w, Rect: r}
	case "nrgba64", "rgba64":
		pix := make([]uint8, 8*w*h)
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				i := 8 * (y*w + x)
				a := alpha16(s.Alpha, s.Seed, x, y, uint32(smp(x, y, 6))<<8|uint32(smp(x, y, 7)))
				for k := 0; k < 3; k++ {
					c := uint32(smp(x, y, 2*k))<<8 | uint32(smp(x, y, 2*k+1))
					if s.Kind == "rgba64" {
						c = c * a / 0xffff
					}
					pix[i+2*k] = uint8(c >> 8)
					pix[i+2*k+1] = uint8(c)
				}
				pix[i+6] = uint8(a >> 8)
				pix[i+7] = uint8(a)
			}
		}
		if s.Kind == "rgba64" {
			return &image.RGBA64{Pix: pix, Stride: 8 * w, Rect: r}
		}
		return &image.NRGBA64{Pix: pix, Stride: 8 * w, Rect: r}
	case "paletted":
		pal := make(color.Palette, s.Palette)
		for i := range pal {
			cr := uint8(hash4(s.Seed+7, i, 0, 0) >> 56)
			cg := uint8(hash4(s.Seed+7, i, 0, 1) >> 56)
			cb := uint8(hash4(s.Seed+7, i, 0, 2) >> 56)
			a := alpha8(s.Alpha, s.Seed+7, i, 0, uint8(hash4(s.Seed+7, i, 0, 3)>>56))
			if a == 0xff {
				pal[i] = color.RGBA{cr, cg, cb, 0xff}
			} else {
				pal[i] = color.NRGBA{cr, cg, cb, a}
			}
		}
		m := image.NewPaletted(r, pal)
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				m.Pix[y*m.Stride+x] = uint8(int(smp(x, y, 0)) % s.Palette)
			}
		}
		return m
	case "ycbcr":
		m := image.NewYCbCr(r, ratios[s.Ratio])
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				m.Y[y*m.YStride+x] = smp(x, y, 0)
			}
		}
		cw, ch := m.CStride, len(m.Cb)/max(m.CStride, 1)
		for cy := 0; cy < ch; cy++ {
			for cx := 0; cx < cw; cx++ {
				m.Cb[cy*m.CStride+cx] = smp(cx, cy, 1)
				m.Cr[cy*m.CStride+cx] = smp(cx, cy, 2)
			}
		}
		return m
	case "cmyk":
		m := image.NewCMYK(r)
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				i := y*m.Stride + 4*x
				for k := 0; k < 4; k++ {
					m.Pix[i+k] = smp(x, y, k)
				}
			}
		}
		return m
	}
	panic("unknown kind " + s.Kind)
}

// --- describing an image ------------------------------------------------------------------------

func sha(b []byte) string {
	s := sha256.Sum256(b)
	return hex.EncodeToString(s[:])
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

// paletteEntry spells one palette colour with its Go type, since `color.RGBA{..,0xff}` and
// `color.NRGBA{..,0xff}` are equal in value and different in what `NRGBAModel.Convert` does with
// them when alpha is not 0xff.
func paletteEntry(c color.Color) []any {
	switch c := c.(type) {
	case color.RGBA:
		return []any{"rgba", c.R, c.G, c.B, c.A}
	case color.NRGBA:
		return []any{"nrgba", c.R, c.G, c.B, c.A}
	}
	r, g, b, a := c.RGBA()
	return []any{fmt.Sprintf("%T", c), r, g, b, a}
}

// describe records an image's Go type, geometry and pixel hash. The pixel hash covers exactly the
// backing slices, strides included, because a Rust image with the right colours and a different
// stride is still a different image to every downstream consumer that indexes by stride.
func describe(m image.Image) map[string]any {
	b := m.Bounds()
	d := map[string]any{"rect": []int{b.Min.X, b.Min.Y, b.Max.X, b.Max.Y}}
	switch m := m.(type) {
	case *image.Gray:
		d["type"], d["stride"], d["pix_sha256"] = "gray", m.Stride, sha(m.Pix)
	case *image.Gray16:
		d["type"], d["stride"], d["pix_sha256"] = "gray16", m.Stride, sha(m.Pix)
	case *image.RGBA:
		d["type"], d["stride"], d["pix_sha256"] = "rgba", m.Stride, sha(m.Pix)
	case *image.RGBA64:
		d["type"], d["stride"], d["pix_sha256"] = "rgba64", m.Stride, sha(m.Pix)
	case *image.NRGBA:
		d["type"], d["stride"], d["pix_sha256"] = "nrgba", m.Stride, sha(m.Pix)
	case *image.NRGBA64:
		d["type"], d["stride"], d["pix_sha256"] = "nrgba64", m.Stride, sha(m.Pix)
	case *image.CMYK:
		d["type"], d["stride"], d["pix_sha256"] = "cmyk", m.Stride, sha(m.Pix)
	case *image.Paletted:
		d["type"], d["stride"], d["pix_sha256"] = "paletted", m.Stride, sha(m.Pix)
		pal := make([][]any, len(m.Palette))
		for i, c := range m.Palette {
			pal[i] = paletteEntry(c)
		}
		d["palette"] = pal
	case *image.YCbCr:
		d["type"] = "ycbcr"
		d["ratio"] = m.SubsampleRatio.String()
		d["y_stride"], d["c_stride"] = m.YStride, m.CStride
		d["y_sha256"], d["cb_sha256"], d["cr_sha256"] = sha(m.Y), sha(m.Cb), sha(m.Cr)
		d["y_len"], d["c_len"] = len(m.Y), len(m.Cb)
	default:
		d["type"] = fmt.Sprintf("%T", m)
	}
	return d
}

// encoded records an encoder's output: always its hash and length, and the bytes themselves when
// they are small enough to be worth diffing by eye.
func encoded(b []byte) map[string]any {
	d := map[string]any{"len": len(b), "sha256": sha(b)}
	if len(b) <= 2048 {
		d["b64"] = b64(b)
	}
	return d
}

func imgErr(err error) any {
	if err == nil {
		return nil
	}
	return err.Error()
}

// fileCorpus is the named decoder input set, shared by the png, jpeg, exif and pipeline stages.
// Each stage records which names it used; the bytes travel once, in the stage that owns them.
type namedFile struct {
	Name string
	Data []byte
}

func mustEncode(fn func(*bytes.Buffer) error) []byte {
	var buf bytes.Buffer
	if err := fn(&buf); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

func decodeB64(s string) ([]byte, error) { return base64.StdEncoding.DecodeString(s) }
