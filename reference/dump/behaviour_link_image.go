package main

// Behavioural oracle for the image half of a link preview, written to
// fixtures/behaviour_link_image.json and asserted by `mm_app::link_image::go_parity`.
//
// `parseImages` (channels/app/post_metadata.go:1165) has three outcomes that surface differently on
// the wire — an image embed, a bare link embed (TIFF), or no embed at all (an error) — so *which*
// malformed images each of the six registered decoders rejects is what this pins. Everything here
// is Go's own code: `image.DecodeConfig` with the same decoders the server registers (std png,
// jpeg, gif; x/image bmp, tiff, webp — imported below exactly as channels/app/imaging does),
// `imaging.GetImageOrientation`, `imgutils.CountGIFFrames`, and `http.DetectContentType`.
// `parseImages` itself is unexported, so `parseImagesCopy` is a verbatim copy of it
// (post_metadata.go:1165-1211) with only the logger call removed.
//
// Inputs are built deterministically: Go's own encoders for the well-formed images, and byte
// patches or hand-assembled headers for everything malformed. WebP has no encoder in x/image, so
// its inputs are minimal hand-built RIFF files (a VP8L header, a VP8 key-frame header, a VP8X
// header) — which is all DecodeConfig ever reads.

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"hash/crc32"
	"image"
	"image/color"
	"image/gif"
	"image/jpeg"
	"image/png"
	"io"
	"net/http"
	"os"
	"path/filepath"

	"golang.org/x/image/bmp"
	"golang.org/x/image/tiff"
	_ "golang.org/x/image/webp"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/app/imaging"
	"github.com/mattermost/mattermost/server/v8/channels/utils/imgutils"
)

// parseImagesCopy is post_metadata.go:1165-1211, verbatim but for the logger.
func parseImagesCopy(body io.Reader) (*model.PostImage, error) {
	buf := &bytes.Buffer{}
	t := io.TeeReader(body, buf)

	config, format, err := image.DecodeConfig(t)
	if err != nil {
		return nil, err
	}

	image := &model.PostImage{
		Width:  config.Width,
		Height: config.Height,
		Format: format,
	}

	if format == "jpeg" {
		if imageOrientation, err := imaging.GetImageOrientation(io.MultiReader(buf, body), format); err == nil &&
			(imageOrientation == imaging.RotatedCWMirrored ||
				imageOrientation == imaging.RotatedCCW ||
				imageOrientation == imaging.RotatedCCWMirrored ||
				imageOrientation == imaging.RotatedCW) {
			image.Width, image.Height = image.Height, image.Width
		}
	}

	if format == "gif" {
		frameCount, err := imgutils.CountGIFFrames(io.MultiReader(buf, body))
		if err != nil {
			return nil, err
		}

		image.FrameCount = frameCount
	}

	if format == "tiff" {
		image = nil
	}

	return image, nil
}

type linkImageCase struct {
	Name  string `json:"name"`
	Input []byte `json:"input"`
	// parseImages
	Kind       string `json:"kind"` // image | nil | error
	Width      int    `json:"width"`
	Height     int    `json:"height"`
	Format     string `json:"format"`
	FrameCount int    `json:"frame_count"`
	Error      string `json:"error"`
	// image.DecodeConfig
	ConfigFormat string `json:"config_format"`
	ConfigWidth  int    `json:"config_width"`
	ConfigHeight int    `json:"config_height"`
	ConfigError  string `json:"config_error"`
	// imaging.GetImageOrientation(…, "jpeg") — jpeg inputs only
	Orientation      int  `json:"orientation"`
	OrientationError bool `json:"orientation_error"`
	// imgutils.CountGIFFrames — gif inputs only
	Frames      int    `json:"frames"`
	FramesError string `json:"frames_error"`
}

type sniffCase struct {
	Name  string `json:"name"`
	Input []byte `json:"input"`
	Type  string `json:"type"`
}

func runLinkImageCase(name string, input []byte) linkImageCase {
	c := linkImageCase{Name: name, Input: input}
	img, err := parseImagesCopy(bytes.NewReader(input))
	switch {
	case err != nil:
		c.Kind = "error"
		c.Error = err.Error()
	case img == nil:
		c.Kind = "nil"
	default:
		c.Kind = "image"
		c.Width, c.Height, c.Format, c.FrameCount = img.Width, img.Height, img.Format, img.FrameCount
	}
	cfg, format, err := image.DecodeConfig(bytes.NewReader(input))
	c.ConfigFormat = format
	if err != nil {
		c.ConfigError = err.Error()
	} else {
		c.ConfigWidth, c.ConfigHeight = cfg.Width, cfg.Height
	}
	if format == "jpeg" {
		o, err := imaging.GetImageOrientation(io.MultiReader(bytes.NewReader(input)), "jpeg")
		c.Orientation, c.OrientationError = o, err != nil
	}
	if format == "gif" {
		n, err := imgutils.CountGIFFrames(io.MultiReader(bytes.NewReader(input)))
		c.Frames = n
		if err != nil {
			c.FramesError = err.Error()
		}
	}
	return c
}

// ---------------------------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------------------------

func liEncode(f func(io.Writer) error) []byte {
	var b bytes.Buffer
	if err := f(&b); err != nil {
		panic(err)
	}
	return b.Bytes()
}

func liRGBA(w, h int, opaque bool) *image.RGBA {
	m := image.NewRGBA(image.Rect(0, 0, w, h))
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			a := uint8(255)
			if !opaque && (x+y)%2 == 0 {
				a = 128
			}
			m.Set(x, y, color.RGBA{uint8(x * 40), uint8(y * 40), 90, a})
		}
	}
	return m
}

func liGray(w, h int) *image.Gray {
	m := image.NewGray(image.Rect(0, 0, w, h))
	for i := range m.Pix {
		m.Pix[i] = uint8(i * 13)
	}
	return m
}

func liPaletted(w, h int, transparent bool) *image.Paletted {
	pal := color.Palette{color.RGBA{255, 0, 0, 255}, color.RGBA{0, 255, 0, 255}, color.RGBA{0, 0, 255, 255}}
	if transparent {
		pal = append(pal, color.RGBA{0, 0, 0, 0})
	}
	m := image.NewPaletted(image.Rect(0, 0, w, h), pal)
	for i := range m.Pix {
		m.Pix[i] = uint8(i % len(pal))
	}
	return m
}

func liCopy(b []byte) []byte { return append([]byte(nil), b...) }

func liPngChunk(typ string, data []byte) []byte {
	out := make([]byte, 8, 12+len(data))
	binary.BigEndian.PutUint32(out, uint32(len(data)))
	copy(out[4:], typ)
	out = append(out, data...)
	crc := crc32.NewIEEE()
	crc.Write([]byte(typ))
	crc.Write(data)
	return binary.BigEndian.AppendUint32(out, crc.Sum32())
}

func liPngIHDR(w, h uint32, depth, ctype, interlace byte) []byte {
	d := make([]byte, 13)
	binary.BigEndian.PutUint32(d, w)
	binary.BigEndian.PutUint32(d[4:], h)
	d[8], d[9], d[12] = depth, ctype, interlace
	return liPngChunk("IHDR", d)
}

func liConcat(parts ...[]byte) []byte {
	var out []byte
	for _, p := range parts {
		out = append(out, p...)
	}
	return out
}

const liPngSig = "\x89PNG\r\n\x1a\n"

type liIfdEntry struct {
	tag, typ uint16
	count    uint32
	value    []byte // exactly 4 bytes, inline value or offset
}

func liShort(order binary.ByteOrder, v uint16) []byte {
	b := make([]byte, 4)
	order.PutUint16(b, v)
	return b
}

func liLong(order binary.ByteOrder, v uint32) []byte {
	b := make([]byte, 4)
	order.PutUint32(b, v)
	return b
}

func liIfdBytes(order binary.ByteOrder, entries []liIfdEntry, next uint32) []byte {
	b := make([]byte, 2)
	order.PutUint16(b, uint16(len(entries)))
	for _, e := range entries {
		var x [12]byte
		order.PutUint16(x[0:], e.tag)
		order.PutUint16(x[2:], e.typ)
		order.PutUint32(x[4:], e.count)
		copy(x[8:], e.value)
		b = append(b, x[:]...)
	}
	return append(b, liLong(order, next)...)
}

// liTiffHeader is the 8-byte TIFF header with IFD0 at offset 8.
func liTiffHeader(order binary.ByteOrder) []byte {
	if order == binary.LittleEndian {
		return []byte("II\x2a\x00\x08\x00\x00\x00")
	}
	return []byte("MM\x00\x2a\x00\x00\x00\x08")
}

func liApp1(payload []byte) []byte {
	b := []byte{0xff, 0xe1, 0, 0}
	binary.BigEndian.PutUint16(b[2:], uint16(len(payload)+2))
	return append(b, payload...)
}

func liExifPayload(tiffBytes []byte) []byte {
	return append([]byte("Exif\x00\x00"), tiffBytes...)
}

// liSpliceAfterSOI inserts segments right after a JPEG's SOI.
func liSpliceAfterSOI(jpg []byte, segments ...[]byte) []byte {
	return liConcat(jpg[:2], liConcat(segments...), jpg[2:])
}

func liOrientationTIFF(order binary.ByteOrder, v uint16) []byte {
	return liConcat(liTiffHeader(order), liIfdBytes(order, []liIfdEntry{{0x0112, 3, 1, liShort(order, v)}}, 0))
}

func liJpegSegment(marker byte, data []byte) []byte {
	b := []byte{0xff, marker, 0, 0}
	binary.BigEndian.PutUint16(b[2:], uint16(len(data)+2))
	return append(b, data...)
}

func liSof(precision byte, h, w uint16, comps [][3]byte) []byte {
	d := []byte{precision, byte(h >> 8), byte(h), byte(w >> 8), byte(w), byte(len(comps))}
	for _, c := range comps {
		d = append(d, c[0], c[1], c[2])
	}
	return d
}

var liYcc = [][3]byte{{1, 0x22, 0}, {2, 0x11, 1}, {3, 0x11, 1}}

func liHandJPEG(segments ...[]byte) []byte {
	return liConcat([]byte{0xff, 0xd8}, liConcat(segments...))
}

var liSosMarker = []byte{0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00}

func liWebpFile(chunks ...[]byte) []byte {
	body := liConcat([]byte("WEBP"), liConcat(chunks...))
	h := []byte("RIFF\x00\x00\x00\x00")
	binary.LittleEndian.PutUint32(h[4:], uint32(len(body)))
	return append(h, body...)
}

func liRiffChunk(id string, data []byte) []byte {
	h := []byte(id + "\x00\x00\x00\x00")
	binary.LittleEndian.PutUint32(h[4:], uint32(len(data)))
	out := append(h, data...)
	if len(data)%2 == 1 {
		out = append(out, 0)
	}
	return out
}

func liVp8lHeader(w, h uint32, version uint32) []byte {
	bits := uint64(0x2f) | uint64(w-1)<<8 | uint64(h-1)<<22 | uint64(0)<<36 | uint64(version)<<37
	b := make([]byte, 5)
	for i := range b {
		b[i] = byte(bits >> (8 * i))
	}
	return b
}

func liVp8Header(key bool, w, h uint16) []byte {
	tag := byte(0x10)
	if !key {
		tag |= 1
	}
	return []byte{tag, 0x02, 0x00, 0x9d, 0x01, 0x2a, byte(w), byte(w>>8) & 0x3f, byte(h), byte(h>>8) & 0x3f}
}

func liVp8xChunk(flags byte, w, h uint32) []byte {
	d := make([]byte, 10)
	d[0] = flags
	w--
	h--
	d[4], d[5], d[6] = byte(w), byte(w>>8), byte(w>>16)
	d[7], d[8], d[9] = byte(h), byte(h>>8), byte(h>>16)
	return liRiffChunk("VP8X", d)
}

func liBmpPatch(b []byte, off int, v uint32) []byte {
	b = liCopy(b)
	binary.LittleEndian.PutUint32(b[off:], v)
	return b
}

func linkImageInputs() [][2]any {
	var cases [][2]any
	add := func(name string, b []byte) { cases = append(cases, [2]any{name, b}) }

	// --- PNG -----------------------------------------------------------------------------
	pngRGBA := liEncode(func(w io.Writer) error { return png.Encode(w, liRGBA(3, 2, false)) })
	add("png rgba", pngRGBA)
	add("png gray", liEncode(func(w io.Writer) error { return png.Encode(w, liGray(5, 7)) }))
	add("png gray16", liEncode(func(w io.Writer) error {
		m := image.NewGray16(image.Rect(0, 0, 4, 9))
		return png.Encode(w, m)
	}))
	add("png paletted", liEncode(func(w io.Writer) error { return png.Encode(w, liPaletted(6, 2, false)) }))
	add("png paletted trns", liEncode(func(w io.Writer) error { return png.Encode(w, liPaletted(6, 2, true)) }))
	add("png signature only", []byte(liPngSig))
	add("png truncated in IHDR", pngRGBA[:20])
	add("png truncated before IHDR crc", pngRGBA[:30])
	add("png IHDR only", pngRGBA[:33])
	bad := liCopy(pngRGBA)
	bad[32] ^= 0xff
	add("png bad IHDR crc", bad)
	add("png interlaced", liConcat([]byte(liPngSig), liPngIHDR(9, 4, 8, 6, 1)))
	add("png bad interlace", liConcat([]byte(liPngSig), liPngIHDR(9, 4, 8, 6, 2)))
	add("png compression method", liConcat([]byte(liPngSig), func() []byte {
		c := liPngIHDR(9, 4, 8, 6, 0)
		c[8+10] = 1
		return liPngChunk("IHDR", c[8:21])
	}()))
	add("png zero width", liConcat([]byte(liPngSig), liPngIHDR(0, 4, 8, 6, 0)))
	add("png negative height", liConcat([]byte(liPngSig), liPngIHDR(4, 0x80000000, 8, 6, 0)))
	add("png huge", liConcat([]byte(liPngSig), liPngIHDR(0x7fffffff, 0x7fffffff, 8, 6, 0)))
	add("png depth 3", liConcat([]byte(liPngSig), liPngIHDR(4, 4, 3, 0, 0)))
	add("png 16-bit paletted", liConcat([]byte(liPngSig), liPngIHDR(4, 4, 16, 3, 0)))
	add("png 16-bit truecolor alpha", liConcat([]byte(liPngSig), liPngIHDR(11, 12, 16, 6, 0)))
	add("png IHDR length 12", liConcat([]byte(liPngSig), liPngChunk("IHDR", make([]byte, 12))))
	add("png text before IHDR", liConcat([]byte(liPngSig), liPngChunk("tEXt", []byte("k\x00v")), liPngIHDR(7, 8, 8, 2, 0)))
	add("png text before IHDR bad crc", liConcat([]byte(liPngSig), func() []byte {
		c := liPngChunk("tEXt", []byte("k\x00v"))
		c[len(c)-1] ^= 1
		return c
	}(), liPngIHDR(7, 8, 8, 2, 0)))
	add("png huge ancillary length", liConcat([]byte(liPngSig), []byte("\x80\x00\x00\x00tEXt")))
	add("png PLTE before IHDR", liConcat([]byte(liPngSig), liPngChunk("PLTE", []byte{1, 2, 3}), liPngIHDR(7, 8, 8, 3, 0)))
	add("png IDAT first", liConcat([]byte(liPngSig), liPngChunk("IDAT", nil)))
	add("png IEND first", liConcat([]byte(liPngSig), liPngChunk("IEND", nil)))
	add("png tRNS first", liConcat([]byte(liPngSig), liPngChunk("tRNS", []byte{0, 1})))
	add("png paletted no PLTE", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("IDAT", []byte{1})))
	add("png paletted PLTE then IDAT", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3, 4, 5, 6}), liPngChunk("IDAT", []byte{1})))
	add("png paletted PLTE then end", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3, 4, 5, 6})))
	add("png paletted PLTE too long for depth", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 1, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3, 4, 5, 6, 7, 8, 9})))
	add("png paletted PLTE not multiple of 3", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3, 4})))
	add("png paletted PLTE bad crc", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), func() []byte {
		c := liPngChunk("PLTE", []byte{1, 2, 3})
		c[len(c)-1] ^= 1
		return c
	}()))
	add("png paletted tRNS too long", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3}), liPngChunk("tRNS", make([]byte, 257))))
	add("png paletted tRNS longer than palette", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3}), liPngChunk("tRNS", make([]byte, 4))))
	add("png paletted two PLTE", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3}), liPngChunk("PLTE", []byte{1, 2, 3})))
	add("png paletted IEND after PLTE", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngChunk("PLTE", []byte{1, 2, 3}), liPngChunk("IEND", nil)))
	add("png paletted text between", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 4, 3, 0), liPngChunk("tEXt", []byte("a\x00b")), liPngChunk("PLTE", []byte{1, 2, 3}), liPngChunk("tRNS", []byte{0})))
	add("png two IHDR paletted", liConcat([]byte(liPngSig), liPngIHDR(3, 3, 8, 3, 0), liPngIHDR(3, 3, 8, 3, 0)))

	// --- JPEG ----------------------------------------------------------------------------
	jpg := liEncode(func(w io.Writer) error { return jpeg.Encode(w, liRGBA(4, 3, true), nil) })
	add("jpeg baseline", jpg)
	add("jpeg gray", liEncode(func(w io.Writer) error { return jpeg.Encode(w, liGray(7, 5), nil) }))
	prog := liCopy(jpg)
	if i := bytes.Index(prog, []byte{0xff, 0xc0}); i >= 0 {
		prog[i+1] = 0xc2
	}
	add("jpeg progressive marker", prog)
	add("jpeg soi only", []byte{0xff, 0xd8})
	add("jpeg truncated", jpg[:len(jpg)/3])
	for v := uint16(1); v <= 9; v++ {
		add("jpeg exif le orientation "+string(rune('0'+v)), liSpliceAfterSOI(jpg, liApp1(liExifPayload(liOrientationTIFF(binary.LittleEndian, v)))))
	}
	add("jpeg exif be orientation 6", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liOrientationTIFF(binary.BigEndian, 6)))))
	add("jpeg exif orientation as long", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(binary.LittleEndian),
		liIfdBytes(binary.LittleEndian, []liIfdEntry{{0x0112, 4, 1, liLong(binary.LittleEndian, 6)}}, 0))))))
	add("jpeg exif orientation signed short", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(binary.LittleEndian),
		liIfdBytes(binary.LittleEndian, []liIfdEntry{{0x0112, 8, 1, liShort(binary.LittleEndian, 7)}}, 0))))))
	add("jpeg exif orientation count 2", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(binary.LittleEndian),
		liIfdBytes(binary.LittleEndian, []liIfdEntry{{0x0112, 3, 2, []byte{6, 0, 6, 0}}}, 0))))))
	add("jpeg exif orientation count 0", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(binary.LittleEndian),
		liIfdBytes(binary.LittleEndian, []liIfdEntry{{0x0112, 3, 0, []byte{6, 0, 0, 0}}, {0x0112, 3, 1, liShort(binary.LittleEndian, 6)}}, 0))))))
	add("jpeg exif orientation huge count", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(binary.LittleEndian),
		liIfdBytes(binary.LittleEndian, []liIfdEntry{{0x0112, 3, 0x10001, liShort(binary.LittleEndian, 6)}, {0x0112, 3, 1, liShort(binary.LittleEndian, 8)}}, 0))))))
	add("jpeg exif unknown type first", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(binary.LittleEndian),
		liIfdBytes(binary.LittleEndian, []liIfdEntry{{0x010f, 13, 1, liLong(binary.LittleEndian, 0)}, {0x0112, 3, 1, liShort(binary.LittleEndian, 6)}}, 0))))))
	add("jpeg exif bad header", liSpliceAfterSOI(jpg, liApp1(append([]byte("Exxx\x00\x00"), liOrientationTIFF(binary.LittleEndian, 6)...))))
	add("jpeg exif bad byte order", liSpliceAfterSOI(jpg, liApp1(liExifPayload(append([]byte("XX"), liOrientationTIFF(binary.LittleEndian, 6)[2:]...)))))
	add("jpeg exif ifd0 offset below 8", liSpliceAfterSOI(jpg, liApp1(liExifPayload([]byte("II\x2a\x00\x04\x00\x00\x00")))))
	add("jpeg exif empty app1", liSpliceAfterSOI(jpg, liApp1(nil)))
	add("jpeg exif xmp app1 first", liSpliceAfterSOI(jpg, liApp1([]byte("http://ns.adobe.com/xap/1.0/\x00<x/>")), liApp1(liExifPayload(liOrientationTIFF(binary.LittleEndian, 6)))))
	add("jpeg exif after app0", liSpliceAfterSOI(jpg, liJpegSegment(0xe0, []byte("JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")), liApp1(liExifPayload(liOrientationTIFF(binary.LittleEndian, 8)))))
	{
		// Orientation in the Exif sub-IFD, reached through the 0x8769 pointer.
		le := binary.LittleEndian
		sub := liIfdBytes(le, []liIfdEntry{{0x0112, 3, 1, liShort(le, 5)}}, 0)
		ifd0 := liIfdBytes(le, []liIfdEntry{{0x8769, 4, 1, liLong(le, 8+18)}}, 0)
		add("jpeg exif orientation in exif ifd", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0, sub)))))
		// The same pointer twice: the second is skipped.
		ifd0b := liIfdBytes(le, []liIfdEntry{{0x8769, 4, 1, liLong(le, 8+30)}, {0x8769, 4, 1, liLong(le, 8+30+6)}}, 0)
		subEmpty := liIfdBytes(le, nil, 0)
		add("jpeg exif pointer seen twice", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0b, subEmpty, sub)))))
		// A pointer typed SHORT is an error that ends the walk.
		ifd0c := liIfdBytes(le, []liIfdEntry{{0x8769, 3, 1, liShort(le, 26)}, {0x0112, 3, 1, liShort(le, 6)}}, 0)
		add("jpeg exif pointer typed short", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0c)))))
		// A pointer with two offsets.
		sub2 := liIfdBytes(le, []liIfdEntry{{0x0112, 3, 1, liShort(le, 7)}}, 0)
		ifd0d := liIfdBytes(le, []liIfdEntry{{0x014a, 4, 2, liLong(le, 8+18)}}, 0)
		add("jpeg exif subifd array", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0d, liLong(le, 8+18+8+6), liLong(le, 8+18+8), subEmpty, sub2)))))
		// Orientation in IFD1.
		ifd0e := liIfdBytes(le, []liIfdEntry{{0x010f, 2, 4, []byte("abc\x00")}}, 8+18)
		add("jpeg exif orientation in ifd1", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0e, sub)))))
		// A tag count that runs past the end of the segment.
		trunc := liConcat(liTiffHeader(le), []byte{3, 0}, liConcat(liIfdBytes(le, []liIfdEntry{{0x010f, 2, 4, []byte("abc\x00")}}, 0)[2:14]))
		add("jpeg exif ifd runs out", liSpliceAfterSOI(jpg, liApp1(liExifPayload(trunc))))
		// A value offset that points past the end.
		ifd0f := liIfdBytes(le, []liIfdEntry{{0x0112, 3, 3, liLong(le, 4000)}, {0x0112, 3, 1, liShort(le, 6)}}, 0)
		add("jpeg exif offset past end", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0f)))))
		// Ascii and rational values before the orientation.
		ifd0g := liIfdBytes(le, []liIfdEntry{{0x0112, 2, 2, []byte("x\x00\x00\x00")}, {0x0112, 5, 1, liLong(le, 8+30)}, {0x0112, 3, 1, liShort(le, 8)}}, 0)
		add("jpeg exif ascii and rational orientation", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0g, liLong(le, 1), liLong(le, 0))))))
		// The 5,000-tag limit: the orientation as the 5,000th handled tag and as the 5,001st.
		for _, n := range []int{4999, 5000} {
			var entries []liIfdEntry
			for i := 0; i < n; i++ {
				entries = append(entries, liIfdEntry{0x010f, 2, 4, []byte("abc\x00")})
			}
			entries = append(entries, liIfdEntry{0x0112, 3, 1, liShort(le, 6)})
			name := "jpeg exif orientation after 4999 tags"
			if n == 5000 {
				name = "jpeg exif orientation after 5000 tags"
			}
			add(name, liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), liIfdBytes(le, entries, 0))))))
		}
		// A tag too large to handle (valLen > 10000) is skipped without counting.
		ifd0h := liIfdBytes(le, []liIfdEntry{{0x0112, 3, 6000, liLong(le, 0)}, {0x0112, 3, 1, liShort(le, 6)}}, 0)
		// The segment ends where IFD0's next-IFD offset would be: imagemeta's first io.EOF does
		// not fail the read but returns its scratch buffer, which still holds the last entry's
		// count (8) — so IFD1 is read at offset 8, where an orientation sits.
		{
			ifd1 := liIfdBytes(le, []liIfdEntry{{0x0112, 3, 1, liShort(le, 6)}}, 0)
			ifd0 := liIfdBytes(le, []liIfdEntry{{0x010f, 2, 8, liLong(le, 0)}}, 0)
			ifd0 = ifd0[:len(ifd0)-4]
			stale := liConcat([]byte("II\x2a\x00"), liLong(le, 8+uint32(len(ifd1))), ifd1, ifd0)
			add("jpeg exif stale eof offset", liSpliceAfterSOI(jpg, liApp1(liExifPayload(stale))))
		}
		// An oversized pointer is skipped before it is dereferenced.
		ifd0i := liIfdBytes(le, []liIfdEntry{{0x8769, 4, 3000, liLong(le, 4000)}, {0x0112, 3, 1, liShort(le, 6)}}, 0)
		add("jpeg exif oversized pointer skipped", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0i)))))
		// A second pointer of a seen kind returns without consuming its value field, so the
		// entries after it are read four bytes early.
		ifd0j := liIfdBytes(le, []liIfdEntry{{0x8769, 4, 1, liLong(le, 8+42)}, {0x8769, 4, 1, liLong(le, 8+42+6)}, {0x010f, 2, 4, []byte("abc\x00")}}, 0)
		add("jpeg exif pointer seen twice then tag", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0j, subEmpty, sub)))))
		add("jpeg exif oversized orientation skipped", liSpliceAfterSOI(jpg, liApp1(liExifPayload(liConcat(liTiffHeader(le), ifd0h)))))
	}
	// Header walks: hand-built.
	add("jpeg hand baseline", liHandJPEG(liJpegSegment(0xc0, liSof(8, 33, 44, liYcc)), liSosMarker))
	add("jpeg hand gray", liHandJPEG(liJpegSegment(0xc1, liSof(8, 3, 4, [][3]byte{{1, 0x21, 0}})), liSosMarker))
	add("jpeg hand cmyk", liHandJPEG(liJpegSegment(0xc0, liSof(8, 30, 40, [][3]byte{{1, 0x11, 0}, {2, 0x11, 0}, {3, 0x11, 0}, {4, 0x11, 0}})), liSosMarker))
	add("jpeg hand ycck 22", liHandJPEG(liJpegSegment(0xc0, liSof(8, 30, 40, [][3]byte{{1, 0x22, 0}, {2, 0x11, 0}, {3, 0x11, 0}, {4, 0x22, 0}})), liSosMarker))
	add("jpeg hand cmyk bad k", liHandJPEG(liJpegSegment(0xc0, liSof(8, 30, 40, [][3]byte{{1, 0x22, 0}, {2, 0x11, 0}, {3, 0x11, 0}, {4, 0x11, 0}})), liSosMarker))
	add("jpeg hand cmyk bad first", liHandJPEG(liJpegSegment(0xc0, liSof(8, 30, 40, [][3]byte{{1, 0x21, 0}, {2, 0x11, 0}, {3, 0x11, 0}, {4, 0x21, 0}})), liSosMarker))
	add("jpeg hand cmyk bad middle", liHandJPEG(liJpegSegment(0xc0, liSof(8, 30, 40, [][3]byte{{1, 0x11, 0}, {2, 0x12, 0}, {3, 0x11, 0}, {4, 0x11, 0}})), liSosMarker))
	add("jpeg hand jfif early return", liHandJPEG(liJpegSegment(0xe0, []byte("JFIF\x00\x01\x02")), liJpegSegment(0xc2, liSof(8, 5, 6, liYcc))))
	add("jpeg hand jfif bad sof", liHandJPEG(liJpegSegment(0xe0, []byte("JFIF\x00")), liJpegSegment(0xc0, liSof(12, 5, 6, liYcc))))
	add("jpeg hand app0 short", liHandJPEG(liJpegSegment(0xe0, []byte("JF")), liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand not jfif needs sos", liHandJPEG(liJpegSegment(0xe0, []byte("JFXF\x00")), liJpegSegment(0xc0, liSof(8, 5, 6, liYcc))))
	add("jpeg hand eoi", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), []byte{0xff, 0xd9}))
	add("jpeg hand progressive eoi", liHandJPEG(liJpegSegment(0xc2, liSof(8, 5, 6, liYcc)), []byte{0xff, 0xd9}))
	add("jpeg hand sos before sof", liHandJPEG(liSosMarker))
	add("jpeg hand two sof", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand precision 12", liHandJPEG(liJpegSegment(0xc0, liSof(12, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand two components", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x11, 0}, {2, 0x11, 0}})), liSosMarker))
	add("jpeg hand wrong component count", liHandJPEG(liJpegSegment(0xc0, append(liSof(8, 5, 6, liYcc)[:5], append([]byte{2}, liSof(8, 5, 6, liYcc)[6:]...)...)), liSosMarker))
	add("jpeg hand repeated component", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x22, 0}, {1, 0x11, 1}, {3, 0x11, 1}})), liSosMarker))
	add("jpeg hand tq 4", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x22, 4}, {2, 0x11, 1}, {3, 0x11, 1}})), liSosMarker))
	add("jpeg hand h 3", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x31, 0}, {2, 0x11, 1}, {3, 0x11, 1}})), liSosMarker))
	add("jpeg hand h 5", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x51, 0}, {2, 0x11, 1}, {3, 0x11, 1}})), liSosMarker))
	add("jpeg hand v 4", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x14, 0}, {2, 0x11, 1}, {3, 0x11, 1}})), liSosMarker))
	add("jpeg hand gray v 4 ok", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x14, 0}})), liSosMarker))
	add("jpeg hand cb not divisor", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x21, 0}, {2, 0x12, 1}, {3, 0x12, 1}})), liSosMarker))
	add("jpeg hand cr differs", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, [][3]byte{{1, 0x22, 0}, {2, 0x11, 1}, {3, 0x21, 1}})), liSosMarker))
	add("jpeg hand extraneous bytes", liHandJPEG([]byte{0x00, 0x12, 0x34}, liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand fill bytes", liHandJPEG([]byte{0xff, 0xff, 0xff}, liJpegSegment(0xc0, liSof(8, 5, 6, liYcc))[1:], liSosMarker))
	add("jpeg hand ff00", liHandJPEG([]byte{0xff, 0x00}, liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand rst", liHandJPEG([]byte{0xff, 0xd3}, liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand unknown low marker", liHandJPEG(liJpegSegment(0x05, nil), liSosMarker))
	add("jpeg hand unknown high marker", liHandJPEG(liJpegSegment(0xc3, nil), liSosMarker))
	add("jpeg hand com and app", liHandJPEG(liJpegSegment(0xfe, []byte("hi")), liJpegSegment(0xe5, []byte("x")), liJpegSegment(0xee, []byte("Adobe\x00\x01\x00\x00\x00\x00\x01")), liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand dht dqt dri", liHandJPEG(liJpegSegment(0xc4, []byte{1, 2, 3}), liJpegSegment(0xdb, []byte{9}), liJpegSegment(0xdd, []byte{0, 4}), liJpegSegment(0xc0, liSof(8, 5, 6, liYcc)), liSosMarker))
	add("jpeg hand short segment length", liHandJPEG([]byte{0xff, 0xe3, 0x00, 0x01}))
	add("jpeg hand segment past end", liHandJPEG([]byte{0xff, 0xe3, 0x00, 0x40, 1, 2}))
	add("jpeg hand truncated sof", liHandJPEG(liJpegSegment(0xc0, liSof(8, 5, 6, liYcc))[:9]))
	add("jpeg hand zero size", liHandJPEG(liJpegSegment(0xc0, liSof(8, 0, 0, liYcc)), liSosMarker))
	add("jpeg hand exif rotates zero-height", liHandJPEG(liApp1(liExifPayload(liOrientationTIFF(binary.BigEndian, 6))), liJpegSegment(0xc0, liSof(8, 0, 9, liYcc)), liSosMarker))

	// --- GIF -----------------------------------------------------------------------------
	pal := color.Palette{color.Black, color.White, color.RGBA{255, 0, 0, 255}, color.RGBA{0, 0, 255, 255}}
	frame := func(i int) *image.Paletted {
		m := image.NewPaletted(image.Rect(0, 0, 5, 4), pal)
		for j := range m.Pix {
			m.Pix[j] = uint8((i + j) % 4)
		}
		return m
	}
	anim := liEncode(func(w io.Writer) error {
		return gif.EncodeAll(w, &gif.GIF{Image: []*image.Paletted{frame(0), frame(1), frame(2)}, Delay: []int{5, 5, 5}, LoopCount: 2})
	})
	add("gif animated", anim)
	add("gif single", liEncode(func(w io.Writer) error { return gif.Encode(w, frame(3), nil) }))
	add("gif truncated mid frame", anim[:len(anim)-20])
	add("gif no trailer", anim[:len(anim)-1])
	add("gif header only", anim[:13])
	add("gif truncated header", anim[:10])
	add("gif truncated color table", anim[:20])
	g87 := liCopy(anim)
	copy(g87, "GIF87a")
	add("gif 87a", g87)
	for _, b := range []byte{'x', 0x00, 0xff, '"', '\t'} {
		v := liCopy(anim)
		v[4] = b
		add("gif bad version "+string(rune('a'+b%26)), v)
	}
	add("gen gif zero frames", imgutils.GenGIFData(3, 3, 0))
	add("gen gif two frames", imgutils.GenGIFData(3, 3, 2))
	add("gen gif frame bigger than screen", imgutils.GenGIFData(0, 1, 1))
	// The LZW end code before w*h pixels: accepted by CountGIFFrames.
	early := imgutils.GenGIFData(2, 2, 1)
	early[19+5], early[19+7] = 2, 2
	add("gen gif early end code", early)
	zero := imgutils.GenGIFData(2, 2, 1)
	zero[19+5], zero[19+7] = 0, 0
	add("gen gif zero-size frame", zero)
	gifHead := imgutils.GenGIFData(1, 1, 0)
	gifHead = gifHead[:len(gifHead)-1]
	desc := []byte{0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0}
	add("gif lzw invalid code", liConcat(gifHead, desc, []byte{2, 1, 0x07, 0}, []byte{0x3b}))
	add("gif lzw no end code", liConcat(gifHead, desc, []byte{2, 1, 0x04, 0}, []byte{0x3b}))
	add("gif extra sub-block", liConcat(gifHead, desc, []byte{2, 2, 0x4c, 0x01, 2, 'a', 'b', 0}, []byte{0x3b}))
	add("gif one-byte extra sub-block", liConcat(gifHead, desc, []byte{2, 2, 0x4c, 0x01, 1, 'a', 0}, []byte{0x3b}))
	add("gif trailing bytes in sub-block", liConcat(gifHead, desc, []byte{2, 4, 0x4c, 0x01, 'x', 'y', 0}, []byte{0x3b}))
	add("gif missing terminator", liConcat(gifHead, desc, []byte{2, 2, 0x4c, 0x01}))
	add("gif lit width 1", liConcat(gifHead, desc, []byte{1, 2, 0x4c, 0x01, 0}, []byte{0x3b}))
	add("gif lit width 9", liConcat(gifHead, desc, []byte{9, 2, 0x4c, 0x01, 0}, []byte{0x3b}))
	add("gif no color table", liConcat([]byte("GIF89a\x01\x00\x01\x00\x00\x00\x00"), desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif local color table", liConcat([]byte("GIF89a\x01\x00\x01\x00\x00\x00\x00"), []byte{0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0x80, 0, 0, 0, 1, 1, 1}, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif comment extension", liConcat(gifHead, []byte{0x21, 0xfe, 3, 'a', 'b', 'c', 0}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif text extension", liConcat(gifHead, []byte{0x21, 0x01}, make([]byte, 13), []byte{0}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif unknown extension", liConcat(gifHead, []byte{0x21, 0x02, 0}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif graphic control bad size", liConcat(gifHead, []byte{0x21, 0xf9, 5, 0, 0, 0, 0, 0}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif graphic control bad terminator", liConcat(gifHead, []byte{0x21, 0xf9, 4, 0, 0, 0, 0, 7}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif graphic control ok", liConcat(gifHead, []byte{0x21, 0xf9, 4, 1, 2, 0, 3, 0}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif netscape empty", liConcat(gifHead, []byte{0x21, 0xff, 11}, []byte("NETSCAPE2.0"), []byte{0}, desc, []byte{2, 2, 0x4c, 0x01, 0, 0x3b}))
	add("gif application truncated", liConcat(gifHead, []byte{0x21, 0xff, 11}, []byte("NETSC")))
	add("gif unknown block", liConcat(gifHead, []byte{0x99}))
	add("gif truncated descriptor", liConcat(gifHead, desc[:5]))

	// --- BMP -----------------------------------------------------------------------------
	bmp24 := liEncode(func(w io.Writer) error { return bmp.Encode(w, liRGBA(5, 3, true)) })
	bmp32 := liEncode(func(w io.Writer) error { return bmp.Encode(w, liRGBA(5, 3, false)) })
	bmp8 := liEncode(func(w io.Writer) error { return bmp.Encode(w, liGray(6, 2)) })
	add("bmp 24", bmp24)
	add("bmp 32", bmp32)
	add("bmp 8", bmp8)
	h := int32(-3)
	add("bmp top down", liBmpPatch(bmp24, 22, uint32(h)))
	add("bmp compression", liBmpPatch(bmp24, 30, 1))
	add("bmp planes", func() []byte { b := liCopy(bmp24); b[26] = 2; return b }())
	add("bmp info len 12", liBmpPatch(bmp24, 14, 12))
	add("bmp zero width", liBmpPatch(bmp24, 18, 0))
	add("bmp zero both", liBmpPatch(liBmpPatch(bmp24, 18, 0), 22, 0))
	add("bmp negative width", liBmpPatch(bmp24, 18, 0xfffffff0))
	add("bmp min height", liBmpPatch(bmp24, 22, 0x80000000))
	add("bmp huge", liBmpPatch(liBmpPatch(bmp24, 18, 0x7fffffff), 22, 0x7fffffff))
	add("bmp bad offset", liBmpPatch(bmp24, 10, 99))
	add("bmp bpp 16", func() []byte { b := liCopy(bmp24); b[28] = 16; return b }())
	add("bmp 8 truncated palette", bmp8[:60])
	add("bmp 8 palette at eof", bmp8[:54])
	add("bmp 8 color used 3", liBmpPatch(liBmpPatch(bmp8, 46, 3), 10, 14+40+12))
	add("bmp 8 color used too many", liBmpPatch(bmp8, 46, 257))
	add("bmp truncated header", bmp24[:30])
	{
		// A BITMAPV4HEADER with BI_BITFIELDS and the default masks: treated as uncompressed.
		b := make([]byte, 14+108)
		copy(b, "BM")
		binary.LittleEndian.PutUint32(b[10:], 14+108)
		binary.LittleEndian.PutUint32(b[14:], 108)
		binary.LittleEndian.PutUint32(b[18:], 7)
		binary.LittleEndian.PutUint32(b[22:], 2)
		binary.LittleEndian.PutUint16(b[26:], 1)
		binary.LittleEndian.PutUint16(b[28:], 32)
		binary.LittleEndian.PutUint32(b[30:], 3)
		binary.LittleEndian.PutUint32(b[54:], 0xff0000)
		binary.LittleEndian.PutUint32(b[58:], 0xff00)
		binary.LittleEndian.PutUint32(b[62:], 0xff)
		binary.LittleEndian.PutUint32(b[66:], 0xff000000)
		add("bmp v4 bitfields default masks", b)
		add("bmp v4 bitfields other masks", liBmpPatch(b, 62, 0xfe))
		add("bmp v4 truncated", b[:100])
	}

	// --- TIFF ----------------------------------------------------------------------------
	tifRGBA := liEncode(func(w io.Writer) error { return tiff.Encode(w, liRGBA(6, 4, false), nil) })
	add("tiff rgba", tifRGBA)
	add("tiff gray", liEncode(func(w io.Writer) error { return tiff.Encode(w, liGray(3, 3), nil) }))
	add("tiff paletted", liEncode(func(w io.Writer) error { return tiff.Encode(w, liPaletted(4, 4, false), nil) }))
	add("tiff opaque rgb", liEncode(func(w io.Writer) error { return tiff.Encode(w, liRGBA(2, 2, true), nil) }))
	add("tiff header only", tifRGBA[:8])
	add("tiff bad ifd offset", func() []byte { b := liCopy(tifRGBA); binary.LittleEndian.PutUint32(b[4:], 1<<30); return b }())
	add("tiff truncated", tifRGBA[:len(tifRGBA)-40])
	{
		for _, order := range []binary.ByteOrder{binary.LittleEndian, binary.BigEndian} {
			sfx := " le"
			if order == binary.BigEndian {
				sfx = " be"
			}
			s := func(v uint16) []byte { return liShort(order, v) }
			l := func(v uint32) []byte { return liLong(order, v) }
			mk := func(entries []liIfdEntry, extra ...[]byte) []byte {
				return liConcat(liTiffHeader(order), liIfdBytes(order, entries, 0), liConcat(extra...))
			}
			add("tiff minimal gray"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 4, 1, l(9)}, {258, 3, 1, s(8)}, {262, 3, 1, s(1)}}))
			add("tiff default bits"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}}))
			add("tiff rgb default bits"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {262, 3, 1, s(2)}}))
			add("tiff empty bits"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 0, s(0)}}))
			add("tiff bits 4"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 1, s(4)}}))
			add("tiff zero width"+sfx, mk([]liIfdEntry{{256, 3, 1, s(0)}, {257, 3, 1, s(9)}}))
			add("tiff unsorted"+sfx, mk([]liIfdEntry{{257, 3, 1, s(9)}, {256, 3, 1, s(7)}}))
			add("tiff duplicate tag"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {256, 3, 1, s(9)}}))
			add("tiff cmyk"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 1, s(8)}, {262, 3, 1, s(5)}}))
			add("tiff rgb 3x8 inline bytes"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 1, 3, []byte{8, 8, 8, 0}}, {262, 3, 1, s(2)}}))
			add("tiff rgb 3x8 offset"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 3, l(8 + 2 + 48 + 4)}, {262, 3, 1, s(2)}}, s(8), s(8), s(8)))
			add("tiff rgb 16 mixed"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 3, l(8 + 2 + 48 + 4)}, {262, 3, 1, s(2)}}, s(16), s(8), s(16)))
			add("tiff rgba extra 2"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 4, l(8 + 2 + 60 + 4)}, {262, 3, 1, s(2)}, {338, 3, 1, s(2)}}, s(8), s(8), s(8), s(8)))
			add("tiff rgba extra 0"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 3, 4, l(8 + 2 + 60 + 4)}, {262, 3, 1, s(2)}, {338, 3, 1, s(0)}}, s(8), s(8), s(8), s(8)))
			add("tiff rgb 2 samples"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 1, 2, []byte{8, 8, 0, 0}}, {262, 3, 1, s(2)}}))
			add("tiff gray extra samples"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {258, 1, 2, []byte{8, 8, 0, 0}}, {262, 3, 1, s(1)}}))
			add("tiff ascii width"+sfx, mk([]liIfdEntry{{256, 2, 2, []byte("7\x00\x00\x00")}, {257, 3, 1, s(9)}}))
			add("tiff rational width"+sfx, mk([]liIfdEntry{{256, 5, 1, l(0)}, {257, 3, 1, s(9)}}))
			add("tiff datatype 6"+sfx, mk([]liIfdEntry{{256, 6, 1, l(0)}, {257, 3, 1, s(9)}}))
			add("tiff huge count"+sfx, mk([]liIfdEntry{{256, 4, 0x20000000, l(0)}, {257, 3, 1, s(9)}}))
			add("tiff value past end"+sfx, mk([]liIfdEntry{{256, 4, 2, l(1000)}, {257, 3, 1, s(9)}}))
			add("tiff sample format 2"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {339, 3, 1, s(2)}}))
			add("tiff sample format 1"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {339, 3, 1, s(1)}}))
			add("tiff colormap bad"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {320, 3, 2, l(0)}}))
			add("tiff colormap ok"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {262, 3, 1, s(3)}, {320, 3, 6, l(8 + 2 + 48 + 4)}}, s(1), s(2), s(3), s(4), s(5), s(6)))
			add("tiff strip offsets"+sfx, mk([]liIfdEntry{{256, 3, 1, s(7)}, {257, 3, 1, s(9)}, {273, 4, 99, l(1 << 30)}}))
			add("tiff huge dims"+sfx, mk([]liIfdEntry{{256, 4, 1, l(0xffffffff)}, {257, 4, 1, l(0xffffffff)}}))
			add("tiff no entries"+sfx, mk(nil))
		}
	}

	// --- WebP ----------------------------------------------------------------------------
	add("webp vp8l", liWebpFile(liRiffChunk("VP8L", append(liVp8lHeader(300, 200, 0), 0, 0, 0))))
	add("webp vp8l bad version", liWebpFile(liRiffChunk("VP8L", liVp8lHeader(3, 2, 1))))
	add("webp vp8l bad magic", liWebpFile(liRiffChunk("VP8L", []byte{0x2e, 0, 0, 0, 0})))
	add("webp vp8l short chunk", liWebpFile(liRiffChunk("VP8L", liVp8lHeader(3, 2, 0)[:3])))
	add("webp vp8 key", liWebpFile(liRiffChunk("VP8 ", liVp8Header(true, 640, 480))))
	add("webp vp8 inter", liWebpFile(liRiffChunk("VP8 ", liVp8Header(false, 640, 480))))
	add("webp vp8 bad sync", liWebpFile(liRiffChunk("VP8 ", func() []byte { b := liVp8Header(true, 6, 4); b[3] = 0; return b }())))
	add("webp vp8 short", liWebpFile(liRiffChunk("VP8 ", liVp8Header(true, 6, 4)[:6])))
	add("webp vp8 two bytes", liWebpFile(liRiffChunk("VP8 ", []byte{0, 0})))
	add("webp vp8x", liWebpFile(liVp8xChunk(0, 1000, 700)))
	add("webp vp8x alpha", liWebpFile(liVp8xChunk(0x10, 16, 16)))
	add("webp vp8x too big", liWebpFile(liVp8xChunk(0, 1<<24, 1<<24)))
	add("webp vp8x bad len", liWebpFile(liRiffChunk("VP8X", make([]byte, 9))))
	add("webp unknown then vp8l", liWebpFile(liRiffChunk("VP8Z", []byte{1, 2, 3}), liRiffChunk("VP8L", liVp8lHeader(9, 8, 0))))
	add("webp unknown only", liWebpFile(liRiffChunk("VP8Z", []byte{1, 2})))
	add("webp riff too long", func() []byte {
		b := liWebpFile(liRiffChunk("VP8L", liVp8lHeader(9, 8, 0)))
		binary.LittleEndian.PutUint32(b[4:], 100)
		return b
	}())
	add("webp riff too short", func() []byte {
		b := liWebpFile(liRiffChunk("VP8L", liVp8lHeader(9, 8, 0)))
		binary.LittleEndian.PutUint32(b[4:], 8)
		return b
	}())
	add("webp chunk longer than riff", func() []byte {
		b := liWebpFile(liRiffChunk("VP8L", liVp8lHeader(9, 8, 0)))
		binary.LittleEndian.PutUint32(b[16:], 50)
		return b
	}())
	add("webp missing padding", liWebpFile(liRiffChunk("VP8Z", []byte{1, 2, 3}))[:24])
	add("webp magic only", []byte("RIFF\x04\x00\x00\x00WEBPVP8"))
	add("webp riff len 3", []byte("RIFF\x03\x00\x00\x00WEBPVP8L"))

	// --- Not images ----------------------------------------------------------------------
	add("empty", nil)
	add("text", []byte("hello, world"))
	add("gif prefix only", []byte("GIF8"))
	add("bmp magic short", []byte("BM\x00\x00\x00\x00\x00\x00\x00"))
	add("svg", []byte(`<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>`))

	return cases
}

func sniffInputs() []sniffCase {
	var cases []sniffCase
	add := func(name string, b []byte) { cases = append(cases, sniffCase{Name: name, Input: b}) }
	html := []string{"<!DOCTYPE HTML", "<HTML", "<HEAD", "<SCRIPT", "<IFRAME", "<H1", "<DIV", "<FONT", "<TABLE", "<A", "<STYLE", "<TITLE", "<B", "<BODY", "<BR", "<P", "<!--"}
	for _, h := range html {
		add(h+" space", []byte(h+" x"))
		add(h+" gt", []byte(h+">"))
		add(h+" lower", []byte(" \t\n\r\x0c"+string(bytes.ToLower([]byte(h)))+">"))
		add(h+" other terminator", []byte(h+"x"))
		add(h+" at end", []byte(h))
	}
	for name, s := range map[string]string{
		"xml":            "<?xml version=\"1.0\"?>",
		"xml ws":         "\n <?xml",
		"xml upper":      "<?XML",
		"pdf":            "%PDF-1.4",
		"ps":             "%!PS-Adobe-3.0",
		"utf16be":        "\xfe\xff\x00\x41",
		"utf16le":        "\xff\xfe\x41\x00",
		"utf8 bom":       "\xef\xbb\xbfhello",
		"ico":            "\x00\x00\x01\x00rest",
		"cur":            "\x00\x00\x02\x00rest",
		"bmp":            "BMxx",
		"gif87":          "GIF87a...",
		"gif89":          "GIF89a...",
		"webp":           "RIFF\x10\x00\x00\x00WEBPVP8 ",
		"png":            "\x89PNG\x0d\x0a\x1a\x0a",
		"jpeg":           "\xff\xd8\xff\xe0",
		"jpeg 2 bytes":   "\xff\xd8",
		"aiff":           "FORM\x00\x00\x00\x00AIFF",
		"mp3":            "ID3\x03",
		"ogg":            "OggS\x00\x02",
		"midi":           "MThd\x00\x00\x00\x06",
		"avi":            "RIFF\x00\x00\x00\x00AVI LIST",
		"wave":           "RIFF\x00\x00\x00\x00WAVEfmt ",
		"mp4":            "\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
		"mp4 at 12 only": "\x00\x00\x00\x10ftypisommp4 ",
		"mp4 bad size":   "\x00\x00\x00\x17ftypmp42\x00\x00\x00\x00mp42isom",
		"mp4 too big":    "\x00\x00\x01\x00ftypmp42\x00\x00\x00\x00mp42",
		"webm":           "\x1a\x45\xdf\xa3",
		"ttf":            "\x00\x01\x00\x00",
		"otf":            "OTTO",
		"ttc":            "ttcf",
		"woff":           "wOFF",
		"woff2":          "wOF2",
		"gzip":           "\x1f\x8b\x08\x00",
		"zip":            "PK\x03\x04",
		"rar4":           "Rar!\x1a\x07\x00",
		"rar5":           "Rar!\x1a\x07\x01\x00",
		"wasm":           "\x00asm\x01",
		"text":           "hello world",
		"text with ctrl": "hello\x01world",
		"text esc":       "a\x1bb",
		"text vt":        "a\x0bb",
		"text ff":        "a\x0cb",
		"binary":         "\x00\x00\x00\x00",
		"ws only":        "   \n\t",
		"empty":          "",
		"html after ws":  "\n\n<html><body>",
	} {
		add(name, []byte(s))
	}
	eot := make([]byte, 36)
	copy(eot[34:], "LP")
	add("eot", eot)
	long := bytes.Repeat([]byte("a"), 600)
	long[520] = 0x01
	add("control past 512", long)
	long2 := bytes.Repeat([]byte("a"), 600)
	long2[100] = 0x01
	add("control before 512", long2)
	for i := range cases {
		cases[i].Type = http.DetectContentType(cases[i].Input)
	}
	return cases
}

func writeLinkImageBehaviourFixture(outDir string) error {
	var images []linkImageCase
	for _, c := range linkImageInputs() {
		images = append(images, runLinkImageCase(c[0].(string), c[1].([]byte)))
	}
	sniff := sniffInputs()
	// Map iteration order is random; sort for determinism.
	sortSniff(sniff)
	out := map[string]any{
		"images": images,
		"sniff":  sniff,
	}
	blob, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_link_image.json"), append(blob, '\n'), 0o644)
}

func sortSniff(cases []sniffCase) {
	for i := 1; i < len(cases); i++ {
		for j := i; j > 0 && cases[j].Name < cases[j-1].Name; j-- {
			cases[j], cases[j-1] = cases[j-1], cases[j]
		}
	}
}
