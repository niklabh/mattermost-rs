package main

// The BMP stage of the imaging oracle: golang.org/x/image/bmp's decoder, which
// channels/app/imaging/decode.go registers. See behaviour_imaging.go for the generator and the
// conventions, and behaviour_imaging_codec.go for the `decodeCase`/`configOf`/`imageOf` shape
// every decode corpus uses.
//
// # Two answers per file
//
// `config` and `image` are `image.DecodeConfig`/`image.Decode` — the registry, which sniffs the
// magic ("BM????\x00\x00\x00\x00") and hands the bytes to the decoder through a `bufio.Reader`.
// `direct` is `bmp.DecodeConfig`/`bmp.Decode` over a bare `bytes.Reader`. The Rust port answers
// the *decoder's* question, so it is checked against `direct`; recording both also pins the
// claim that the interposed `bufio.Reader` changes no answer, and gives whoever wires
// `crates/goimage/src/format.rs` the registry's side.
//
// # The corpus
//
// Three sources. golang.org/x/image's own pinned testdata (1, 4 and 8 bit-per-pixel files and the
// three colormap variants). Files this program writes with `bmp.Encode`, which reaches the 8-, 24-
// and 32-bit paths through every image type the encoder special-cases. And crafted files, built
// field by field by `bmpSpec`, because most of the reader is header validation and an encoder
// never produces an invalid header: the BITMAPV4/V5 sizes, the BITFIELDS-with-default-masks
// relaxation, every rejected compression and plane count, palettes shorter than the indices that
// use them, offsets off by four in both directions, and truncations at each of the reader's four
// `io.ReadFull` calls — two of which map `io.EOF` to `io.ErrUnexpectedEOF` and two of which do not.
//
// Every crafted file carries distinctive non-zero junk in the header fields the reader must *not*
// read (image size, the pixels-per-metre pair, "colours important") and 0xcc in every row-padding
// byte, so a port that reads the right value from the wrong offset lands on a value that differs.

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"math"

	"golang.org/x/image/bmp"
)

// bmpCase is decodeCase plus `direct`: what the bmp package itself answered, bypassing the
// registry's sniffing and its bufio.Reader.
type bmpCase struct {
	Name   string         `json:"name"`
	B64    string         `json:"b64"`
	Config map[string]any `json:"config"`
	Image  map[string]any `json:"image"`
	Direct map[string]any `json:"direct"`
}

func bmpDirect(data []byte) map[string]any {
	d := map[string]any{}
	if cfg, err := bmp.DecodeConfig(bytes.NewReader(data)); err != nil {
		d["config"] = map[string]any{"err": err.Error()}
	} else {
		d["config"] = map[string]any{"w": cfg.Width, "h": cfg.Height, "model": modelName(cfg.ColorModel)}
	}
	if m, err := bmp.Decode(bytes.NewReader(data)); err != nil {
		d["image"] = map[string]any{"err": err.Error()}
	} else {
		d["image"] = describe(m)
	}
	return d
}

// --- building a BMP byte by byte ------------------------------------------------------------------

// bmpSpec is every field of the 14-byte file header and the DIB header the reader looks at, each
// settable on its own. `bmpOf` fills in the defaults; a case overrides the one field it is about.
type bmpSpec struct {
	magic string
	// bytes 6..10, the two reserved uint16s. Non-zero hides the file from image.Decode's sniffing
	// without changing a thing for bmp.Decode.
	reserved uint32
	// -1: the natural 14 + len(DIB) + len(palette).
	offset int64
	// The DIB header's length in bytes. 40 unless the case is a BITMAPV4/V5 header.
	infoLen uint32
	// -1: the same as infoLen. Otherwise the value *written* into the DIB's first field while the
	// body stays 40 bytes — for the sizes the reader rejects before reading any more.
	declaredInfoLen int64
	width, height   int32
	planes, bpp     uint16
	compression     uint32
	// Fields the reader must never read.
	imageSize      uint32
	xppm, yppm     int32
	colorUsed      uint32
	colorImportant uint32
	// The red, green, blue and alpha bit masks at file bytes 54..70 (a V4/V5 header only).
	masks   [4]uint32
	palette []byte
	pixels  []byte
}

func bmpOf(w, h int32, bpp uint16) *bmpSpec {
	return &bmpSpec{
		magic: "BM", offset: -1, infoLen: 40, declaredInfoLen: -1,
		width: w, height: h, planes: 1, bpp: bpp,
		imageSize: 0x0a0b0c0d, xppm: 2835, yppm: 5671, colorImportant: 0x03020100,
	}
}

func bmpPutU32(b []byte, v uint32) { binary.LittleEndian.PutUint32(b, v) }
func bmpPutU16(b []byte, v uint16) { binary.LittleEndian.PutUint16(b, v) }

func (s *bmpSpec) build() []byte {
	dib := make([]byte, 40)
	declared := s.infoLen
	if s.declaredInfoLen >= 0 {
		declared = uint32(s.declaredInfoLen)
	}
	bmpPutU32(dib[0:], declared)
	bmpPutU32(dib[4:], uint32(s.width))
	bmpPutU32(dib[8:], uint32(s.height))
	bmpPutU16(dib[12:], s.planes)
	bmpPutU16(dib[14:], s.bpp)
	bmpPutU32(dib[16:], s.compression)
	bmpPutU32(dib[20:], s.imageSize)
	bmpPutU32(dib[24:], uint32(s.xppm))
	bmpPutU32(dib[28:], uint32(s.yppm))
	bmpPutU32(dib[32:], s.colorUsed)
	bmpPutU32(dib[36:], s.colorImportant)
	switch {
	case s.infoLen > 40:
		ext := make([]byte, s.infoLen-40)
		for i, m := range s.masks {
			if 4*i+4 <= len(ext) {
				bmpPutU32(ext[4*i:], m)
			}
		}
		dib = append(dib, ext...)
	case s.infoLen < 40:
		dib = dib[:s.infoLen]
	}

	off := uint32(14 + len(dib) + len(s.palette))
	if s.offset >= 0 {
		off = uint32(s.offset)
	}
	var b bytes.Buffer
	b.WriteString(s.magic)
	total := uint32(14 + len(dib) + len(s.palette) + len(s.pixels))
	bmpPutU32Buf(&b, total)
	bmpPutU32Buf(&b, s.reserved)
	bmpPutU32Buf(&b, off)
	b.Write(dib)
	b.Write(s.palette)
	b.Write(s.pixels)
	return b.Bytes()
}

func bmpPutU32Buf(b *bytes.Buffer, v uint32) {
	var tmp [4]byte
	bmpPutU32(tmp[:], v)
	b.Write(tmp[:])
}

// bmpFileRows is the file order of the image's rows: bottom-up unless the header said top-down.
func bmpFileRows(h int, topDown bool) []int {
	ys := make([]int, 0, h)
	if topDown {
		for y := 0; y < h; y++ {
			ys = append(ys, y)
		}
		return ys
	}
	for y := h - 1; y >= 0; y-- {
		ys = append(ys, y)
	}
	return ys
}

// bmpPalette is a colour table whose blue, green and red bytes all differ, so a port that reads
// them in RGB order rather than BGR produces a different palette; the fourth byte of each entry is
// padding the reader must ignore in favour of a hard 0xFF alpha, so it is not 0xFF here.
func bmpPalette(n int) []byte {
	p := make([]byte, 4*n)
	for i := 0; i < n; i++ {
		p[4*i+0] = byte(7*i + 1)
		p[4*i+1] = byte(7*i + 2)
		p[4*i+2] = byte(7*i + 3)
		p[4*i+3] = 0x7c
	}
	return p
}

// bmpRows24 lays out 24-bit rows: three bytes per pixel in BGR order, each row padded to a
// multiple of four with 0xcc. Every channel of every pixel is a different byte, so a row read in
// the wrong order or a channel triple read as RGB hashes differently.
func bmpRows24(w, h int, topDown bool) []byte {
	step := (3*w + 3) &^ 3
	var out []byte
	for _, y := range bmpFileRows(h, topDown) {
		row := make([]byte, step)
		for i := 3 * w; i < step; i++ {
			row[i] = 0xcc
		}
		for x := 0; x < w; x++ {
			i := 3 * (y*w + x)
			row[3*x+0], row[3*x+1], row[3*x+2] = byte(i+2), byte(i+1), byte(i)
		}
		out = append(out, row...)
	}
	return out
}

// bmpRows32 lays out 32-bit rows: four bytes per pixel in BGRA order and never any padding. The
// alpha byte is only 0xFF once every 64 pixels, so the allowAlpha branch is visible.
func bmpRows32(w, h int, topDown bool) []byte {
	var out []byte
	for _, y := range bmpFileRows(h, topDown) {
		row := make([]byte, 4*w)
		for x := 0; x < w; x++ {
			i := 4 * (y*w + x)
			row[4*x+0], row[4*x+1] = byte(i+2), byte(i+1)
			row[4*x+2], row[4*x+3] = byte(i), byte(i+3)
		}
		out = append(out, row...)
	}
	return out
}

// bmpRowsIdx lays out 1, 2, 4 or 8 bit-per-pixel rows, the first pixel in each byte's high bits.
// The bits past the last pixel of the last used byte are set, and the bytes past that are 0xcc:
// both are outside the `pixIndex < c.Width` loop and a port that reads them produces other indices.
func bmpRowsIdx(w, h, bpp int, topDown bool, idx func(x, y int) byte) []byte {
	ppb := 8 / bpp
	used := (w + ppb - 1) / ppb
	step := (used + 3) &^ 3
	var out []byte
	for _, y := range bmpFileRows(h, topDown) {
		row := make([]byte, step)
		for i := used; i < step; i++ {
			row[i] = 0xcc
		}
		for x := 0; x < w; x++ {
			shift := uint(8 - bpp*(x%ppb+1))
			row[x/ppb] |= idx(x, y) << shift
		}
		if used > 0 && w%ppb != 0 {
			row[used-1] |= byte(1<<uint(8-bpp*(w%ppb))) - 1
		}
		out = append(out, row...)
	}
	return out
}

// bmpIndices is the index pattern: it varies with x within a byte and with y between rows, so the
// bit order inside a byte and the bottom-up row order are both load-bearing.
func bmpIndices(n int) func(x, y int) byte {
	return func(x, y int) byte { return byte((3*x + 5*y) % n) }
}

func imagingBMPStage() (map[string]any, error) {
	var cases []bmpCase
	addRaw := func(name string, data []byte) {
		cases = append(cases, bmpCase{
			Name: name, B64: b64(data),
			Config: configOf(data), Image: imageOf(data), Direct: bmpDirect(data),
		})
	}

	// --- golang.org/x/image's own corpus ----------------------------------------------------
	for _, f := range xImageTestdata("testdata", "bmp") {
		addRaw(f.Name, f.Data)
	}

	// --- what bmp.Encode writes -------------------------------------------------------------
	//
	// Encode always writes a 40-byte header, a 1024-byte colour table for *image.Gray and
	// *image.Paletted (so colourUsed stays 0 and the reader defaults it to 256), 24 bits per
	// pixel for an opaque RGBA/NRGBA and for everything that falls to its generic path, and 32
	// for a non-opaque one.
	seed := uint64(7000)
	for _, spec := range []imgSpec{
		{"gray", 1, 1, "noise", "opaque", 0, 0, ""},
		{"gray", 5, 4, "gradient", "opaque", 0, 0, ""},
		{"gray", 13, 9, "smooth", "opaque", 0, 0, ""},
		{"gray", 0, 0, "noise", "opaque", 0, 0, ""},
		{"paletted", 7, 3, "noise", "opaque", 0, 4, ""},
		{"paletted", 9, 5, "blocks", "opaque", 0, 17, ""},
		{"paletted", 40, 30, "blocks", "mixed", 0, 256, ""},
		{"rgba", 5, 4, "smooth", "opaque", 0, 0, ""},
		{"rgba", 5, 4, "smooth", "mixed", 0, 0, ""},
		{"nrgba", 9, 7, "blocks", "opaque", 0, 0, ""},
		{"nrgba", 9, 7, "blocks", "binary", 0, 0, ""},
		{"nrgba", 1, 1, "flat", "soft", 0, 0, ""},
		{"nrgba", 64, 48, "noise", "mixed", 0, 0, ""},
		{"gray16", 6, 5, "gradient", "opaque", 0, 0, ""},
		{"ycbcr", 8, 6, "smooth", "opaque", 0, 0, "420"},
		{"cmyk", 4, 3, "blocks", "opaque", 0, 0, ""},
	} {
		spec.Seed = seed
		seed++
		data := mustEncode(func(b *bytes.Buffer) error { return bmp.Encode(b, spec.build()) })
		addRaw(fmt.Sprintf("go_%s_%s_%dx%d", spec.Kind, spec.Alpha, spec.W, spec.H), data)
	}

	// --- crafted --------------------------------------------------------------------------
	craft := func(name string, s *bmpSpec) { addRaw("crafted_"+name, s.build()) }
	rows24 := bmpRows24(5, 3, false)
	rows24td := bmpRows24(5, 3, true)
	rows32 := bmpRows32(5, 3, false)
	rows32td := bmpRows32(5, 3, true)
	pal256 := bmpPalette(256)
	rows8 := bmpRowsIdx(5, 3, 8, false, bmpIndices(256))

	good24 := func() *bmpSpec { s := bmpOf(5, 3, 24); s.pixels = rows24; return s }
	good32 := func() *bmpSpec { s := bmpOf(5, 3, 32); s.pixels = rows32; return s }
	good8 := func() *bmpSpec {
		s := bmpOf(5, 3, 8)
		s.palette, s.pixels = pal256, rows8
		return s
	}

	craft("baseline_24_bottomup", good24())
	craft("baseline_32_bottomup", good32())
	craft("baseline_8_bottomup", good8())
	{
		s := bmpOf(5, -3, 24)
		s.pixels = rows24td
		craft("baseline_24_topdown", s)
	}
	{
		s := bmpOf(5, -3, 32)
		s.pixels = rows32td
		craft("baseline_32_topdown", s)
	}
	{
		s := bmpOf(5, -3, 8)
		s.palette, s.pixels = pal256, bmpRowsIdx(5, 3, 8, true, bmpIndices(256))
		craft("baseline_8_topdown", s)
	}
	{
		s := good24()
		s.reserved = 0x0000_0100
		craft("reserved_nonzero", s)
	}
	{
		s := good24()
		s.pixels = append(bytes.Clone(rows24), bytes.Repeat([]byte{0x5e}, 50)...)
		craft("trailing_garbage", s)
	}

	// Signature.
	for _, m := range []string{"MB", "bm", "BA", "B\x00", "\x00M"} {
		s := good24()
		s.magic = m
		craft("magic_"+hexs([]byte(m)), s)
	}

	// DIB header length: 40, 108 and 124 are the three the reader takes.
	for _, n := range []uint32{0, 12, 16, 39, 41, 52, 56, 64, 107, 109, 123, 125, 0xffffffff} {
		s := good24()
		s.declaredInfoLen = int64(n)
		craft("infolen_"+itoa(int(n)), s)
	}
	for _, n := range []uint32{108, 124} {
		s := bmpOf(5, 3, 24)
		s.infoLen, s.pixels = n, rows24
		craft("infolen_"+itoa(int(n))+"_24", s)
		p := bmpOf(5, 3, 8)
		p.infoLen, p.palette, p.pixels = n, pal256, rows8
		craft("infolen_"+itoa(int(n))+"_8", p)
		q := bmpOf(5, 3, 32)
		q.infoLen, q.pixels = n, rows32
		craft("infolen_"+itoa(int(n))+"_32", q)
	}

	// Dimensions.
	{
		s := bmpOf(-5, 3, 24)
		craft("negative_width", s)
	}
	{
		s := bmpOf(math.MinInt32, 3, 24)
		craft("width_min_int32", s)
	}
	{
		// int32 min as the height negates to 2147483648, which is a positive Go int on a 64-bit
		// machine: the reader gets past the sign checks and past Mul3, and stops at the bit
		// depth. Nothing here allocates.
		s := bmpOf(1, math.MinInt32, 16)
		craft("height_min_int32_bpp16", s)
	}
	{
		s := bmpOf(0, 3, 24)
		craft("zero_width", s)
	}
	{
		s := bmpOf(5, 0, 24)
		craft("zero_height", s)
	}
	{
		s := bmpOf(0, -3, 24)
		craft("zero_width_topdown", s)
	}
	craft("zero_both_24", bmpOf(0, 0, 24))
	craft("zero_both_32", bmpOf(0, 0, 32))
	{
		s := bmpOf(0, 0, 8)
		s.palette = pal256
		craft("zero_both_8", s)
	}
	{
		s := bmpOf(0, 0, 1)
		s.palette = bmpPalette(2)
		craft("zero_both_1", s)
	}
	{
		// Mul3(w, h, 4) overflows a Go int, so the reader refuses before allocating anything.
		s := bmpOf(math.MaxInt32, math.MaxInt32, 24)
		craft("mul3_overflow_24", s)
	}
	{
		s := bmpOf(math.MaxInt32, math.MaxInt32, 8)
		s.palette = pal256
		craft("mul3_overflow_8", s)
	}

	// Colour planes and compression.
	for _, p := range []uint16{0, 2, 3, 0xffff} {
		s := good24()
		s.planes = p
		craft("planes_"+itoa(int(p)), s)
	}
	for _, c := range []uint32{1, 2, 3, 4, 5, 6, 11, 13} {
		s := good24()
		s.compression = c
		craft("compression_"+itoa(int(c)), s)
	}

	// BI_BITFIELDS with exactly the masks a compression of 0 implies is treated as a compression
	// of 0 — but only in a header large enough to carry them.
	defMasks := [4]uint32{0xff0000, 0xff00, 0xff, 0xff000000}
	{
		s := good32()
		s.compression, s.masks = 3, defMasks
		craft("bitfields_default_masks_v3_32", s)
	}
	for _, n := range []uint32{108, 124} {
		s := bmpOf(5, 3, 32)
		s.infoLen, s.compression, s.masks, s.pixels = n, 3, defMasks, rows32
		craft(fmt.Sprintf("bitfields_default_masks_%d_32", n), s)
	}
	{
		s := bmpOf(5, 3, 24)
		s.infoLen, s.compression, s.masks, s.pixels = 108, 3, defMasks, rows24
		craft("bitfields_default_masks_108_24", s)
	}
	{
		s := bmpOf(5, 3, 8)
		s.infoLen, s.compression, s.masks = 108, 3, defMasks
		s.palette, s.pixels = pal256, rows8
		craft("bitfields_default_masks_108_8", s)
	}
	for i, name := range []string{"red", "green", "blue", "alpha"} {
		m := defMasks
		m[i] = 0x0f0f0f0f
		s := bmpOf(5, 3, 32)
		s.infoLen, s.compression, s.masks, s.pixels = 108, 3, m, rows32
		craft("bitfields_wrong_"+name+"_mask", s)
	}
	{
		// Every default mask is present, each one offset by four bytes from where it belongs.
		s := bmpOf(5, 3, 32)
		s.infoLen, s.compression, s.pixels = 108, 3, rows32
		s.masks = [4]uint32{0xff000000, 0xff0000, 0xff00, 0xff}
		craft("bitfields_rotated_masks", s)
	}
	{
		// The default masks in a header that did not claim BI_BITFIELDS: nothing to relax.
		s := bmpOf(5, 3, 32)
		s.infoLen, s.masks, s.pixels = 108, defMasks, rows32
		craft("bitfields_masks_without_compression", s)
	}

	// Bit depths the reader has no case for.
	for _, bpp := range []uint16{0, 3, 5, 6, 7, 9, 15, 16, 25, 31, 33, 48, 64, 0xffff} {
		s := bmpOf(5, 3, bpp)
		craft("bpp_"+itoa(int(bpp)), s)
	}

	// Every palette bit depth, at widths that leave a partial byte and rows that need padding.
	for _, t := range [][3]int{
		{1, 1, 1}, {1, 7, 2}, {1, 8, 3}, {1, 9, 3}, {1, 32, 2}, {1, 33, 2},
		{2, 1, 1}, {2, 3, 2}, {2, 5, 3}, {2, 8, 2}, {2, 9, 2},
		{4, 1, 1}, {4, 3, 3}, {4, 7, 2}, {4, 8, 2}, {4, 9, 2},
		{8, 1, 1}, {8, 4, 2}, {8, 5, 3}, {8, 8, 2},
	} {
		bpp, w, h := t[0], t[1], t[2]
		n := 1 << uint(bpp)
		for _, td := range []bool{false, true} {
			height := int32(h)
			dir := "bottomup"
			if td {
				height, dir = -height, "topdown"
			}
			s := bmpOf(int32(w), height, uint16(bpp))
			s.palette = bmpPalette(n)
			s.pixels = bmpRowsIdx(w, h, bpp, td, bmpIndices(n))
			craft(fmt.Sprintf("paletted_%dbpp_%dx%d_%s", bpp, w, h, dir), s)
		}
	}

	// colourUsed: 0 means the full table, anything past 1<<bpp is refused, and a short table is
	// only as long as it says — an index past its end is an error, not a wrap.
	for _, t := range []struct {
		bpp       int
		colorUsed uint32
		maxIdx    int
		name      string
	}{
		{8, 256, 256, "8bpp_256"},
		{8, 257, 256, "8bpp_257"},
		{8, 1, 1, "8bpp_1"},
		{8, 3, 3, "8bpp_3_ok"},
		{8, 3, 4, "8bpp_3_index_3"},
		{8, 200, 250, "8bpp_200_index_249"},
		{4, 16, 16, "4bpp_16"},
		{4, 17, 16, "4bpp_17"},
		{4, 3, 3, "4bpp_3_ok"},
		{4, 3, 4, "4bpp_3_index_3"},
		{2, 4, 4, "2bpp_4"},
		{2, 5, 4, "2bpp_5"},
		{2, 2, 3, "2bpp_2_index_2"},
		{1, 2, 2, "1bpp_2"},
		{1, 3, 2, "1bpp_3"},
		{1, 1, 2, "1bpp_1_index_1"},
	} {
		n := int(t.colorUsed)
		if n > 1<<uint(t.bpp) {
			n = 1 << uint(t.bpp)
		}
		s := bmpOf(6, 2, uint16(t.bpp))
		s.colorUsed = t.colorUsed
		s.palette = bmpPalette(n)
		s.pixels = bmpRowsIdx(6, 2, t.bpp, false, bmpIndices(t.maxIdx))
		craft("colorused_"+t.name, s)
	}

	// Where the pixel data is said to start. The reader never seeks: the offset is a claim it
	// checks against the bytes it has already read past.
	for _, d := range []int64{-4, -1, 1, 4} {
		s := good24()
		s.offset = 54 + d
		craft(fmt.Sprintf("offset_24_%+d", d), s)
		q := good32()
		q.offset = 54 + d
		craft(fmt.Sprintf("offset_32_%+d", d), q)
		p := good8()
		p.offset = 1078 + d
		craft(fmt.Sprintf("offset_8_%+d", d), p)
	}
	{
		s := good24()
		s.offset = 0
		craft("offset_24_zero", s)
	}
	{
		s := bmpOf(5, 3, 24)
		s.infoLen, s.pixels, s.offset = 108, rows24, 54
		craft("offset_v4_24_as_if_v3", s)
	}
	{
		// colourUsed 0 defaults to 256, so the offset must clear a 1024-byte table even though
		// the file's own colourUsed field says nothing.
		s := good8()
		s.offset = 54
		craft("offset_8_no_palette", s)
	}

	// Truncation, at each of the four io.ReadFull calls. The two header reads map io.EOF to
	// io.ErrUnexpectedEOF; the colour-table read and the pixel reads do not, so a file that stops
	// exactly on one of those boundaries answers "EOF".
	full24 := good24().build()
	for _, n := range []int{0, 1, 2, 9, 10, 13, 14, 17, 18, 19, 30, 53, 54, 55, 60, 69, 70, 85, 86, 101} {
		if n <= len(full24) {
			addRaw("cut24_"+itoa(n), full24[:n])
		}
	}
	full32 := good32().build()
	for _, n := range []int{54, 55, 60, 73, 74, 90, 113} {
		if n <= len(full32) {
			addRaw("cut32_"+itoa(n), full32[:n])
		}
	}
	full8 := good8().build()
	for _, n := range []int{54, 55, 500, 1077, 1078, 1079, 1082, 1085, 1086, 1093, 1101} {
		if n <= len(full8) {
			addRaw("cut8_"+itoa(n), full8[:n])
		}
	}

	return map[string]any{"decode": cases}, nil
}
