package main

// The TIFF stage of the imaging oracle. See behaviour_imaging.go for the generator and the
// conventions, and behaviour_imaging_codec.go for the `decodeCase`/`configOf`/`imageOf` shape
// every decode corpus uses.
//
// # Where the corpus comes from
//
// golang.org/x/image's TIFF decoder is reached through three quite different kinds of input, and
// the corpus carries all three:
//
//  1. The module's own pinned testdata — the video-001 family (LZW strips, a tiled file, 16-bit,
//     paletted, gray), the bw-* bilevel files (uncompressed, PackBits, Deflate, CCITT G3 and G4),
//     the tiled-*.tiff.bz2 set, and the degenerate 0x0/0x1/1x0 files.
//  2. Files this program encodes with `tiff.Encode`, which writes uncompressed and Deflate only.
//  3. Files this program builds byte by byte, which is the only way to reach most of the reader:
//     big-endian files, 1- and 16-bit samples, every photometric interpretation including the
//     rejected ones, LZW and PackBits (x/image has no encoder for either), the horizontal
//     predictor, tiles, a missing RowsPerStrip, unsorted tags, a short IFD, an IFD pointer past
//     the end of the file, and lengths that overflow.
//
// The LZW and PackBits encoders below are this file's own. The LZW one mirrors the *decoder's*
// `hi`/`overflow` bookkeeping exactly, including TIFF's off-by-one code widening, and every
// stream it produces is checked against `tiff/lzw`'s reader before it goes into the corpus — so
// a bug in the encoder is a panic here rather than a wrong fixture.

import (
	"bytes"
	"compress/bzip2"
	"compress/zlib"
	"encoding/binary"
	"fmt"
	"image"
	"image/color"
	"io"
	"sort"
	"strings"

	xlzw "golang.org/x/image/tiff/lzw"

	"golang.org/x/image/ccitt"
	"golang.org/x/image/tiff"
)

// --- a TIFF assembler -----------------------------------------------------------------------

const (
	ttByte     = 1
	ttASCII    = 2
	ttShort    = 3
	ttLong     = 4
	ttRational = 5
)

var ttLengths = [...]uint32{0, 1, 1, 2, 4, 8}

// ttLen is the byte length of one value of a type; the corpus writes types the spec does not
// define, so an unknown one is laid out as a single byte.
func ttLen(typ uint16) uint32 {
	if int(typ) < len(ttLengths) {
		return ttLengths[typ]
	}
	return 1
}

func appU16(o binary.ByteOrder, p []byte, v uint16) []byte {
	var b [2]byte
	o.PutUint16(b[:], v)
	return append(p, b[:]...)
}

func appU32(o binary.ByteOrder, p []byte, v uint32) []byte {
	var b [4]byte
	o.PutUint32(b[:], v)
	return append(p, b[:]...)
}

// tentry is one IFD entry. `count` and `word` override the count field and the final four bytes
// (the inline value or the pointer) when they are non-nil, which is how the corpus reaches the
// reader's overflow and out-of-range branches.
type tentry struct {
	tag   uint16
	typ   uint16
	vals  []uint32
	count *uint32
	word  *uint32
}

// tbuild lays a TIFF out the way x/image's own writer does: header, image data, IFD, then a
// pointer area for the entries too big to fit inline.
type tbuild struct {
	be       bool
	data     []byte
	entries  []tentry
	unsorted bool // emit entries in insertion order rather than by tag
	numItems *int // override the entry count written into the IFD
	ifdAt    *uint32
}

func (b *tbuild) order() binary.ByteOrder {
	if b.be {
		return binary.BigEndian
	}
	return binary.LittleEndian
}

// addData appends a block of image data and returns its file offset.
func (b *tbuild) addData(p []byte) uint32 {
	off := uint32(8 + len(b.data))
	b.data = append(b.data, p...)
	return off
}

func (b *tbuild) put(tag, typ uint16, vals ...uint32) {
	b.entries = append(b.entries, tentry{tag: tag, typ: typ, vals: vals})
}

func (b *tbuild) putRaw(e tentry) { b.entries = append(b.entries, e) }

func putVals(o binary.ByteOrder, typ uint16, vals []uint32) []byte {
	var p []byte
	for _, v := range vals {
		switch typ {
		case ttByte, ttASCII:
			p = append(p, byte(v))
		case ttShort:
			p = appU16(o, p, uint16(v))
		default:
			p = appU32(o, p, v)
		}
	}
	return p
}

func (b *tbuild) bytes() []byte {
	o := b.order()
	entries := append([]tentry(nil), b.entries...)
	if !b.unsorted {
		sort.SliceStable(entries, func(i, j int) bool { return entries[i].tag < entries[j].tag })
	}
	n := len(entries)
	if b.numItems != nil {
		n = *b.numItems
	}
	ifdOffset := uint32(8 + len(b.data))
	extBase := ifdOffset + uint32(2+12*len(entries)+4)

	var ifd, ext bytes.Buffer
	ifd.Write(appU16(o, nil, uint16(n)))
	for _, e := range entries {
		payload := putVals(o, e.typ, e.vals)
		count := uint32(len(e.vals))
		if e.count != nil {
			count = *e.count
		}
		var buf [12]byte
		o.PutUint16(buf[0:2], e.tag)
		o.PutUint16(buf[2:4], e.typ)
		o.PutUint32(buf[4:8], count)
		switch {
		case e.word != nil:
			o.PutUint32(buf[8:12], *e.word)
		case ttLen(e.typ)*count > 4:
			o.PutUint32(buf[8:12], extBase+uint32(ext.Len()))
			ext.Write(payload)
			if ext.Len()%2 == 1 {
				ext.WriteByte(0)
			}
		default:
			copy(buf[8:12], payload)
		}
		ifd.Write(buf[:])
	}
	ifd.Write(appU32(o, nil, 0)) // no next IFD

	var out bytes.Buffer
	if b.be {
		out.WriteString("MM\x00\x2a")
	} else {
		out.WriteString("II\x2a\x00")
	}
	at := ifdOffset
	if b.ifdAt != nil {
		at = *b.ifdAt
	}
	out.Write(appU32(o, nil, at))
	out.Write(b.data)
	out.Write(ifd.Bytes())
	out.Write(ext.Bytes())
	return out.Bytes()
}

func u32p(v uint32) *uint32 { return &v }
func intp(v int) *int       { return &v }

// --- packing pixels -------------------------------------------------------------------------

// sampleSrc gives sample k of the pixel at (x, y).
type sampleSrc func(x, y, k int) uint32

// noiseSrc is a deterministic source of bps-bit samples.
func noiseSrc(seed uint64, bps uint32) sampleSrc {
	maxv := uint32(1)<<bps - 1
	return func(x, y, k int) uint32 {
		return uint32(hash4(seed, x, y, k)>>40) & maxv
	}
}

// packRegion writes the tw×th region at (x0, y0) as TIFF rows: `samples` samples of `bps` bits
// per pixel, most significant bit first, each row padded out to a whole byte. Samples outside
// the w×h image read as zero, which is how an encoder pads the last tile.
func packRegion(o binary.ByteOrder, src sampleSrc, w, h, x0, y0, tw, th, samples int, bps uint32) []byte {
	var out []byte
	for y := y0; y < y0+th; y++ {
		var acc uint32
		var nbits uint32
		for x := x0; x < x0+tw; x++ {
			for k := 0; k < samples; k++ {
				var v uint32
				if x < w && y < h {
					v = src(x, y, k)
				}
				switch bps {
				case 8:
					out = append(out, byte(v))
				case 16:
					out = appU16(o, out, uint16(v))
				default:
					acc = acc<<bps | v
					nbits += bps
					for nbits >= 8 {
						nbits -= 8
						out = append(out, byte(acc>>nbits))
					}
				}
			}
		}
		if nbits > 0 {
			out = append(out, byte(acc<<(8-nbits)))
		}
	}
	return out
}

// --- LZW and PackBits encoders --------------------------------------------------------------

type msbBits struct {
	out []byte
	acc uint32
	n   uint
}

func (w *msbBits) put(v uint32, width uint) {
	w.acc = w.acc<<width | v
	w.n += width
	for w.n >= 8 {
		w.n -= 8
		w.out = append(w.out, byte(w.acc>>w.n))
	}
}

func (w *msbBits) done() []byte {
	if w.n > 0 {
		w.out = append(w.out, byte(w.acc<<(8-w.n)))
	}
	return w.out
}

type lzwKey struct {
	code uint16
	b    byte
}

// tiffLZW compresses src the way a TIFF writer must: MSB-first codes, a clear code first, and
// the code width widened one code earlier than real LZW does, mirroring the decoder's
// `hi+1 >= overflow`. When the table fills it emits another clear.
func tiffLZW(src []byte) []byte {
	const clearCode, eofCode = 256, 257
	w := &msbBits{}
	table := map[lzwKey]uint16{}
	width := uint(9)
	hi := uint16(257)
	overflow := uint16(512)

	w.put(clearCode, width)
	if len(src) == 0 {
		w.put(eofCode, width)
		return w.done()
	}

	// advance mirrors the decoder's per-code bookkeeping and reports whether the table is full.
	advance := func() (full bool) {
		hi++
		if hi+1 >= overflow {
			if width == 12 {
				return true
			}
			width++
			overflow <<= 1
		}
		return false
	}
	reset := func() {
		w.put(clearCode, width)
		table = map[lzwKey]uint16{}
		width = 9
		hi = 257
		overflow = 512
	}

	cur := uint16(src[0])
	for _, b := range src[1:] {
		if next, ok := table[lzwKey{cur, b}]; ok {
			cur = next
			continue
		}
		w.put(uint32(cur), width)
		if advance() {
			reset()
		} else {
			table[lzwKey{cur, b}] = hi
		}
		cur = uint16(b)
	}
	w.put(uint32(cur), width)
	if advance() {
		reset()
	}
	w.put(eofCode, width)
	return w.done()
}

// checkedLZW is tiffLZW with the real decoder as a witness.
func checkedLZW(src []byte) []byte {
	out := tiffLZW(src)
	r := xlzw.NewReader(bytes.NewReader(out), xlzw.MSB, 8)
	got, err := io.ReadAll(r)
	if err != nil {
		panic(fmt.Errorf("tiffLZW: round trip: %w", err))
	}
	if !bytes.Equal(got, src) {
		panic(fmt.Errorf("tiffLZW: round trip differs: %d vs %d bytes", len(got), len(src)))
	}
	return out
}

// packBits compresses src in the PackBits scheme of section 9 of the spec.
func packBits(src []byte) []byte {
	var out []byte
	for i := 0; i < len(src); {
		j := i + 1
		for j < len(src) && src[j] == src[i] && j-i < 128 {
			j++
		}
		if j-i >= 3 {
			out = append(out, byte(257-(j-i)), src[i])
			i = j
			continue
		}
		j = i
		for j < len(src) && j-i < 128 {
			if j+2 < len(src) && src[j] == src[j+1] && src[j+1] == src[j+2] {
				break
			}
			j++
		}
		if j == i {
			j = i + 1
		}
		out = append(out, byte(j-i-1))
		out = append(out, src[i:j]...)
		i = j
	}
	return out
}

func zlibBytes(raw []byte) []byte {
	var b bytes.Buffer
	w := zlib.NewWriter(&b)
	_, _ = w.Write(raw)
	_ = w.Close()
	return b.Bytes()
}

// compressBlock applies the TIFF compression `c` to one strip or tile.
func compressBlock(c uint32, raw []byte) []byte {
	switch c {
	case 0, 1:
		return raw
	case 5:
		return checkedLZW(raw)
	case 8, 32946:
		return zlibBytes(raw)
	case 32773:
		return packBits(raw)
	}
	panic(fmt.Sprintf("compressBlock: no encoder for compression %d", c))
}

// --- the image spec a crafted file is built from ----------------------------------------------

type tifSpec struct {
	name         string
	be           bool
	w, h         int
	bps          []uint32 // BitsPerSample; the first value is the decoder's d.bpp
	photometric  uint32
	omitPhoto    bool
	compression  uint32 // 0 omits the Compression tag entirely
	omitCompress bool
	extraSamples *uint32
	rowsPerStrip int // 0 omits the tag; the whole image is then one strip
	predictor    uint32
	colorMap     []uint32
	sampleFormat []uint32
	fillOrder    uint32
	tileW, tileH int
	seed         uint64
	// extra entries and post-hoc surgery.
	extra    []tentry
	mutate   func(*tbuild)
	truncate int
}

// build lays the spec out as a file, splitting the pixels into strips or tiles and compressing
// each block.
func (s tifSpec) build() []byte {
	b := &tbuild{be: s.be}
	o := b.order()
	samples := len(s.bps)
	bps := uint32(1)
	if samples > 0 {
		bps = s.bps[0]
	} else {
		// No BitsPerSample entry: the reader defaults to one bit and one sample, and the pixel
		// data has to be laid out that way or the case cannot tell that default from any other.
		samples = 1
	}
	src := noiseSrc(s.seed, bps)
	comp := s.compression
	if s.omitCompress {
		comp = 0
	}
	encode := func(raw []byte) []byte { return compressBlock(comp, raw) }

	var offsets, counts []uint32
	if s.tileW > 0 {
		across := (s.w + s.tileW - 1) / s.tileW
		down := (s.h + s.tileH - 1) / s.tileH
		for j := 0; j < down; j++ {
			for i := 0; i < across; i++ {
				raw := packRegion(o, src, s.w, s.h, i*s.tileW, j*s.tileH, s.tileW, s.tileH, samples, bps)
				blk := encode(raw)
				offsets = append(offsets, b.addData(blk))
				counts = append(counts, uint32(len(blk)))
			}
		}
		b.put(322, ttShort, uint32(s.tileW))
		b.put(323, ttShort, uint32(s.tileH))
		b.put(324, ttLong, offsets...)
		b.put(325, ttLong, counts...)
	} else {
		rps := s.rowsPerStrip
		if rps <= 0 || rps > s.h {
			rps = s.h
		}
		for y := 0; y < s.h; y += rps {
			th := rps
			if y+th > s.h {
				th = s.h - y
			}
			raw := packRegion(o, src, s.w, s.h, 0, y, s.w, th, samples, bps)
			blk := encode(raw)
			offsets = append(offsets, b.addData(blk))
			counts = append(counts, uint32(len(blk)))
		}
		if s.rowsPerStrip > 0 {
			b.put(278, ttShort, uint32(s.rowsPerStrip))
		}
		b.put(273, ttLong, offsets...)
		b.put(279, ttLong, counts...)
	}

	b.put(256, ttShort, uint32(s.w))
	b.put(257, ttShort, uint32(s.h))
	if len(s.bps) > 0 {
		b.put(258, ttShort, s.bps...)
	}
	if !s.omitCompress {
		b.put(259, ttShort, s.compression)
	}
	if !s.omitPhoto {
		b.put(262, ttShort, s.photometric)
	}
	if s.fillOrder != 0 {
		b.put(266, ttShort, s.fillOrder)
	}
	b.put(277, ttShort, uint32(samples))
	if s.predictor != 0 {
		b.put(317, ttShort, s.predictor)
	}
	if len(s.colorMap) > 0 {
		b.put(320, ttShort, s.colorMap...)
	}
	if s.extraSamples != nil {
		b.put(338, ttShort, *s.extraSamples)
	}
	if len(s.sampleFormat) > 0 {
		b.put(339, ttShort, s.sampleFormat...)
	}
	for _, e := range s.extra {
		b.putRaw(e)
	}
	if s.mutate != nil {
		s.mutate(b)
	}
	out := b.bytes()
	if s.truncate > 0 && s.truncate < len(out) {
		out = out[:s.truncate]
	}
	return out
}

// grayMap is a ColorMap of n entries: 16-bit RGB stored as n reds, then n greens, then n blues.
func grayMap(n int, seed uint64) []uint32 {
	m := make([]uint32, 3*n)
	for i := 0; i < n; i++ {
		m[i] = uint32(hash4(seed, i, 0, 0) >> 48)
		m[i+n] = uint32(hash4(seed, i, 0, 1) >> 48)
		m[i+2*n] = uint32(hash4(seed, i, 0, 2) >> 48)
	}
	return m
}

// --- the corpus -------------------------------------------------------------------------------

func craftedTIFFs() []namedFile {
	var files []namedFile
	add := func(s tifSpec) {
		files = append(files, namedFile{"crafted_" + s.name, s.build()})
	}
	raw := func(name string, data []byte) {
		files = append(files, namedFile{"crafted_" + name, data})
	}
	es := func(v uint32) *uint32 { return &v }
	seed := uint64(9100)
	next := func() uint64 { seed++; return seed }

	// --- byte order and the plain photometric interpretations ---
	for _, be := range []bool{false, true} {
		tag := "le"
		if be {
			tag = "be"
		}
		add(tifSpec{name: "gray8_black_" + tag, be: be, w: 7, h: 5, bps: []uint32{8}, photometric: 1, compression: 1, seed: next()})
		add(tifSpec{name: "gray8_white_" + tag, be: be, w: 7, h: 5, bps: []uint32{8}, photometric: 0, compression: 1, seed: next()})
		add(tifSpec{name: "gray16_black_" + tag, be: be, w: 5, h: 4, bps: []uint32{16}, photometric: 1, compression: 1, seed: next()})
		add(tifSpec{name: "gray16_white_" + tag, be: be, w: 5, h: 4, bps: []uint32{16}, photometric: 0, compression: 1, seed: next()})
		add(tifSpec{name: "bilevel_black_" + tag, be: be, w: 11, h: 3, bps: []uint32{1}, photometric: 1, compression: 1, seed: next()})
		add(tifSpec{name: "bilevel_white_" + tag, be: be, w: 11, h: 3, bps: []uint32{1}, photometric: 0, compression: 1, seed: next()})
		add(tifSpec{name: "rgb8_" + tag, be: be, w: 6, h: 4, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, seed: next()})
		add(tifSpec{name: "rgb16_" + tag, be: be, w: 4, h: 3, bps: []uint32{16, 16, 16}, photometric: 2, compression: 1, seed: next()})
		add(tifSpec{name: "rgba8_" + tag, be: be, w: 6, h: 4, bps: []uint32{8, 8, 8, 8}, photometric: 2, compression: 1, extraSamples: es(1), seed: next()})
		add(tifSpec{name: "nrgba8_" + tag, be: be, w: 6, h: 4, bps: []uint32{8, 8, 8, 8}, photometric: 2, compression: 1, extraSamples: es(2), seed: next()})
		add(tifSpec{name: "rgba16_" + tag, be: be, w: 4, h: 3, bps: []uint32{16, 16, 16, 16}, photometric: 2, compression: 1, extraSamples: es(1), seed: next()})
		add(tifSpec{name: "nrgba16_" + tag, be: be, w: 4, h: 3, bps: []uint32{16, 16, 16, 16}, photometric: 2, compression: 1, extraSamples: es(2), seed: next()})
		add(tifSpec{name: "paletted8_" + tag, be: be, w: 6, h: 4, bps: []uint32{8}, photometric: 3, compression: 1, colorMap: grayMap(256, 31), seed: next()})
	}

	// --- palettes ---
	add(tifSpec{name: "paletted1", w: 9, h: 3, bps: []uint32{1}, photometric: 3, compression: 1, colorMap: grayMap(2, 32), seed: next()})
	add(tifSpec{name: "paletted16", w: 4, h: 3, bps: []uint32{16}, photometric: 3, compression: 1, colorMap: grayMap(256, 33), seed: next()})
	add(tifSpec{name: "paletted8_no_colormap", w: 4, h: 3, bps: []uint32{8}, photometric: 3, compression: 1, seed: next()})
	add(tifSpec{name: "paletted8_short_colormap", w: 4, h: 3, bps: []uint32{8}, photometric: 3, compression: 1, colorMap: grayMap(4, 34), seed: next()})
	add(tifSpec{name: "paletted1_two_colours", w: 4, h: 2, bps: []uint32{1}, photometric: 3, compression: 1, colorMap: grayMap(2, 35), seed: next()})
	// A ColorMap of exactly n entries with the largest index in use at n-1 and, next door, at n:
	// the pair that separates `idx >= len(palette)` from `idx > len(palette)`.
	for _, top := range []int{3, 4} {
		b := &tbuild{}
		pix := []byte{0, 1, 2, 3, byte(top)}
		off := b.addData(pix)
		b.put(256, ttShort, uint32(len(pix)))
		b.put(257, ttShort, 1)
		b.put(258, ttShort, 8)
		b.put(259, ttShort, 1)
		b.put(262, ttShort, 3)
		b.put(273, ttLong, off)
		b.put(277, ttShort, 1)
		b.put(279, ttLong, uint32(len(pix)))
		b.put(320, ttShort, grayMap(4, 37)...)
		raw(fmt.Sprintf("paletted_top_index_%d_of_4", top), b.bytes())
	}
	add(tifSpec{name: "colormap_bad_length", w: 4, h: 3, bps: []uint32{8}, photometric: 3, compression: 1, colorMap: make([]uint32, 4), seed: next()})
	add(tifSpec{name: "colormap_too_long", w: 4, h: 3, bps: []uint32{8}, photometric: 3, compression: 1, colorMap: make([]uint32, 3*257), seed: next()})

	// --- rejected photometric interpretations and sample counts ---
	for _, p := range []uint32{4, 5, 6, 7, 8, 9} {
		add(tifSpec{name: fmt.Sprintf("photometric_%d", p), w: 4, h: 3, bps: []uint32{8}, photometric: p, compression: 1, seed: next()})
	}
	add(tifSpec{name: "rgb_two_samples", w: 4, h: 3, bps: []uint32{8, 8}, photometric: 2, compression: 1, seed: next()})
	add(tifSpec{name: "rgb_four_samples_no_extra", w: 4, h: 3, bps: []uint32{8, 8, 8, 8}, photometric: 2, compression: 1, seed: next()})
	add(tifSpec{name: "rgb_four_samples_extra_3", w: 4, h: 3, bps: []uint32{8, 8, 8, 8}, photometric: 2, compression: 1, extraSamples: es(3), seed: next()})
	add(tifSpec{name: "rgb_five_samples", w: 4, h: 3, bps: []uint32{8, 8, 8, 8, 8}, photometric: 2, compression: 1, extraSamples: es(1), seed: next()})
	add(tifSpec{name: "rgb8_mixed_depths", w: 4, h: 3, bps: []uint32{8, 8, 16}, photometric: 2, compression: 1, seed: next()})
	add(tifSpec{name: "rgb16_mixed_depths", w: 4, h: 3, bps: []uint32{16, 16, 8}, photometric: 2, compression: 1, seed: next()})
	add(tifSpec{name: "gray_two_samples", w: 4, h: 3, bps: []uint32{8, 8}, photometric: 1, compression: 1, seed: next()})
	add(tifSpec{name: "paletted_two_samples", w: 4, h: 3, bps: []uint32{8, 8}, photometric: 3, compression: 1, colorMap: grayMap(256, 36), seed: next()})
	// BitsPerSample with more than the sixteen values parseIFD keeps.
	{
		bps := make([]uint32, 20)
		for i := range bps {
			bps[i] = 8
		}
		add(tifSpec{name: "bps_twenty_values", w: 4, h: 3, bps: bps, photometric: 2, compression: 1, seed: next()})
	}

	// --- bit depths ---
	for _, d := range []uint32{2, 4, 12, 32} {
		add(tifSpec{name: fmt.Sprintf("bps_%d", d), w: 4, h: 3, bps: []uint32{d}, photometric: 1, compression: 1, seed: next()})
	}
	add(tifSpec{name: "bps_zero", w: 4, h: 3, bps: []uint32{0}, photometric: 1, compression: 1, seed: next()})
	add(tifSpec{name: "bps_missing", w: 9, h: 3, photometric: 1, compression: 1, seed: next()})

	// --- compression ---
	for _, c := range []uint32{1, 5, 8, 32946, 32773} {
		add(tifSpec{name: fmt.Sprintf("gray8_c%d", c), w: 9, h: 6, bps: []uint32{8}, photometric: 1, compression: c, seed: next()})
		add(tifSpec{name: fmt.Sprintf("rgb8_c%d", c), w: 5, h: 4, bps: []uint32{8, 8, 8}, photometric: 2, compression: c, seed: next()})
		add(tifSpec{name: fmt.Sprintf("bilevel_c%d", c), w: 17, h: 5, bps: []uint32{1}, photometric: 0, compression: c, seed: next()})
	}
	add(tifSpec{name: "gray8_no_compression_tag", w: 5, h: 4, bps: []uint32{8}, photometric: 1, omitCompress: true, seed: next()})
	// Unsupported and unknown compression values: the payload is the uncompressed data, which
	// the decoder never reaches.
	for _, c := range []uint32{2, 3, 4, 6, 7, 99, 65535} {
		s := tifSpec{name: fmt.Sprintf("compression_%d", c), w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: c, seed: next()}
		s.compression = 1
		data := s.build()
		// Rewrite the Compression tag's inline value in place.
		raw(fmt.Sprintf("compression_%d", c), withShortTag(data, 259, uint16(c)))
	}
	add(tifSpec{name: "g3_fill_order_2", w: 16, h: 4, bps: []uint32{1}, photometric: 0, compression: 1, fillOrder: 2, seed: next(),
		mutate: func(b *tbuild) { setShort(b, 259, 3) }})

	// --- strips ---
	add(tifSpec{name: "strips_rps1", w: 6, h: 5, bps: []uint32{8}, photometric: 1, compression: 1, rowsPerStrip: 1, seed: next()})
	add(tifSpec{name: "strips_rps2", w: 6, h: 5, bps: []uint32{8}, photometric: 1, compression: 1, rowsPerStrip: 2, seed: next()})
	add(tifSpec{name: "strips_rps2_deflate", w: 6, h: 5, bps: []uint32{8}, photometric: 1, compression: 8, rowsPerStrip: 2, seed: next()})
	add(tifSpec{name: "strips_rps2_lzw", w: 6, h: 5, bps: []uint32{8}, photometric: 1, compression: 5, rowsPerStrip: 2, seed: next()})
	add(tifSpec{name: "strips_rps_equal_h", w: 6, h: 5, bps: []uint32{8}, photometric: 1, compression: 1, rowsPerStrip: 5, seed: next()})
	add(tifSpec{name: "strips_rps_over_h", w: 6, h: 5, bps: []uint32{8}, photometric: 1, compression: 1, rowsPerStrip: 9, seed: next()})
	add(tifSpec{name: "strips_rps_bilevel", w: 13, h: 7, bps: []uint32{1}, photometric: 0, compression: 1, rowsPerStrip: 3, seed: next()})
	add(tifSpec{name: "strips_rps_rgb", w: 5, h: 6, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, rowsPerStrip: 4, seed: next()})
	// One strip too few, and byte counts missing altogether.
	add(tifSpec{name: "strips_missing_one", w: 6, h: 6, bps: []uint32{8}, photometric: 1, compression: 1, rowsPerStrip: 2, seed: next(),
		mutate: func(b *tbuild) { dropLast(b, 273) }})
	add(tifSpec{name: "strips_no_byte_counts", w: 6, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { drop(b, 279) }})
	add(tifSpec{name: "strips_no_offsets", w: 6, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { drop(b, 273) }})
	// A byte count larger than the block could ever hold, and an offset past the end.
	add(tifSpec{name: "strip_count_too_large", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 4*3*8+1) }})
	add(tifSpec{name: "strip_count_at_the_limit", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 4*3*8) }})
	add(tifSpec{name: "strip_offset_past_eof", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 273, 100000) }})
	add(tifSpec{name: "strip_short_data", w: 8, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 9) }})
	add(tifSpec{name: "strip_short_data_rgb", w: 8, h: 4, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 13) }})
	add(tifSpec{name: "strip_short_data_rgba", w: 8, h: 4, bps: []uint32{8, 8, 8, 8}, photometric: 2, compression: 1, extraSamples: es(2), seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 17) }})
	add(tifSpec{name: "strip_short_data_gray16", w: 8, h: 4, bps: []uint32{16}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 15) }})
	add(tifSpec{name: "strip_short_data_rgb16", w: 8, h: 4, bps: []uint32{16, 16, 16}, photometric: 2, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 25) }})

	// --- tiles ---
	add(tifSpec{name: "tiles_8x8_exact", w: 16, h: 16, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_8x8_padded", w: 20, h: 12, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_16x8_rgb", w: 20, h: 12, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, tileW: 16, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_8x8_bilevel", w: 20, h: 12, bps: []uint32{1}, photometric: 0, compression: 1, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_8x8_gray16", w: 12, h: 10, bps: []uint32{16}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_8x8_lzw", w: 20, h: 12, bps: []uint32{8}, photometric: 1, compression: 5, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_8x8_packbits", w: 20, h: 12, bps: []uint32{8}, photometric: 1, compression: 32773, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "tiles_within_padding", w: 12, h: 12, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 24, tileH: 24, seed: next()})
	add(tifSpec{name: "tiles_too_small", w: 16, h: 16, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next(),
		mutate: func(b *tbuild) { setShort(b, 322, 4) }})
	add(tifSpec{name: "tiles_no_length", w: 16, h: 16, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next(),
		mutate: func(b *tbuild) { drop(b, 323) }})
	add(tifSpec{name: "tiles_exceed_image", w: 16, h: 16, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next(),
		mutate: func(b *tbuild) { setShort(b, 322, 2048); setShort(b, 323, 2048) }})
	add(tifSpec{name: "tiles_missing_offset", w: 20, h: 12, bps: []uint32{8}, photometric: 1, compression: 1, tileW: 8, tileH: 8, seed: next(),
		mutate: func(b *tbuild) { dropLast(b, 324) }})

	// --- the horizontal predictor ---
	add(tifSpec{name: "predictor_gray8", w: 8, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, predictor: 2, seed: next()})
	add(tifSpec{name: "predictor_rgb8", w: 6, h: 4, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, predictor: 2, seed: next()})
	add(tifSpec{name: "predictor_rgba8_lzw", w: 6, h: 4, bps: []uint32{8, 8, 8, 8}, photometric: 2, compression: 5, extraSamples: es(2), predictor: 2, seed: next()})
	add(tifSpec{name: "predictor_gray16", w: 6, h: 4, bps: []uint32{16}, photometric: 1, compression: 1, predictor: 2, seed: next()})
	add(tifSpec{name: "predictor_rgb16", w: 5, h: 3, bps: []uint32{16, 16, 16}, photometric: 2, compression: 1, predictor: 2, seed: next()})
	add(tifSpec{name: "predictor_bilevel", w: 16, h: 4, bps: []uint32{1}, photometric: 0, compression: 1, predictor: 2, seed: next()})
	add(tifSpec{name: "predictor_none_value", w: 6, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, predictor: 1, seed: next()})
	add(tifSpec{name: "predictor_strips", w: 6, h: 6, bps: []uint32{8}, photometric: 1, compression: 1, predictor: 2, rowsPerStrip: 2, seed: next()})
	add(tifSpec{name: "predictor_tiles", w: 20, h: 12, bps: []uint32{8}, photometric: 1, compression: 1, predictor: 2, tileW: 8, tileH: 8, seed: next()})
	add(tifSpec{name: "predictor_short_strip", w: 8, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, predictor: 2, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 10) }})
	add(tifSpec{name: "predictor16_short_strip", w: 8, h: 4, bps: []uint32{16}, photometric: 1, compression: 1, predictor: 2, seed: next(),
		mutate: func(b *tbuild) { setLong(b, 279, 20) }})
	// Two strips pointing at the same bytes: `buffer.Slice` hands out a window on the file, so
	// the predictor's accumulation into strip 0 is what strip 1 then reads.
	add(tifSpec{name: "predictor_overlapping_strips", w: 6, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, predictor: 2, rowsPerStrip: 2, seed: next(),
		mutate: func(b *tbuild) { setLongAt(b, 273, 1, 8) }})

	// --- IFD structure ---
	add(tifSpec{name: "unknown_tags", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		extra: []tentry{{tag: 254, typ: ttLong, vals: []uint32{0}}, {tag: 700, typ: ttByte, vals: []uint32{1, 2, 3}}, {tag: 65000, typ: ttLong, vals: []uint32{7, 8, 9}}}})
	add(tifSpec{name: "resolution_rationals", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		extra: []tentry{{tag: 282, typ: ttRational, vals: []uint32{72, 1}}, {tag: 283, typ: ttRational, vals: []uint32{72, 1}}, {tag: 296, typ: ttShort, vals: []uint32{2}}}})
	add(tifSpec{name: "tags_unsorted", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { b.unsorted = true }})
	add(tifSpec{name: "tags_duplicated", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		extra: []tentry{{tag: 256, typ: ttShort, vals: []uint32{5}}}})
	add(tifSpec{name: "ifd_count_too_high", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { b.numItems = intp(len(b.entries) + 40) }})
	add(tifSpec{name: "ifd_count_zero", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { b.numItems = intp(0) }})
	add(tifSpec{name: "ifd_count_one", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { b.numItems = intp(1) }})
	add(tifSpec{name: "ifd_offset_past_eof", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { b.ifdAt = u32p(1 << 20) }})
	add(tifSpec{name: "ifd_offset_inside_data", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { b.ifdAt = u32p(9) }})
	add(tifSpec{name: "ifd_entry_datatype_0", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setType(b, 256, 0) }})
	add(tifSpec{name: "ifd_entry_datatype_7", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setType(b, 256, 7) }})
	add(tifSpec{name: "ifd_entry_ascii_width", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setType(b, 256, ttASCII) }})
	add(tifSpec{name: "ifd_entry_rational_width", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setType(b, 256, ttRational) }})
	add(tifSpec{name: "ifd_entry_byte_width", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setType(b, 256, ttByte) }})
	// 800000000 Longs is over MaxInt32/4 and under MaxUint32/4, so it separates the reader's
	// `count > MaxInt32/lengths[datatype]` from the looser bound a reader might write instead.
	add(tifSpec{name: "ifd_count_overflows", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setCount(b, 273, 800000000) }})
	add(tifSpec{name: "ifd_count_max_int32", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setCount(b, 273, 0x7fffffff) }})
	add(tifSpec{name: "ifd_count_just_under", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setCount(b, 273, 536870911) }})
	add(tifSpec{name: "ifd_external_pointer_past_eof", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, rowsPerStrip: 1, seed: next(),
		mutate: func(b *tbuild) { setWord(b, 273, 1<<20) }})
	add(tifSpec{name: "sample_format_1", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, sampleFormat: []uint32{1}, seed: next()})
	add(tifSpec{name: "sample_format_2", w: 5, h: 4, bps: []uint32{8}, photometric: 1, compression: 1, sampleFormat: []uint32{2}, seed: next()})
	add(tifSpec{name: "sample_format_3_values", w: 5, h: 4, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, sampleFormat: []uint32{1, 1, 3}, seed: next()})

	// --- dimensions ---
	add(tifSpec{name: "zero_width", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setShort(b, 256, 0) }})
	add(tifSpec{name: "zero_height", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setShort(b, 257, 0) }})
	add(tifSpec{name: "no_width_tag", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { drop(b, 256) }})
	add(tifSpec{name: "huge_dimensions", w: 4, h: 3, bps: []uint32{8}, photometric: 1, compression: 1, seed: next(),
		mutate: func(b *tbuild) { setLongTag(b, 256, 0xffffffff); setLongTag(b, 257, 0xffffffff) }})
	add(tifSpec{name: "width_one", w: 1, h: 5, bps: []uint32{8}, photometric: 1, compression: 1, seed: next()})
	add(tifSpec{name: "height_one", w: 5, h: 1, bps: []uint32{8}, photometric: 1, compression: 1, seed: next()})
	add(tifSpec{name: "one_by_one_rgb", w: 1, h: 1, bps: []uint32{8, 8, 8}, photometric: 2, compression: 1, seed: next()})

	// --- damaged compressed streams ---
	{
		base := tifSpec{name: "lzw_base", w: 9, h: 6, bps: []uint32{8}, photometric: 1, compression: 5, seed: next()}.build()
		raw("lzw_truncated_strip", withLongTag(base, 279, 4))
		bad := bytes.Clone(base)
		bad[10] ^= 0xff
		raw("lzw_corrupt", bad)
		def := tifSpec{name: "deflate_base", w: 9, h: 6, bps: []uint32{8}, photometric: 1, compression: 8, seed: next()}.build()
		bad = bytes.Clone(def)
		bad[8] = 0x00 // break the zlib header
		raw("deflate_bad_header", bad)
		bad = bytes.Clone(def)
		bad[len(def)/3] ^= 0x55
		raw("deflate_corrupt", bad)
		raw("deflate_truncated_strip", withLongTag(def, 279, 5))
		pb := tifSpec{name: "packbits_base", w: 9, h: 6, bps: []uint32{8}, photometric: 1, compression: 32773, seed: next()}.build()
		raw("packbits_truncated_strip", withLongTag(pb, 279, 3))
	}
	// A PackBits strip with a -128 no-op, and one that expands past blockMaxDataSize.
	{
		b := &tbuild{}
		payload := []byte{0x80, 0x02, 'a', 'b', 'c', 0x80, 0xfd, 'z'}
		off := b.addData(payload)
		b.put(256, ttShort, 3)
		b.put(257, ttShort, 1)
		b.put(258, ttShort, 8)
		b.put(259, ttShort, 32773)
		b.put(262, ttShort, 1)
		b.put(273, ttLong, off)
		b.put(277, ttShort, 1)
		b.put(279, ttLong, uint32(len(payload)))
		raw("packbits_noop_code", b.bytes())
	}
	{
		b := &tbuild{}
		// 2x1 at 8bpp: blockMaxDataSize is 16 bytes, and this expands to 128.
		payload := []byte{0x81, 'q'}
		off := b.addData(payload)
		b.put(256, ttShort, 2)
		b.put(257, ttShort, 1)
		b.put(258, ttShort, 8)
		b.put(259, ttShort, 32773)
		b.put(262, ttShort, 1)
		b.put(273, ttLong, off)
		b.put(277, ttShort, 1)
		b.put(279, ttLong, uint32(len(payload)))
		raw("packbits_bomb", b.bytes())
	}

	// --- truncation ---
	{
		base := tifSpec{name: "trunc_base", w: 9, h: 6, bps: []uint32{8}, photometric: 1, compression: 1, seed: next()}.build()
		for _, cut := range []int{0, 1, 3, 4, 7, 8, 9, 10, 20, len(base) / 2, len(base) - 30, len(base) - 12, len(base) - 4, len(base) - 1} {
			if cut < 0 || cut >= len(base) {
				continue
			}
			raw(fmt.Sprintf("truncated_%d", cut), base[:cut])
		}
		raw("trailing_garbage", append(bytes.Clone(base), "garbage"...))
	}

	return files
}

// --- IFD surgery helpers ------------------------------------------------------------------------

func findEntry(b *tbuild, tag uint16) *tentry {
	for i := range b.entries {
		if b.entries[i].tag == tag {
			return &b.entries[i]
		}
	}
	panic(fmt.Sprintf("no entry for tag %d", tag))
}

func setShort(b *tbuild, tag uint16, v uint32) {
	e := findEntry(b, tag)
	e.typ = ttShort
	e.vals = []uint32{v}
}

func setLongTag(b *tbuild, tag uint16, v uint32) {
	e := findEntry(b, tag)
	e.typ = ttLong
	e.vals = []uint32{v}
}

// setLong replaces every value of a Long entry with v.
func setLong(b *tbuild, tag uint16, v uint32) {
	e := findEntry(b, tag)
	for i := range e.vals {
		e.vals[i] = v
	}
}

// setLongAt replaces one value of a Long entry.
func setLongAt(b *tbuild, tag uint16, i int, v uint32) {
	findEntry(b, tag).vals[i] = v
}

func setType(b *tbuild, tag, typ uint16) { findEntry(b, tag).typ = typ }

func setCount(b *tbuild, tag uint16, c uint32) { findEntry(b, tag).count = u32p(c) }

func setWord(b *tbuild, tag uint16, w uint32) { findEntry(b, tag).word = u32p(w) }

func drop(b *tbuild, tag uint16) {
	out := b.entries[:0]
	for _, e := range b.entries {
		if e.tag != tag {
			out = append(out, e)
		}
	}
	b.entries = out
}

// dropLast removes the final value of a multi-value entry.
func dropLast(b *tbuild, tag uint16) {
	e := findEntry(b, tag)
	e.vals = e.vals[:len(e.vals)-1]
}

// withShortTag rewrites the inline value of an existing Short entry in an assembled file.
func withShortTag(data []byte, tag uint16, v uint16) []byte {
	out := bytes.Clone(data)
	o := fileOrder(out)
	i := entryOffset(out, tag)
	o.PutUint16(out[i+8:i+10], v)
	return out
}

// withLongTag rewrites every value of an existing Long entry, inline or in the pointer area.
func withLongTag(data []byte, tag uint16, v uint32) []byte {
	out := bytes.Clone(data)
	o := fileOrder(out)
	i := entryOffset(out, tag)
	count := o.Uint32(out[i+4 : i+8])
	if 4*count > 4 {
		at := o.Uint32(out[i+8 : i+12])
		for k := uint32(0); k < count; k++ {
			o.PutUint32(out[at+4*k:at+4*k+4], v)
		}
		return out
	}
	o.PutUint32(out[i+8:i+12], v)
	return out
}

func fileOrder(data []byte) binary.ByteOrder {
	if string(data[0:4]) == "MM\x00\x2a" {
		return binary.BigEndian
	}
	return binary.LittleEndian
}

// entryOffset is the file offset of the IFD entry for tag.
func entryOffset(data []byte, tag uint16) int {
	o := fileOrder(data)
	ifd := int(o.Uint32(data[4:8]))
	n := int(o.Uint16(data[ifd : ifd+2]))
	for k := 0; k < n; k++ {
		i := ifd + 2 + 12*k
		if o.Uint16(data[i:i+2]) == tag {
			return i
		}
	}
	panic(fmt.Sprintf("no IFD entry for tag %d", tag))
}

// --- encoded and pinned inputs ------------------------------------------------------------------

// goEncodedTIFFs is what x/image's own writer produces: uncompressed and Deflate, over every
// concrete image type its type switch names plus one it does not (the default arm).
func goEncodedTIFFs() []namedFile {
	var files []namedFile
	seed := uint64(9500)
	specs := []imgSpec{
		{"gray", 13, 9, "smooth", "opaque", 0, 0, ""},
		{"gray16", 11, 7, "gradient", "opaque", 0, 0, ""},
		{"nrgba", 12, 8, "blocks", "mixed", 0, 0, ""},
		{"rgba", 12, 8, "smooth", "mixed", 0, 0, ""},
		{"nrgba64", 9, 6, "noise", "mixed", 0, 0, ""},
		{"rgba64", 9, 6, "noise", "opaque", 0, 0, ""},
		{"paletted", 14, 10, "noise", "opaque", 0, 7, ""},
		{"paletted", 14, 10, "noise", "mixed", 0, 256, ""},
		{"cmyk", 10, 6, "noise", "opaque", 0, 0, ""},
		{"ycbcr", 10, 6, "blocks", "opaque", 0, 0, "420"},
	}
	for _, spec := range specs {
		spec.Seed = seed
		seed++
		m := spec.build()
		for _, o := range []struct {
			name string
			opt  *tiff.Options
		}{
			{"none", nil},
			{"deflate", &tiff.Options{Compression: tiff.Deflate}},
			{"deflate_pred", &tiff.Options{Compression: tiff.Deflate, Predictor: true}},
		} {
			var buf bytes.Buffer
			if err := tiff.Encode(&buf, m, o.opt); err != nil {
				panic(err)
			}
			name := fmt.Sprintf("go_%s_%d_%s", spec.Kind, spec.Palette, o.name)
			files = append(files, namedFile{name, buf.Bytes()})
		}
	}
	return files
}

// bz2TIFFs are the tiled files x/image keeps bzip2'd, decompressed with compress/bzip2 so the
// result is a function of this checkout and not of the host.
func bz2TIFFs() []namedFile {
	var files []namedFile
	for _, f := range xImageTestdata("tiff/testdata", "tiff.bz2") {
		data, err := io.ReadAll(bzip2.NewReader(bytes.NewReader(f.Data)))
		if err != nil {
			panic(fmt.Errorf("bunzip %s: %w", f.Name, err))
		}
		files = append(files, namedFile{f.Name[:len(f.Name)-4], data})
	}
	return files
}

// --- the CCITT reader, driven directly -----------------------------------------------------
//
// `tiff.Decode` only ever calls `ccitt.NewReader` with Align false and an explicit height, so a
// corpus of TIFF files leaves most of ccitt/reader.go unexercised: the byte-alignment paths, the
// AutoDetectHeight paths, LSB bit order, and the truncated-trailer tolerance. These cases drive
// the reader the way the package's own API does, over the raw CCITT streams x/image ships.

type ccittCase struct {
	Name   string `json:"name"`
	B64    string `json:"b64"`
	Order  string `json:"order"` // "msb" or "lsb"
	Sub    string `json:"sub"`   // "group3" or "group4"
	Width  int    `json:"width"`
	Height int    `json:"height"` // -1 is ccitt.AutoDetectHeight
	Align  bool   `json:"align"`
	Invert bool   `json:"invert"`
	OutSHA string `json:"out_sha256"`
	OutLen int    `json:"out_len"`
	Err    any    `json:"err"`
}

func runCCITT(name string, data []byte, order string, sub string, w, h int, align, invert bool) ccittCase {
	o := ccitt.MSB
	if order == "lsb" {
		o = ccitt.LSB
	}
	sf := ccitt.Group3
	if sub == "group4" {
		sf = ccitt.Group4
	}
	r := ccitt.NewReader(bytes.NewReader(data), o, sf, w, h, &ccitt.Options{Align: align, Invert: invert})
	out, err := io.ReadAll(r)
	return ccittCase{
		Name: name, B64: b64(data), Order: order, Sub: sub, Width: w, Height: h,
		Align: align, Invert: invert,
		OutSHA: sha(out), OutLen: len(out), Err: imgErr(err),
	}
}

// ccittStreams are the raw CCITT streams in x/image's own ccitt/testdata, which cover the
// aligned, inverted and truncated variants of one 153x55 image.
func ccittStreams() []namedFile {
	var files []namedFile
	files = append(files, xImageTestdata("ccitt/testdata", "ccitt_group3")...)
	files = append(files, xImageTestdata("ccitt/testdata", "ccitt_group4")...)
	return files
}

func ccittCases() []ccittCase {
	const gopherW, gopherH = 153, 55
	var cases []ccittCase
	for _, f := range ccittStreams() {
		sub := "group3"
		if strings.HasSuffix(f.Name, "group4") {
			sub = "group4"
		}
		align := strings.Contains(f.Name, "aligned")
		invert := strings.Contains(f.Name, "inverted")
		// The combination the file was written with, then each option flipped: an option the
		// stream was not written with is a decode failure or a different image, and both are
		// answers the port has to reproduce.
		for _, v := range []struct {
			tag           string
			order         string
			w, h          int
			align, invert bool
		}{
			{"", "msb", gopherW, gopherH, align, invert},
			{"_noalign", "msb", gopherW, gopherH, false, invert},
			{"_align", "msb", gopherW, gopherH, true, invert},
			{"_flipinvert", "msb", gopherW, gopherH, align, !invert},
			{"_lsb", "lsb", gopherW, gopherH, align, invert},
			{"_autoheight", "msb", gopherW, -1, align, invert},
		} {
			cases = append(cases, runCCITT(f.Name+v.tag, f.Data, v.order, sub, v.w, v.h, v.align, v.invert))
		}
	}
	// Geometry the stream does not match, and the two widths NewReader rejects outright.
	for _, f := range ccittStreams() {
		if f.Name != "bw-gopher.ccitt_group3" && f.Name != "bw-gopher.ccitt_group4" {
			continue
		}
		sub := "group3"
		if strings.HasSuffix(f.Name, "group4") {
			sub = "group4"
		}
		for _, wh := range [][2]int{{152, 55}, {154, 55}, {153, 54}, {153, 56}, {153, 0}, {8, 55}, {0, 3}, {-1, 5}, {1 << 21, 5}} {
			cases = append(cases, runCCITT(fmt.Sprintf("%s_%dx%d", f.Name, wh[0], wh[1]), f.Data, "msb", sub, wh[0], wh[1], false, false))
		}
		for _, cut := range []int{0, 1, 2, 5, 17, 64, len(f.Data) / 2, len(f.Data) - 3, len(f.Data) - 1} {
			if cut < 0 || cut > len(f.Data) {
				continue
			}
			cases = append(cases, runCCITT(fmt.Sprintf("%s_cut%d", f.Name, cut), f.Data[:cut], "msb", sub, gopherW, gopherH, false, false))
		}
		// A byte flipped in the middle of the stream: an invalid code, an invalid mode or a run
		// that overflows the row, depending where it lands.
		for _, pos := range []int{3, 11, 40, 100, len(f.Data) - 10} {
			if pos < 0 || pos >= len(f.Data) {
				continue
			}
			bad := bytes.Clone(f.Data)
			bad[pos] ^= 0x5a
			cases = append(cases, runCCITT(fmt.Sprintf("%s_flip%d", f.Name, pos), bad, "msb", sub, gopherW, gopherH, false, false))
		}
	}
	// Hand-made streams for the codes the gopher image may not contain: an immediate EOL, an
	// all-zero stream, an all-one stream and the extension mode (0000001 in Table 1).
	for _, h := range []struct {
		name string
		data []byte
	}{
		{"eol_only", []byte{0x00, 0x10}},
		{"eol_then_eol", []byte{0x00, 0x10, 0x01}},
		{"zeros", make([]byte, 16)},
		{"ones", bytes.Repeat([]byte{0xff}, 16)},
		{"ext_mode", []byte{0x02, 0x00, 0x00, 0x00}},
		{"empty", nil},
	} {
		for _, sub := range []string{"group3", "group4"} {
			for _, wh := range [][2]int{{8, 2}, {8, -1}, {0, 1}} {
				cases = append(cases, runCCITT(fmt.Sprintf("hand_%s_%s_%dx%d", h.name, sub, wh[0], wh[1]), h.data, "msb", sub, wh[0], wh[1], false, false))
			}
		}
	}
	return cases
}

// ccittTIFFs wraps the raw CCITT streams in TIFF containers, which is how tiff.Decode reaches
// the ccitt package: Align is always false there and the height is always the block height.
func ccittTIFFs() []namedFile {
	var files []namedFile
	for _, f := range ccittStreams() {
		comp := uint32(3)
		if strings.HasSuffix(f.Name, "group4") {
			comp = 4
		}
		for _, v := range []struct {
			tag         string
			photometric uint32
			fillOrder   uint32
			w, h, rps   int
		}{
			{"_p0", 0, 0, 153, 55, 0},
			{"_p1", 1, 0, 153, 55, 0},
			{"_fill2", 0, 2, 153, 55, 0},
			{"_strips", 0, 0, 153, 55, 11},
			{"_narrow", 0, 0, 100, 55, 0},
			{"_short", 0, 0, 153, 20, 0},
		} {
			b := &tbuild{}
			off := b.addData(f.Data)
			b.put(256, ttShort, uint32(v.w))
			b.put(257, ttShort, uint32(v.h))
			b.put(258, ttShort, 1)
			b.put(259, ttShort, comp)
			b.put(262, ttShort, v.photometric)
			if v.fillOrder != 0 {
				b.put(266, ttShort, v.fillOrder)
			}
			b.put(273, ttLong, off)
			b.put(277, ttShort, 1)
			b.put(279, ttLong, uint32(len(f.Data)))
			if v.rps > 0 {
				b.put(278, ttShort, uint32(v.rps))
				// Every strip points at the whole stream: only the first can decode.
				n := (v.h + v.rps - 1) / v.rps
				offs := make([]uint32, n)
				counts := make([]uint32, n)
				for i := range offs {
					offs[i], counts[i] = off, uint32(len(f.Data))
				}
				setLongVals(b, 273, offs)
				setLongVals(b, 279, counts)
			}
			files = append(files, namedFile{"ccitt_" + f.Name + v.tag, b.bytes()})
		}
	}
	// One tiled G4 file, so the tile path meets the ccitt reader too.
	{
		f := ccittStreams()[0]
		b := &tbuild{}
		off := b.addData(f.Data)
		b.put(256, ttShort, 32)
		b.put(257, ttShort, 32)
		b.put(258, ttShort, 1)
		b.put(259, ttShort, 4)
		b.put(262, ttShort, 0)
		b.put(277, ttShort, 1)
		b.put(322, ttShort, 16)
		b.put(323, ttShort, 16)
		b.put(324, ttLong, off, off, off, off)
		b.put(325, ttLong, uint32(len(f.Data)), uint32(len(f.Data)), uint32(len(f.Data)), uint32(len(f.Data)))
		files = append(files, namedFile{"ccitt_tiled_g4", b.bytes()})
	}
	return files
}

// setLongVals replaces a Long entry's whole value list.
func setLongVals(b *tbuild, tag uint16, vals []uint32) {
	e := findEntry(b, tag)
	e.typ = ttLong
	e.vals = vals
}

// tiffCorpus is every decoder input for the tiff stage, in a stable order. The pipeline stage
// draws a slice of it by name.
func tiffCorpus() []namedFile {
	var corpus []namedFile
	corpus = append(corpus, xImageTestdata("testdata", "tiff")...)
	corpus = append(corpus, xImageTestdata("tiff/testdata", "tiff")...)
	corpus = append(corpus, bz2TIFFs()...)
	corpus = append(corpus, goEncodedTIFFs()...)
	corpus = append(corpus, craftedTIFFs()...)
	corpus = append(corpus, ccittTIFFs()...)
	return corpus
}

func imagingTIFFStage() (map[string]any, error) {
	var dec []decodeCase
	for _, f := range tiffCorpus() {
		dec = append(dec, decodeCase{Name: f.Name, B64: b64(f.Data), Config: configOf(f.Data), Image: imageOf(f.Data)})
	}
	return map[string]any{"decode": dec, "ccitt": ccittCases()}, nil
}

// keep the image and color imports honest if a case above is ever commented out.
var _ = image.Rect
var _ = color.Black
