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
// The corpus is hand-built TIFF structures inside JPEG APP1 and PNG eXIf, walking the branches
// that decide the number: tag type (SHORT and SSHORT are read, LONG is not), count, which IFD it
// sits in (IFD1 and sub-IFDs count, because Mattermost's ShouldHandleTag replaces imagemeta's
// IFD0-only default), what comes before it (an unknown type aborts the walk), pointer loops, and
// the JPEG segment order (an XMP APP1 uses the same marker as EXIF and consumes the EXIF source).

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
	pos := uint32(8)
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
	bo.PutUint16(out[2:4], 42)
	ifd0 := uint32(8)
	if t.ifd0Offset != nil {
		ifd0 = *t.ifd0Offset
	}
	bo.PutUint32(out[4:8], ifd0)
	for i, ifd := range t.ifds {
		p := offs[i]
		bo.PutUint16(out[p:], uint16(len(ifd)))
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

type exifCase struct {
	Name   string         `json:"name"`
	Format string         `json:"format"`
	B64    string         `json:"b64"`
	Seeker map[string]any `json:"seeker"`
	Stream map[string]any `json:"stream"`
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
	add("png_truncated", "png", exifBasePNG[:20])
	add("png_empty", "png", nil)
	return map[string]any{"cases": cases}, nil
}
