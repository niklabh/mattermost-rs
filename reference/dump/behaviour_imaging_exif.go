package main

// The EXIF orientation stage of the imaging oracle: `imaging.GetImageOrientation`
// (channels/app/imaging/orientation.go) over `github.com/bep/imagemeta`.
//
// The only thing Mattermost reads out of the walk is one number, and an error turns it into 1
// (`Upright`). So the recorded answer per file is that pair — the orientation returned and whether
// an error came back — under both reader shapes the server hands in:
//
//   - "seeker": a `bytes.Reader` (prepareImage, AdjustImage, DoUploadFileExpectModification);
//   - "stream": a plain `io.Reader` (preprocessImage's `io.MultiReader`), which the function wraps
//     in its own `bufReadSeeker` with different seek-past-the-end semantics.
//
// The corpus is hand-built TIFF structures carried all four ways GetImageOrientation accepts —
// inside a JPEG APP1, inside a PNG eXIf, as a TIFF file in its own right, and inside a WebP EXIF
// chunk — walking the branches that decide the number: tag type (SHORT and SSHORT are read, LONG
// is not), count, which IFD it sits in (IFD1 and sub-IFDs count, because Mattermost's
// ShouldHandleTag replaces imagemeta's IFD0-only default), what comes before it (an unknown type
// aborts the walk), pointer loops, and the JPEG segment order (an XMP APP1 uses the same marker as
// EXIF and consumes the EXIF source).
//
// The two container formats added later each have a trap of their own, and both are recorded here
// rather than reasoned about: a TIFF's EXIF walk runs on the *caller's* reader, so it is the one
// format where the seeker and the stream shapes give different answers for the same bytes
// (tiff_padded_*); and imagemeta skips exactly chunkLen in a WebP and never the RIFF pad byte, so
// a spec-conforming odd chunk hides everything behind it (webp_odd_chunk_padded_before_exif
// against webp_odd_chunk_unpadded_before_exif).

import (
	"bytes"
	"encoding/binary"
	"image/jpeg"
	"image/png"
	"io"

	mmimaging "github.com/mattermost/mattermost/server/v8/channels/app/imaging"
)

type ifdEntry struct {
	tag, typ uint16
	count    uint32
	value    []byte // raw value bytes; stored inline when <= 4, else at an offset
	subIFD   int    // when > 0: the value is the offset of ifds[subIFD]
	rawOff   *uint32
}

type tiffSpec struct {
	le    bool
	ifds  [][]ifdEntry
	chain bool // IFD0's next pointer names IFD1 (ifds[1])
	// Overrides for broken headers.
	ifd0Offset *uint32
	order      string // replaces the byte-order marker when set
	magic      *uint16
	// numTags replaces IFD0's entry count, so the walk can be pointed past the entries that
	// are really there.
	numTags *uint16
	// pad inserts zero bytes between the 8-byte header and the first IFD, so an IFD offset
	// larger than 8 can still be a *correct* one.
	pad int
}

func (t tiffSpec) bo() binary.ByteOrder {
	if t.le {
		return binary.LittleEndian
	}
	return binary.BigEndian
}

func (t tiffSpec) build() []byte {
	bo := t.bo()
	// Offsets of each IFD, laid out one after another after the 8-byte header.
	offs := make([]uint32, len(t.ifds))
	pos := uint32(8 + t.pad)
	for i, ifd := range t.ifds {
		offs[i] = pos
		size := uint32(2 + 12*len(ifd) + 4)
		for _, e := range ifd {
			if len(e.value) > 4 && e.subIFD == 0 && e.rawOff == nil {
				size += uint32(len(e.value))
			}
		}
		pos += size
	}
	out := make([]byte, pos)
	if t.order != "" {
		copy(out[0:2], t.order)
	} else if t.le {
		copy(out[0:2], "II")
	} else {
		copy(out[0:2], "MM")
	}
	magic := uint16(42)
	if t.magic != nil {
		magic = *t.magic
	}
	bo.PutUint16(out[2:4], magic)
	ifd0 := uint32(8 + t.pad)
	if t.ifd0Offset != nil {
		ifd0 = *t.ifd0Offset
	}
	bo.PutUint32(out[4:8], ifd0)
	for i, ifd := range t.ifds {
		p := offs[i]
		n := uint16(len(ifd))
		if i == 0 && t.numTags != nil {
			n = *t.numTags
		}
		bo.PutUint16(out[p:], n)
		p += 2
		extra := offs[i] + uint32(2+12*len(ifd)+4)
		for _, e := range ifd {
			bo.PutUint16(out[p:], e.tag)
			bo.PutUint16(out[p+2:], e.typ)
			bo.PutUint32(out[p+4:], e.count)
			switch {
			case e.subIFD > 0:
				bo.PutUint32(out[p+8:], offs[e.subIFD])
			case e.rawOff != nil:
				bo.PutUint32(out[p+8:], *e.rawOff)
			case len(e.value) > 4:
				bo.PutUint32(out[p+8:], extra)
				copy(out[extra:], e.value)
				extra += uint32(len(e.value))
			default:
				copy(out[p+8:p+12], e.value)
			}
			p += 12
		}
		if i == 0 && t.chain && len(t.ifds) > 1 {
			bo.PutUint32(out[p:], offs[1])
		}
	}
	return out
}

func u16(bo binary.ByteOrder, v uint16) []byte {
	b := make([]byte, 2)
	bo.PutUint16(b, v)
	return b
}

func u32(bo binary.ByteOrder, v uint32) []byte {
	b := make([]byte, 4)
	bo.PutUint32(b, v)
	return b
}

func orientationEntry(bo binary.ByteOrder, typ uint16, v uint16) ifdEntry {
	return ifdEntry{tag: 0x0112, typ: typ, count: 1, value: u16(bo, v)}
}

var (
	exifBaseJPEG = mustEncode(func(b *bytes.Buffer) error {
		return jpeg.Encode(b, imgSpec{"nrgba", 24, 16, "blocks", "opaque", 4242, 0, ""}.build(), &jpeg.Options{Quality: 90})
	})
	exifBasePNG = mustEncode(func(b *bytes.Buffer) error {
		return (&png.Encoder{CompressionLevel: png.BestCompression}).Encode(b, imgSpec{"nrgba", 24, 16, "blocks", "mixed", 4243, 0, ""}.build())
	})
)

// jpegSegment is one APPn (or other) marker segment, length included.
func jpegSegment(marker byte, payload []byte) []byte {
	b := []byte{0xff, marker, 0, 0}
	binary.BigEndian.PutUint16(b[2:], uint16(len(payload)+2))
	return append(b, payload...)
}

func withJPEGSegments(segs ...[]byte) []byte {
	out := []byte{0xff, 0xd8}
	for _, s := range segs {
		out = append(out, s...)
	}
	return append(out, exifBaseJPEG[2:]...)
}

func exifAPP1(tiff []byte) []byte { return jpegSegment(0xe1, append([]byte("Exif\x00\x00"), tiff...)) }

func withPNGChunks(chunks ...[]byte) []byte {
	// Signature (8) + IHDR (25), then the extra chunks, then the rest.
	out := append([]byte{}, exifBasePNG[:33]...)
	for _, c := range chunks {
		out = append(out, c...)
	}
	return append(out, exifBasePNG[33:]...)
}

// riffChunk builds one RIFF chunk: the fourCC, a little-endian payload length, the payload, and —
// when pad is set — the RIFF pad byte that makes an odd chunk even. imagemeta skips exactly
// chunkLen and never the pad byte, so which of the two a file uses decides where it looks for the
// next chunk id. Both shapes are in the corpus.
func riffChunk(id string, payload []byte, pad bool) []byte {
	b := append([]byte(id), 0, 0, 0, 0)
	binary.LittleEndian.PutUint32(b[4:], uint32(len(payload)))
	b = append(b, payload...)
	if pad && len(payload)%2 == 1 {
		b = append(b, 0)
	}
	return b
}

// riffChunkLen overrides a chunk's declared length, leaving the payload as it is.
func riffChunkLen(chunk []byte, n uint32) []byte {
	out := bytes.Clone(chunk)
	binary.LittleEndian.PutUint32(out[4:], n)
	return out
}

// riffFile wraps chunks in a RIFF container. The declared size is the conventional one
// (payload + the 4-byte form type); imagemeta skips those four bytes without reading them.
func riffFile(form string, chunks ...[]byte) []byte {
	var body []byte
	for _, c := range chunks {
		body = append(body, c...)
	}
	out := append([]byte("RIFF"), 0, 0, 0, 0)
	binary.LittleEndian.PutUint32(out[4:], uint32(len(body)+4))
	out = append(out, form...)
	return append(out, body...)
}

// vp8xChunk is an extended-format header chunk. flags is its first byte: bit 2 is XMP, bit 3 EXIF.
func vp8xChunk(flags byte, w, h int) []byte {
	b := make([]byte, 10)
	b[0] = flags
	w, h = w-1, h-1
	b[4], b[5], b[6] = byte(w), byte(w>>8), byte(w>>16)
	b[7], b[8], b[9] = byte(h), byte(h>>8), byte(h>>16)
	return riffChunk("VP8X", b, true)
}

// webpImageChunk lifts the single image chunk out of one of golang.org/x/image's simple-format
// WebP test files: bytes 12 onwards of `RIFF <size> WEBP <chunk>`. No WebP encoder exists in Go,
// so a real VP8/VP8L payload can only come from a file.
func webpImageChunk(name string) []byte {
	for _, f := range xImageTestdata("testdata", "webp") {
		if f.Name != name {
			continue
		}
		if len(f.Data) < 16 || string(f.Data[:4]) != "RIFF" || string(f.Data[8:12]) != "WEBP" {
			panic("not a RIFF/WEBP file: " + name)
		}
		fcc := string(f.Data[12:16])
		if fcc != "VP8 " && fcc != "VP8L" {
			panic("not a simple-format WebP: " + name + " starts with " + fcc)
		}
		return f.Data[12:]
	}
	panic("no such x/image WebP testdata file: " + name)
}

type exifCase struct {
	Name   string `json:"name"`
	Format string `json:"format"`
	B64    string `json:"b64"`
	// PNG only: the bytes are B64 with an ancillary "abCD" chunk of Pad zero bytes inserted
	// after IHDR — the recipe for inputs too large to carry (the 10 MiB scan limit).
	Pad int `json:"pad,omitempty"`
	// TIFF only: the bytes are B64 with TiffPad zero bytes inserted between the 8-byte header
	// and IFD0, and the header's IFD0 offset raised to match — the same trick as Pad, for the
	// one structure whose walk runs on the caller's reader and so feels the 10 MiB scan limit.
	TiffPad int            `json:"tiff_pad,omitempty"`
	Seeker  map[string]any `json:"seeker"`
	Stream  map[string]any `json:"stream"`
}

func orientationOf(r io.Reader, format string) map[string]any {
	o, err := mmimaging.GetImageOrientation(r, format)
	return map[string]any{"orientation": o, "err": err != nil}
}

func imagingEXIFStage() (map[string]any, error) {
	var cases []exifCase
	add := func(name, format string, data []byte) {
		cases = append(cases, exifCase{
			Name: name, Format: format, B64: b64(data),
			Seeker: orientationOf(bytes.NewReader(data), format),
			Stream: orientationOf(io.MultiReader(bytes.NewReader(data)), format),
		})
	}
	le, be := binary.LittleEndian, binary.BigEndian
	simple := func(isLE bool, entries ...ifdEntry) []byte {
		return tiffSpec{le: isLE, ifds: [][]ifdEntry{entries}}.build()
	}

	for o := uint16(0); o <= 9; o++ {
		add("jpeg_le_"+itoa(int(o)), "jpeg", withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 3, o)))))
		add("jpeg_be_"+itoa(int(o)), "jpeg", withJPEGSegments(exifAPP1(simple(false, orientationEntry(be, 3, o)))))
		add("png_be_"+itoa(int(o)), "png", withPNGChunks(pngChunkBytes("eXIf", simple(false, orientationEntry(be, 3, o)))))
	}
	add("jpeg_65535", "jpeg", withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 3, 65535)))))
	add("mime_format", "image/jpeg", withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 3, 6)))))
	add("mime_png", "image/png", withPNGChunks(pngChunkBytes("eXIf", simple(false, orientationEntry(be, 3, 8)))))
	for _, f := range []string{"gif", "bmp", "", "JPEG", "image/gif", "webp", "tiff"} {
		add("format_"+f, f, withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 3, 6)))))
	}
	// Types.
	add("type_sshort", "jpeg", withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 8, 6)))))
	add("type_long", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 4, count: 1, value: u32(le, 6)}))))
	add("type_byte", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 1, count: 1, value: []byte{6, 0, 0, 0}}))))
	add("type_unknown_13", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 13, count: 1, value: u16(le, 6)}))))
	add("type_zero", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 0, count: 1, value: u16(le, 6)}))))
	add("count_2", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 3, count: 2, value: append(u16(le, 6), u16(le, 3)...)}))))
	add("count_3_offset", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 3, count: 3, value: append(append(u16(le, 6), u16(le, 3)...), u16(le, 3)...)}))))
	add("count_0", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 3, count: 0, value: u16(le, 6)}))))
	// A count over 0x10000 is skipped before its type is checked: an unknown type there is not
	// an error.
	add("count_huge_unknown_type", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x0110, typ: 99, count: 0x10001, value: u16(le, 6)}, orientationEntry(le, 3, 6)))))
	add("count_huge", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 3, count: 0x10001, value: u16(le, 6)}, orientationEntry(le, 3, 3)))))
	// Order and neighbours.
	add("first_wins", "jpeg", withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 3, 6), orientationEntry(le, 3, 8)))))
	add("long_then_short", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x112, typ: 4, count: 1, value: u32(le, 6)}, orientationEntry(le, 3, 8)))))
	add("unknown_type_before", "jpeg", withJPEGSegments(exifAPP1(simple(true, ifdEntry{tag: 0x010f, typ: 99, count: 1, value: []byte{1, 2, 3, 4}}, orientationEntry(le, 3, 6)))))
	add("unknown_type_after", "jpeg", withJPEGSegments(exifAPP1(simple(true, orientationEntry(le, 3, 6), ifdEntry{tag: 0x010f, typ: 99, count: 1, value: []byte{1, 2, 3, 4}}))))
	add("other_tags_before", "jpeg", withJPEGSegments(exifAPP1(simple(true,
		ifdEntry{tag: 0x010f, typ: 2, count: 6, value: []byte("Canon\x00")},
		ifdEntry{tag: 0x0110, typ: 2, count: 3, value: []byte("X1\x00\x00")},
		ifdEntry{tag: 0x011a, typ: 5, count: 1, value: append(u32(le, 72), u32(le, 1)...)},
		orientationEntry(le, 3, 5)))))
	add("big_tag_before", "jpeg", withJPEGSegments(exifAPP1(simple(true,
		ifdEntry{tag: 0x9286, typ: 7, count: 12000, value: make([]byte, 12000)},
		orientationEntry(le, 3, 7)))))
	// IFD1, sub-IFDs, pointer shapes.
	add("ifd1_only", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, chain: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x010f, typ: 2, count: 4, value: []byte("abc\x00")}},
		{orientationEntry(le, 3, 6)},
	}}.build())))
	add("ifd0_and_ifd1", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, chain: true, ifds: [][]ifdEntry{
		{orientationEntry(le, 3, 3)},
		{orientationEntry(le, 3, 6)},
	}}.build())))
	add("exif_subifd", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: false, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x8769, typ: 4, count: 1, subIFD: 1}},
		{ifdEntry{tag: 0x9000, typ: 7, count: 4, value: []byte("0231")}, orientationEntry(be, 3, 8)},
	}}.build())))
	add("gps_subifd", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x8825, typ: 4, count: 1, subIFD: 1}},
		{orientationEntry(le, 3, 2)},
	}}.build())))
	add("subifd_self_loop", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x8769, typ: 4, count: 1, subIFD: 1}},
		{ifdEntry{tag: 0x8769, typ: 4, count: 1, subIFD: 1}, orientationEntry(le, 3, 4)},
	}}.build())))
	add("subifd_pointer_short", "jpeg", withJPEGSegments(exifAPP1(simple(true,
		ifdEntry{tag: 0x8769, typ: 3, count: 1, value: u16(le, 8)}, orientationEntry(le, 3, 6)))))
	add("subifd_pointer_past_end", "jpeg", withJPEGSegments(exifAPP1(simple(true,
		ifdEntry{tag: 0x8769, typ: 4, count: 1, value: u32(le, 5000)}, orientationEntry(le, 3, 6)))))
	add("subifd_array", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x014a, typ: 4, count: 2, value: append(u32(le, 1), u32(le, 2)...)}},
		{orientationEntry(le, 3, 6)},
	}}.build())))
	// Header damage.
	off := func(v uint32) *uint32 { return &v }
	add("ifd0_offset_4", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, ifd0Offset: off(4), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())))
	add("ifd0_offset_past_end", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, ifd0Offset: off(900), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())))
	add("ifd0_offset_far_past_end", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, ifd0Offset: off(20000000), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())))
	add("bad_byte_order", "jpeg", withJPEGSegments(exifAPP1(tiffSpec{le: true, order: "XX", ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())))
	add("value_offset_past_end", "jpeg", withJPEGSegments(exifAPP1(simple(true,
		ifdEntry{tag: 0x112, typ: 3, count: 3, rawOff: off(9000)}, orientationEntry(le, 3, 6)))))
	add("value_offset_past_end_then_nothing", "jpeg", withJPEGSegments(exifAPP1(simple(true,
		ifdEntry{tag: 0x112, typ: 3, count: 3, rawOff: off(9000)}))))
	// JPEG segment order.
	jfif := jpegSegment(0xe0, []byte("JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00"))
	xmp := jpegSegment(0xe1, []byte("http://ns.adobe.com/xap/1.0/\x00<x:xmpmeta/>"))
	good := exifAPP1(simple(true, orientationEntry(le, 3, 6)))
	add("after_jfif", "jpeg", withJPEGSegments(jfif, good))
	add("after_xmp", "jpeg", withJPEGSegments(xmp, good))
	add("before_xmp", "jpeg", withJPEGSegments(good, xmp))
	add("two_exif", "jpeg", withJPEGSegments(good, exifAPP1(simple(true, orientationEntry(le, 3, 8)))))
	add("not_exif_header", "jpeg", withJPEGSegments(jpegSegment(0xe1, append([]byte("Exix\x00\x00"), simple(true, orientationEntry(le, 3, 6))...)), good))
	add("app1_len_1", "jpeg", append([]byte{0xff, 0xd8, 0xff, 0xe1, 0x00, 0x01}, exifBaseJPEG[2:]...))
	add("app1_len_0", "jpeg", append([]byte{0xff, 0xd8, 0xff, 0xe1, 0x00, 0x00}, exifBaseJPEG[2:]...))
	add("app1_truncated_file", "jpeg", withJPEGSegments(good)[:30])
	add("app1_overlong", "jpeg", func() []byte {
		b := withJPEGSegments(good)
		binary.BigEndian.PutUint16(b[4:], 0xfff0)
		return b
	}())
	add("no_soi", "jpeg", good)
	add("plain_jpeg", "jpeg", exifBaseJPEG)
	add("zero_padding_markers", "jpeg", append(append([]byte{0xff, 0xd8, 0x00, 0x00, 0x00, 0x00}, good...), exifBaseJPEG[2:]...))
	add("empty", "jpeg", nil)
	add("two_bytes", "jpeg", []byte{0xff, 0xd8})
	// Many tags: the walk stops after 5000 handled tags.
	{
		var many []ifdEntry
		for i := 0; i < 5000; i++ {
			many = append(many, ifdEntry{tag: 0x010e, typ: 3, count: 1, value: u16(le, 1)})
		}
		many = append(many, orientationEntry(le, 3, 6))
		add("after_5000_tags", "jpeg", withJPEGSegments(exifAPP1(simple(true, many...))))
		add("after_4999_tags", "jpeg", withJPEGSegments(exifAPP1(simple(true, many[1:]...))))
	}
	// PNG chunk shapes.
	pngGood := pngChunkBytes("eXIf", simple(false, orientationEntry(be, 3, 6)))
	add("png_plain", "png", exifBasePNG)
	add("png_after_idat", "png", append(append(append([]byte{}, exifBasePNG[:len(exifBasePNG)-12]...), pngGood...), exifBasePNG[len(exifBasePNG)-12:]...))
	add("png_after_text", "png", withPNGChunks(pngChunkBytes("tEXt", []byte("Comment\x00hello")), pngGood))
	add("png_after_ztxt", "png", withPNGChunks(pngChunkBytes("zTXt", append([]byte("Comment\x00\x00"), zlibOf([]byte("hello"))...)), pngGood))
	add("png_after_ztxt_raw_exif", "png", withPNGChunks(pngChunkBytes("zTXt", append([]byte("Raw profile type exif\x00\x00"), zlibOf([]byte("x"))...)), pngGood))
	add("png_two_exif", "png", withPNGChunks(pngGood, pngChunkBytes("eXIf", simple(false, orientationEntry(be, 3, 8)))))
	add("png_exif_with_header", "png", withPNGChunks(pngChunkBytes("eXIf", append([]byte("Exif\x00\x00"), simple(false, orientationEntry(be, 3, 6))...))))
	add("png_exif_le", "png", withPNGChunks(pngChunkBytes("eXIf", simple(true, orientationEntry(le, 3, 7)))))
	add("png_exif_truncated", "png", withPNGChunks(pngChunkBytes("eXIf", simple(false, orientationEntry(be, 3, 6))[:12])))
	add("png_exif_len_past_end", "png", func() []byte {
		b := withPNGChunks(pngGood)
		binary.BigEndian.PutUint32(b[33:], 0x7fffff)
		return b
	}())
	// Where the two reader shapes part: an eXIf behind a chunk that ends past bufReadSeeker's
	// 10 MiB scan limit.
	mib := 10 * 1024 * 1024
	for _, pad := range []int{1000, mib - 100, mib + 100, 2 * mib} {
		small := withPNGChunks(pngGood)
		big := withPNGChunks(pngChunkBytes("abCD", make([]byte, pad)), pngGood)
		cases = append(cases, exifCase{
			Name: "png_padded_" + itoa(pad), Format: "png", B64: b64(small), Pad: pad,
			Seeker: orientationOf(bytes.NewReader(big), "png"),
			Stream: orientationOf(io.MultiReader(bytes.NewReader(big)), "png"),
		})
	}
	// Minimal JPEGs: SOI, one APP1 Exif segment around a little-endian TIFF, then SOS.
	tiffLE := func(entries ...[4]any) []byte {
		t := []byte("II\x2a\x00\x08\x00\x00\x00")
		t = binary.LittleEndian.AppendUint16(t, uint16(len(entries)))
		for _, e := range entries {
			t = binary.LittleEndian.AppendUint16(t, e[0].(uint16))
			t = binary.LittleEndian.AppendUint16(t, e[1].(uint16))
			t = binary.LittleEndian.AppendUint32(t, e[2].(uint32))
			v := e[3].([4]byte)
			t = append(t, v[:]...)
		}
		return append(t, 0, 0, 0, 0)
	}
	jpegWith := func(tiff []byte) []byte {
		j := []byte{0xff, 0xd8, 0xff, 0xe1}
		j = binary.BigEndian.AppendUint16(j, uint16(len(tiff)+8))
		j = append(j, "Exif\x00\x00"...)
		j = append(j, tiff...)
		return append(j, 0xff, 0xda)
	}
	e := func(tag, typ uint16, count uint32, v [4]byte) [4]any { return [4]any{tag, typ, count, v} }
	{
		// A tag over LimitTagSize is skipped before it is counted, so 5000 counted tags still fit.
		entries := [][4]any{e(0x9286, 7, 10001, [4]byte{})}
		for i := 0; i < 4999; i++ {
			entries = append(entries, e(0x010e, 3, 1, [4]byte{1}))
		}
		entries = append(entries, e(0x0112, 3, 1, [4]byte{6}))
		add("oversize_tag_not_counted", "jpeg", jpegWith(tiffLE(entries...)))
	}
	// XMP (0x02bc) and IPTC (0x83bb) are skipped whatever their size, but after the type check.
	add("xmp_iptc_tags_skipped", "jpeg", jpegWith(tiffLE(e(0x02bc, 7, 1, [4]byte{}), e(0x83bb, 7, 1, [4]byte{}), e(0x0112, 3, 1, [4]byte{3}))))
	add("xmp_tag_unknown_type", "jpeg", jpegWith(tiffLE(e(0x02bc, 99, 1, [4]byte{}), e(0x0112, 3, 1, [4]byte{3}))))
	// A SubIFD pointer array of SHORTs is a []any with no uint32 in it: nothing followed.
	add("subifd_short_array", "jpeg", jpegWith(tiffLE(e(0x014a, 3, 2, [4]byte{1, 0, 2, 0}), e(0x0112, 3, 1, [4]byte{8}))))
	// The one silent EOF hands back the previous read's bytes: a value cut off after the count
	// reads the count's last two bytes.
	truncated := []byte("MM\x00\x2a\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01")
	add("truncated_value_count_1", "jpeg", jpegWith(truncated))
	truncated2 := bytes.Clone(truncated)
	truncated2[17] = 2
	add("truncated_value_count_2", "jpeg", jpegWith(truncated2))
	add("png_truncated", "png", exifBasePNG[:20])
	add("png_empty", "png", nil)

	// ---- TIFF ---------------------------------------------------------------------------
	//
	// A TIFF *is* the EXIF structure, and imagedecoder_tif.go hands the EXIF walk the file's own
	// streamReader rather than an in-memory copy. Three consequences the corpus pins:
	//
	//   - a header the walk dislikes is errInvalidFormat here (a bad byte-order marker, a magic
	//     that is not 42, an IFD offset below 8), where the same shapes inside a JPEG APP1 are
	//     simply "no orientation";
	//   - every value offset and sub-IFD pointer is a seek on the *input*, so the seekable and
	//     the stream shapes can disagree where they never do on a JPEG;
	//   - decodeTags is called directly, so the next-IFD pointer is never read and an
	//     orientation that lives only in IFD1 is invisible.
	off16 := func(v uint16) *uint16 { return &v }
	for o := uint16(0); o <= 9; o++ {
		add("tiff_le_"+itoa(int(o)), "tiff", simple(true, orientationEntry(le, 3, o)))
		add("tiff_be_"+itoa(int(o)), "tiff", simple(false, orientationEntry(be, 3, o)))
	}
	add("tiff_mime_format", "image/tiff", simple(true, orientationEntry(le, 3, 6)))
	add("tiff_type_sshort", "tiff", simple(true, orientationEntry(le, 8, 5)))
	add("tiff_type_long", "tiff", simple(true, ifdEntry{tag: 0x112, typ: 4, count: 1, value: u32(le, 6)}))
	add("tiff_type_byte", "tiff", simple(true, ifdEntry{tag: 0x112, typ: 1, count: 1, value: []byte{6, 0, 0, 0}}))
	add("tiff_type_unknown_before", "tiff", simple(true,
		ifdEntry{tag: 0x010f, typ: 99, count: 1, value: []byte{1, 2, 3, 4}}, orientationEntry(le, 3, 6)))
	add("tiff_count_2", "tiff", simple(true, ifdEntry{tag: 0x112, typ: 3, count: 2, value: append(u16(le, 6), u16(le, 3)...)}))
	add("tiff_after_config_tags", "tiff", simple(true,
		ifdEntry{tag: 0x0100, typ: 4, count: 1, value: u32(le, 100)},
		ifdEntry{tag: 0x0101, typ: 4, count: 1, value: u32(le, 50)},
		orientationEntry(le, 3, 7)))
	// A value wider than the four inline bytes is fetched by seeking the file and seeking back.
	add("tiff_value_at_offset", "tiff", simple(true,
		ifdEntry{tag: 0x0131, typ: 2, count: 20, value: []byte("mattermost-rs oracle")},
		orientationEntry(le, 3, 3)))
	add("tiff_value_offset_past_end", "tiff", simple(true,
		ifdEntry{tag: 0x0131, typ: 2, count: 20, rawOff: off(9000)},
		orientationEntry(le, 3, 3)))
	add("tiff_value_offset_far_past_end", "tiff", simple(true,
		ifdEntry{tag: 0x0131, typ: 2, count: 20, rawOff: off(20000000)},
		orientationEntry(le, 3, 3)))
	// IFD0 where the header says, rather than at 8; and past the 4 KiB bufio buffer.
	add("tiff_ifd0_at_64", "tiff", tiffSpec{le: true, pad: 56, ifds: [][]ifdEntry{{orientationEntry(le, 3, 4)}}}.build())
	add("tiff_ifd0_past_4k", "tiff", tiffSpec{le: true, pad: 5000, ifds: [][]ifdEntry{{orientationEntry(le, 3, 2)}}}.build())
	add("tiff_ifd0_offset_0", "tiff", tiffSpec{le: true, ifd0Offset: off(0), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_ifd0_offset_7", "tiff", tiffSpec{le: true, ifd0Offset: off(7), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_ifd0_offset_9", "tiff", tiffSpec{le: true, ifd0Offset: off(9), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_ifd0_offset_past_end", "tiff", tiffSpec{le: true, ifd0Offset: off(900), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_ifd0_offset_far_past_end", "tiff", tiffSpec{le: true, ifd0Offset: off(20000000), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	// Sub-IFDs are followed; IFD1 is not, because decodeTags never reads the next-IFD pointer.
	add("tiff_exif_subifd", "tiff", tiffSpec{le: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x8769, typ: 4, count: 1, subIFD: 1}},
		{orientationEntry(le, 3, 8)},
	}}.build())
	add("tiff_gps_subifd_be", "tiff", tiffSpec{le: false, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x8825, typ: 4, count: 1, subIFD: 1}},
		{orientationEntry(be, 3, 5)},
	}}.build())
	add("tiff_subifd_past_end", "tiff", simple(true,
		ifdEntry{tag: 0x8769, typ: 4, count: 1, value: u32(le, 9000)}, orientationEntry(le, 3, 6)))
	// The only tag shape whose value is wide enough to be fetched from an offset *and* still
	// handled: a SubIFD pointer array. Two LONGs are 8 bytes, so the walk seeks the file, reads
	// them, seeks back, and then follows them. IFD0 is 18 bytes plus its 8-byte out-of-line
	// value, so IFD1 starts at 8+26.
	subArrayOff := u32(le, 8+26)
	add("tiff_subifd_array_at_offset", "tiff", tiffSpec{le: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x014a, typ: 4, count: 2, value: append(subArrayOff, subArrayOff...)}},
		{orientationEntry(le, 3, 6)},
	}}.build())
	add("tiff_subifd_array_offset_past_end", "tiff", simple(true,
		ifdEntry{tag: 0x014a, typ: 4, count: 2, rawOff: off(9000)}, orientationEntry(le, 3, 7)))
	add("tiff_subifd_array_offset_past_scan_limit", "tiff", simple(true,
		ifdEntry{tag: 0x014a, typ: 4, count: 2, rawOff: off(20000000)}, orientationEntry(le, 3, 7)))
	add("tiff_ifd1_only", "tiff", tiffSpec{le: true, chain: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x010f, typ: 2, count: 4, value: []byte("abc\x00")}},
		{orientationEntry(le, 3, 6)},
	}}.build())
	add("tiff_ifd0_and_ifd1", "tiff", tiffSpec{le: true, chain: true, ifds: [][]ifdEntry{
		{orientationEntry(le, 3, 3)},
		{orientationEntry(le, 3, 6)},
	}}.build())
	// Header damage.
	add("tiff_bad_magic", "tiff", tiffSpec{le: true, magic: off16(43), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_magic_wrong_order", "tiff", tiffSpec{le: true, magic: off16(0x2a00), ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_bad_byte_order", "tiff", tiffSpec{le: true, order: "XX", ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_byte_order_mm_body_le", "tiff", tiffSpec{le: true, order: "MM", ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build())
	add("tiff_num_tags_overrun", "tiff", tiffSpec{le: true, numTags: off16(100), ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x010f, typ: 2, count: 4, value: []byte("abc\x00")}},
	}}.build())
	add("tiff_num_tags_overrun_found_first", "tiff", tiffSpec{le: true, numTags: off16(100), ifds: [][]ifdEntry{
		{orientationEntry(le, 3, 9)},
	}}.build())
	add("tiff_num_tags_zero", "tiff", tiffSpec{le: true, numTags: off16(0), ifds: [][]ifdEntry{
		{orientationEntry(le, 3, 6)},
	}}.build())
	{
		// Truncations: at the byte-order marker, inside the magic, inside the IFD offset, at the
		// IFD start, inside the tag count, inside the entry, and one byte short of the value.
		full := simple(true, ifdEntry{tag: 0x0131, typ: 2, count: 20, value: []byte("mattermost-rs oracle")}, orientationEntry(le, 3, 3))
		for _, n := range []int{1, 2, 3, 4, 6, 8, 9, 10, 14, 20, 26, 33, len(full) - 1} {
			add("tiff_truncated_"+itoa(n), "tiff", full[:n])
		}
	}
	// Where the two reader shapes part on a TIFF: IFD0 behind a hole whose far side is past
	// `bufReadSeeker`'s 10 MiB scan limit. A `bytes.Reader` seeks there; the stream wrapper
	// refuses, `skip` swallows the refusal, and the walk reads on from where it stood.
	for _, pad := range []int{1000, mib - 100, mib + 100, 2 * mib} {
		small := tiffSpec{le: true, ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build()
		big := tiffSpec{le: true, pad: pad, ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}}}.build()
		cases = append(cases, exifCase{
			Name: "tiff_padded_" + itoa(pad), Format: "tiff", B64: b64(small), TiffPad: pad,
			Seeker: orientationOf(bytes.NewReader(big), "tiff"),
			Stream: orientationOf(io.MultiReader(bytes.NewReader(big)), "tiff"),
		})
	}
	// `skip` is a *relative* seek whose error is discarded, and both halves of that matter here.
	// The header says IFD0 is at 20000000, which `bufReadSeeker` refuses (past its 10 MiB scan
	// limit); the refusal is swallowed, the 4 KiB bufio buffer is dropped, and the walk reads on
	// from the underlying reader's position — 4096, where a real IFD is planted. So the stream
	// shape finds 6 and the seekable shape, which seeks to 20000000 and reads nothing, finds
	// none. An absolute `seek` in place of the relative `skip` would answer 1 both ways.
	add("tiff_ifd0_offset_refused_lands_at_4096", "tiff", tiffSpec{
		le: true, pad: 4096 - 8, ifd0Offset: off(20000000),
		ifds: [][]ifdEntry{{orientationEntry(le, 3, 6)}},
	}.build())
	add("tiff_ifd0_offset_4096", "tiff", tiffSpec{
		le: true, pad: 4096 - 8, ifds: [][]ifdEntry{{orientationEntry(le, 3, 8)}},
	}.build())
	add("tiff_empty", "tiff", nil)
	// The wrong arm: a TIFF read as the other three formats, and a JPEG read as a TIFF
	// (format_tiff, above).
	tiffGood := simple(true, orientationEntry(le, 3, 6))
	add("tiff_as_jpeg", "jpeg", tiffGood)
	add("tiff_as_png", "png", tiffGood)
	add("tiff_as_webp", "webp", tiffGood)

	// ---- WebP ---------------------------------------------------------------------------
	//
	// imagemeta walks the RIFF chunk list and decodes the EXIF chunk's payload as a bare TIFF
	// structure in its own in-memory reader — but it skips exactly chunkLen and never the RIFF
	// pad byte, so a spec-conforming odd-length chunk misaligns every chunk id after it. A VP8X
	// header chunk is read whatever the requested sources, and its EXIF flag can end the walk
	// before an EXIF chunk is reached.
	//
	// The image chunk of the structural cases is a stub with an **even** payload length: the walk
	// only skips it, so its bytes need not decode, and an even length keeps every chunk after it
	// aligned. The two real x/image files are kept as their own cases — and the lossless one has
	// a 421-byte payload, so its pad byte is exactly the misalignment this note is about.
	imgChunk := riffChunk("VP8L", []byte{0x2f, 0x00, 0x00, 0x00, 0x00, 0x00}, false)
	realVP8L := webpImageChunk("gopher-doc.1bpp.lossless.webp")
	realVP8 := webpImageChunk("blue-purple-pink.lossy.webp")
	exifPayload := func(isLE bool, o uint16) []byte {
		if isLE {
			return simple(true, orientationEntry(le, 3, o))
		}
		return simple(false, orientationEntry(be, 3, o))
	}
	exifChunk := func(o uint16) []byte { return riffChunk("EXIF", exifPayload(true, o), true) }
	const vp8xEXIF, vp8xXMP = 0x08, 0x04
	add("webp_vp8x_exif_after_image", "webp", riffFile("WEBP", vp8xChunk(vp8xEXIF, 32, 32), imgChunk, exifChunk(6)))
	add("webp_vp8x_exif_before_image", "webp", riffFile("WEBP", vp8xChunk(vp8xEXIF, 32, 32), exifChunk(2), imgChunk))
	add("webp_vp8x_no_exif_flag", "webp", riffFile("WEBP", vp8xChunk(0, 32, 32), imgChunk, exifChunk(6)))
	add("webp_vp8x_xmp_flag_only", "webp", riffFile("WEBP", vp8xChunk(vp8xXMP, 32, 32), imgChunk, exifChunk(6)))
	add("webp_vp8x_both_flags", "webp", riffFile("WEBP", vp8xChunk(vp8xEXIF|vp8xXMP, 32, 32), imgChunk, exifChunk(3)))
	add("webp_vp8x_all_flags", "webp", riffFile("WEBP", vp8xChunk(0xff, 32, 32), imgChunk, exifChunk(4)))
	add("webp_vp8x_bad_len", "webp", riffFile("WEBP", riffChunkLen(vp8xChunk(vp8xEXIF, 32, 32), 11), imgChunk, exifChunk(6)))
	add("webp_vp8x_len_9", "webp", riffFile("WEBP", riffChunkLen(vp8xChunk(vp8xEXIF, 32, 32), 9), imgChunk, exifChunk(6)))
	// Simple format: no VP8X, so nothing can clear the EXIF source.
	add("webp_simple_exif_after_image", "webp", riffFile("WEBP", imgChunk, exifChunk(8)))
	add("webp_simple_exif_before_image", "webp", riffFile("WEBP", exifChunk(5), imgChunk))
	add("webp_simple_exif_be", "webp", riffFile("WEBP", imgChunk, riffChunk("EXIF", exifPayload(false, 7), true)))
	// Two real x/image payloads. The lossy one's chunk is 2430 bytes — even, so the EXIF chunk
	// behind it is found; the lossless one's is 421, and the pad byte the file must carry is the
	// byte imagemeta does not skip, so the same EXIF chunk is lost.
	add("webp_real_lossy_then_exif", "webp", riffFile("WEBP", realVP8, exifChunk(7)))
	add("webp_real_lossless_then_exif", "webp", riffFile("WEBP", realVP8L, exifChunk(7)))
	add("webp_two_exif", "webp", riffFile("WEBP", imgChunk, exifChunk(4), exifChunk(6)))
	add("webp_mime_format", "image/webp", riffFile("WEBP", imgChunk, exifChunk(6)))
	// The pad byte imagemeta does not skip: the same odd chunk, padded and not.
	oddPayload := []byte("odd-iccp")[:5]
	add("webp_odd_chunk_padded_before_exif", "webp", riffFile("WEBP", riffChunk("ICCP", oddPayload, true), imgChunk, exifChunk(6)))
	add("webp_odd_chunk_unpadded_before_exif", "webp", riffFile("WEBP", riffChunk("ICCP", oddPayload, false), imgChunk, exifChunk(6)))
	add("webp_odd_exif_then_xmp", "webp", riffFile("WEBP", imgChunk,
		riffChunk("EXIF", append(exifPayload(true, 3), 0xff), true), riffChunk("XMP ", []byte("<x/>"), true)))
	// Chunk length damage.
	add("webp_exif_len_overrun", "webp", riffFile("WEBP", imgChunk, riffChunkLen(exifChunk(6), 60000)))
	add("webp_exif_len_huge", "webp", riffFile("WEBP", imgChunk, riffChunkLen(exifChunk(6), 20*1024*1024)))
	add("webp_exif_len_zero", "webp", riffFile("WEBP", imgChunk, riffChunkLen(exifChunk(6), 0)))
	add("webp_exif_len_short", "webp", riffFile("WEBP", imgChunk, riffChunkLen(exifChunk(6), 10)))
	// An "Exif\0\0" prefix is JPEG's alone: here it is read as the byte-order marker.
	add("webp_exif_with_jpeg_header", "webp", riffFile("WEBP", imgChunk,
		riffChunk("EXIF", append([]byte("Exif\x00\x00"), exifPayload(true, 6)...), true)))
	// Inside the segment the EXIF walk is the same one JPEG runs: sub-IFDs and IFD1 both count.
	add("webp_exif_subifd", "webp", riffFile("WEBP", imgChunk, riffChunk("EXIF", tiffSpec{le: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x8769, typ: 4, count: 1, subIFD: 1}},
		{orientationEntry(le, 3, 5)},
	}}.build(), true)))
	add("webp_exif_ifd1", "webp", riffFile("WEBP", imgChunk, riffChunk("EXIF", tiffSpec{le: true, chain: true, ifds: [][]ifdEntry{
		{ifdEntry{tag: 0x010f, typ: 2, count: 4, value: []byte("abc\x00")}},
		{orientationEntry(le, 3, 4)},
	}}.build(), true)))
	// Container damage. The RIFF size is skipped, never read, so a lying one changes nothing.
	add("webp_no_chunks", "webp", riffFile("WEBP"))
	add("webp_lying_riff_size", "webp", func() []byte {
		b := riffFile("WEBP", imgChunk, exifChunk(6))
		binary.LittleEndian.PutUint32(b[4:], 4)
		return b
	}())
	add("webp_riff_size_huge", "webp", func() []byte {
		b := riffFile("WEBP", imgChunk, exifChunk(6))
		binary.LittleEndian.PutUint32(b[4:], 0xffffffff)
		return b
	}())
	add("webp_bad_riff_fourcc", "webp", func() []byte {
		b := riffFile("WEBP", imgChunk, exifChunk(6))
		copy(b[0:4], "RIFX")
		return b
	}())
	add("webp_bad_form_fourcc", "webp", riffFile("WEBQ", imgChunk, exifChunk(6)))
	add("webp_lowercase_form", "webp", riffFile("webp", imgChunk, exifChunk(6)))
	{
		full := riffFile("WEBP", vp8xChunk(vp8xEXIF, 32, 32), exifChunk(2), imgChunk)
		for _, n := range []int{1, 4, 8, 11, 12, 16, 20, 24, 30, 36, 44} {
			add("webp_truncated_"+itoa(n), "webp", full[:n])
		}
	}
	add("webp_empty", "webp", nil)
	webpGood := riffFile("WEBP", imgChunk, exifChunk(6))
	add("webp_as_jpeg", "jpeg", webpGood)
	add("webp_as_png", "png", webpGood)
	add("webp_as_tiff", "tiff", webpGood)
	return map[string]any{"cases": cases}, nil
}
