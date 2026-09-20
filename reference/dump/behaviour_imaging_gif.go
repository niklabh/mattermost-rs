package main

// The GIF stage of the imaging oracle. See behaviour_imaging.go for the generator and the
// conventions, and behaviour_imaging_codec.go for the `decodeCase`/`configOf`/`imageOf` shape
// every decode corpus uses.
//
// Two arrays, over one corpus in one order:
//
//   - "decode" is the usual `decodeCase`: `image.DecodeConfig` and `image.Decode` over the whole
//     registry, so a file whose first six bytes are not "GIF8?a" is recorded as "image: unknown
//     format" exactly as the PNG stage records its non-PNGs.
//   - "decode_all" calls `gif.DecodeConfig`, `gif.Decode` and `gif.DecodeAll` *directly*, so the
//     decoder's own error text is recorded even for inputs the registry would never route to it,
//     and so the frames past the first — which `image.Decode` never shows — are covered: per
//     frame the `describe`, the delay and the disposal, plus the loop count and the background
//     index.
//
// There is no GIF testdata in GOROOT, so the corpus is built here: `gif.Encode`/`gif.EncodeAll`
// over generated images for the paths an encoder produces, and hand-assembled byte sequences for
// the ones it never does (GIF87a, a missing colour table, an out-of-bounds frame, every extension
// label, litWidth at its extremes, interlacing, and LZW streams that are truncated, overlong,
// missing their end code or carrying an invalid one).

import (
	"bytes"
	"compress/lzw"
	"fmt"
	"image"
	"image/color"
	"image/gif"
	"io"
)

// --- assembling a GIF by hand ---------------------------------------------------------------

type gifWriter struct{ b bytes.Buffer }

func (g *gifWriter) raw(p ...byte)    { g.b.Write(p) }
func (g *gifWriter) blob(p []byte)    { g.b.Write(p) }
func (g *gifWriter) str(s string)     { g.b.WriteString(s) }
func (g *gifWriter) u16(v int)        { g.b.Write([]byte{byte(v), byte(v >> 8)}) }
func (g *gifWriter) bytes() []byte    { return append([]byte(nil), g.b.Bytes()...) }
func (g *gifWriter) trailer()         { g.raw(0x3B) }
func (g *gifWriter) extension(b byte) { g.raw(0x21, b) }
func (g *gifWriter) subBlocks(p []byte) {
	for len(p) > 255 {
		g.raw(255)
		g.blob(p[:255])
		p = p[255:]
	}
	if len(p) > 0 {
		g.raw(byte(len(p)))
		g.blob(p)
	}
	g.raw(0)
}

// header writes the 13-byte header and logical screen descriptor. gctBits < 0 means no global
// colour table; otherwise the table has 1<<(gctBits+1) entries and must follow immediately.
func (g *gifWriter) header(vers string, w, h, gctBits int, bg, aspect byte) {
	g.str(vers)
	g.u16(w)
	g.u16(h)
	fields := byte(0)
	if gctBits >= 0 {
		fields = 0x80 | byte(gctBits&7)
	}
	g.raw(fields, bg, aspect)
}

// imageDescriptor writes the 10-byte image descriptor. lctBits < 0 means no local colour table.
func (g *gifWriter) imageDescriptor(left, top, w, h, lctBits int, interlace bool) {
	g.raw(0x2C)
	g.u16(left)
	g.u16(top)
	g.u16(w)
	g.u16(h)
	fields := byte(0)
	if lctBits >= 0 {
		fields |= 0x80 | byte(lctBits&7)
	}
	if interlace {
		fields |= 0x40
	}
	g.raw(fields)
}

// gce writes a graphic control extension. transparent < 0 leaves the transparent-colour flag off.
func (g *gifWriter) gce(disposal byte, delay, transparent int) {
	g.extension(0xF9)
	flags := (disposal & 7) << 2
	ti := byte(0)
	if transparent >= 0 {
		flags |= 1
		ti = byte(transparent)
	}
	g.raw(4, flags, byte(delay), byte(delay>>8), ti, 0)
}

// colorTable is a deterministic table of 1<<(bits+1) RGB triples.
func colorTable(bits int, seed uint64) []byte {
	n := 1 << uint(bits+1)
	out := make([]byte, 0, 3*n)
	for i := 0; i < n; i++ {
		out = append(out,
			uint8(hash4(seed, i, 0, 0)>>56),
			uint8(hash4(seed, i, 0, 1)>>56),
			uint8(hash4(seed, i, 0, 2)>>56))
	}
	return out
}

// --- LZW code streams -----------------------------------------------------------------------

// lsbWriter packs codes least-significant-bit first, as the GIF variant of LZW does.
type lsbWriter struct {
	out []byte
	acc uint32
	n   uint
}

func (w *lsbWriter) write(code uint32, width uint) {
	w.acc |= code << w.n
	w.n += width
	for w.n >= 8 {
		w.out = append(w.out, byte(w.acc))
		w.acc >>= 8
		w.n -= 8
	}
}

func (w *lsbWriter) done() []byte {
	if w.n > 0 {
		w.out = append(w.out, byte(w.acc))
	}
	return w.out
}

// lzwEmit writes a code stream by mirroring compress/lzw's *decoder* state machine, so each code
// is packed at exactly the width the decoder will read it with. Mirroring the decoder rather than
// the encoder is what lets a case emit a stream no encoder would: one with no end code, one whose
// code is past `hi`, one with more codes than the frame has pixels. Every stream that is meant to
// be well-formed is round-tripped through compress/lzw in buildLZW before it reaches the corpus.
type lzwEmit struct {
	w                        lsbWriter
	litWidth, width          uint
	clear, eof, hi, overflow uint32
}

func newLZWEmit(litWidth uint) *lzwEmit {
	e := &lzwEmit{litWidth: litWidth, width: litWidth + 1}
	e.clear = 1 << litWidth
	e.eof = e.clear + 1
	e.hi = e.eof
	e.overflow = 1 << e.width
	return e
}

func (e *lzwEmit) emit(code uint32) {
	e.w.write(code, e.width)
	switch code {
	case e.clear:
		e.width = e.litWidth + 1
		e.hi = e.eof
		e.overflow = 1 << e.width
		return
	case e.eof:
		return
	}
	e.hi++
	if e.hi >= e.overflow {
		if e.width == 12 {
			e.hi--
		} else {
			e.width++
			e.overflow = 1 << e.width
		}
	}
}

// literals emits one literal code per index, and the end code when end is true.
func (e *lzwEmit) literals(indices []byte, end bool) []byte {
	for _, i := range indices {
		e.emit(uint32(i))
	}
	if end {
		e.emit(e.eof)
	}
	return e.w.done()
}

// goLZW compresses indices with compress/lzw, the way image/gif's encoder does.
func goLZW(litWidth int, indices []byte) []byte {
	var buf bytes.Buffer
	w := lzw.NewWriter(&buf, lzw.LSB, litWidth)
	if _, err := w.Write(indices); err != nil {
		panic(err)
	}
	if err := w.Close(); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

// checkLZW decodes a code stream and panics unless it yields want. Every hand-built stream that
// claims to be well-formed goes through here, so a mistake in lzwEmit's width schedule is a
// generator crash and not a fixture that records the wrong answer as if it were Go's.
func checkLZW(litWidth int, stream, want []byte) []byte {
	if got := decodeLZW(litWidth, stream); !bytes.Equal(got, want) {
		panic(fmt.Errorf("checkLZW: got %v, want %v", got, want))
	}
	return stream
}

// checkLZWPrefix is checkLZW for a stream with no end code, where the zero bits padding the final
// byte may still form one more code and so one more pixel — which is the point of those cases.
func checkLZWPrefix(litWidth int, stream, want []byte) []byte {
	if got := decodeLZW(litWidth, stream); !bytes.HasPrefix(got, want) {
		panic(fmt.Errorf("checkLZWPrefix: got %v, want prefix %v", got, want))
	}
	return stream
}

func decodeLZW(litWidth int, stream []byte) []byte {
	r := lzw.NewReader(bytes.NewReader(stream), lzw.LSB, litWidth)
	got, err := io.ReadAll(r)
	if err != nil && err != io.ErrUnexpectedEOF {
		panic(fmt.Errorf("decodeLZW: %w", err))
	}
	return got
}

// interlaceOrder reorders a row-major index plane into the four GIF interlace passes.
func interlaceOrder(pix []byte, dx, dy int) []byte {
	var out []byte
	for _, p := range [][2]int{{8, 0}, {8, 4}, {4, 2}, {2, 1}} {
		for y := p[1]; y < dy; y += p[0] {
			out = append(out, pix[y*dx:(y+1)*dx]...)
		}
	}
	return out
}

// indexPlane is a deterministic dx*dy plane of indices below n.
func indexPlane(dx, dy, n int, seed uint64) []byte {
	out := make([]byte, dx*dy)
	for i := range out {
		out[i] = uint8(int(hash4(seed, i, 0, 0)>>56) % n)
	}
	return out
}

// --- the crafted corpus ---------------------------------------------------------------------

func craftedGIFs() []namedFile {
	var files []namedFile
	add := func(name string, data []byte) { files = append(files, namedFile{"crafted_" + name, data}) }

	gct2 := colorTable(1, 900)   // 4 entries
	gct8 := colorTable(2, 901)   // 8 entries
	gct256 := colorTable(7, 902) // 256 entries

	// A 2x2 frame of four distinct indices, compressed by compress/lzw as the encoder would.
	quad := []byte{0, 1, 2, 3}
	quadData := goLZW(2, quad)

	// basic, both versions, with and without a GCE.
	for _, vers := range []string{"GIF87a", "GIF89a"} {
		var g gifWriter
		g.header(vers, 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("basic_"+vers, g.bytes())
	}

	// Every version string the "GIF8?a" magic admits, plus two that it does not: the decoder
	// formats the six bytes with %q, so this is the only place that quoting is observable.
	for _, mid := range []byte{'7', '9', '8', 'a', 0x00, 0x20, 0x22, 0x5c, 0x7f, 0x80, 0xc3, 0xff} {
		var h gifWriter
		h.str("GIF8")
		h.raw(mid, 'a')
		h.u16(2)
		h.u16(2)
		h.raw(0x80|1, 0, 0)
		h.blob(gct2)
		h.imageDescriptor(0, 0, 2, 2, -1, false)
		h.raw(2)
		h.subBlocks(quadData)
		h.trailer()
		add(fmt.Sprintf("version_%02x", mid), h.bytes())
	}
	for _, vers := range []string{"GIF87b", "NOTGIF", "gif89a"} {
		var g gifWriter
		g.header(vers, 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("version_"+vers, g.bytes())
	}

	// Colour tables: no global with a local, no table at all, a local that overrides the global,
	// and every global table size.
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, -1, 0, 0)
		g.imageDescriptor(0, 0, 2, 2, 1, false)
		g.blob(gct2)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("no_gct_local_only", g.bytes())
	}
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, -1, 0, 0)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("no_color_table", g.bytes())
	}
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, 2, false)
		g.blob(gct8)
		g.raw(3)
		g.subBlocks(goLZW(3, quad))
		g.trailer()
		add("local_overrides_global", g.bytes())
	}
	for bits := 0; bits <= 7; bits++ {
		n := 1 << uint(bits+1)
		pix := indexPlane(3, 2, n, uint64(910+bits))
		var g gifWriter
		g.header("GIF89a", 3, 2, bits, byte(n-1), 7)
		g.blob(colorTable(bits, uint64(920+bits)))
		g.imageDescriptor(0, 0, 3, 2, -1, false)
		g.raw(8)
		g.subBlocks(goLZW(8, pix))
		g.trailer()
		add(fmt.Sprintf("gct_bits_%d", bits), g.bytes())
	}
	// A truncated global colour table.
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 7, 0, 0)
		g.blob(gct256[:100])
		add("gct_truncated", g.bytes())
	}

	// Frame bounds. left+width == d.width is the boundary the decoder allows.
	{
		var g gifWriter
		g.header("GIF89a", 4, 4, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(2, 2, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("frame_exactly_fits", g.bytes())
	}
	for _, off := range [][2]int{{3, 2}, {2, 3}} {
		var g gifWriter
		g.header("GIF89a", 4, 4, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(off[0], off[1], 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add(fmt.Sprintf("frame_out_of_bounds_%d_%d", off[0], off[1]), g.bytes())
	}
	// A zero-sized logical screen with a zero-sized frame, and a 1x1 image.
	{
		var g gifWriter
		g.header("GIF89a", 0, 0, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 0, 0, -1, false)
		g.raw(2)
		g.subBlocks(goLZW(2, nil))
		g.trailer()
		add("zero_screen", g.bytes())
	}
	{
		var g gifWriter
		g.header("GIF89a", 1, 1, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 1, 1, -1, false)
		g.raw(2)
		g.subBlocks(goLZW(2, []byte{3}))
		g.trailer()
		add("one_by_one", g.bytes())
	}
	// A 4x4 screen holding a 0x4 frame: an empty Pix, so io.ReadFull never reads a byte and the
	// whole LZW stream is "too much".
	{
		var g gifWriter
		g.header("GIF89a", 4, 4, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 0, 4, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("empty_frame_with_data", g.bytes())
	}
	{
		var g gifWriter
		g.header("GIF89a", 4, 4, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 4, 0, -1, false)
		g.raw(2)
		g.subBlocks(goLZW(2, nil))
		g.trailer()
		add("empty_frame_no_data", g.bytes())
	}

	// litWidth at and outside its range.
	for _, lw := range []int{0, 1, 2, 3, 8, 9, 255} {
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(byte(lw))
		if lw >= 2 && lw <= 8 {
			g.subBlocks(goLZW(lw, quad))
		} else {
			g.subBlocks(quadData)
		}
		g.trailer()
		add(fmt.Sprintf("litwidth_%d", lw), g.bytes())
	}

	// LZW streams the encoder never writes.
	frameWith := func(name string, dx, dy int, litWidth int, data []byte, tail func(*gifWriter)) {
		var g gifWriter
		g.header("GIF89a", dx, dy, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, dx, dy, -1, false)
		g.raw(byte(litWidth))
		if tail == nil {
			g.subBlocks(data)
		} else {
			tail(&g)
		}
		g.trailer()
		add(name, g.bytes())
	}
	// No end code, but exactly enough pixels: accepted (golang.org/issue/9856).
	frameWith("lzw_no_end_code", 2, 2, 2,
		checkLZWPrefix(2, newLZWEmit(2).literals(quad, false), quad), nil)
	// No end code and two pixels short: the padding bits are too few to form another code.
	frameWith("lzw_short_no_end_code", 2, 2, 2,
		checkLZWPrefix(2, newLZWEmit(2).literals(quad[:2], false), quad[:2]), nil)
	// Three codes and no end code: the seven zero bits padding the second byte form a fourth code
	// at the widened code width, so the frame is filled by a pixel no encoder wrote.
	frameWith("lzw_padding_becomes_a_pixel", 2, 2, 2,
		checkLZWPrefix(2, newLZWEmit(2).literals(quad[:3], false), quad[:3]), nil)
	// End code, one pixel short.
	frameWith("lzw_short_with_end_code", 2, 2, 2,
		checkLZW(2, newLZWEmit(2).literals(quad[:3], true), quad[:3]), nil)
	// No codes at all: io.ReadFull reads nothing.
	frameWith("lzw_end_code_only", 2, 2, 2,
		checkLZW(2, newLZWEmit(2).literals(nil, true), nil), nil)
	// More pixels than the frame holds.
	frameWith("lzw_too_much_data", 2, 2, 2,
		checkLZW(2, newLZWEmit(2).literals([]byte{0, 1, 2, 3, 1, 0}, true), []byte{0, 1, 2, 3, 1, 0}), nil)
	// One pixel too many, with no end code.
	frameWith("lzw_one_too_many", 2, 2, 2,
		checkLZWPrefix(2, newLZWEmit(2).literals([]byte{0, 1, 2, 3, 2}, false), []byte{0, 1, 2, 3, 2}), nil)
	// A code past hi: 7 with litWidth 2 (clear 4, eof 5, hi 5).
	{
		e := newLZWEmit(2)
		e.emit(0)
		e.emit(7)
		frameWith("lzw_invalid_code", 2, 2, 2, e.w.done(), nil)
	}
	// A clear code in the middle, which resets the width and the table.
	{
		e := newLZWEmit(2)
		for _, c := range []uint32{0, 1, 4 /* clear */, 2, 3} {
			e.emit(c)
		}
		e.emit(e.eof)
		frameWith("lzw_clear_midstream", 2, 2, 2, checkLZW(2, e.w.done(), quad), nil)
	}
	// A clear code first, which is what most encoders write.
	{
		e := newLZWEmit(2)
		e.emit(4)
		for _, c := range []uint32{0, 1, 2, 3} {
			e.emit(c)
		}
		e.emit(e.eof)
		frameWith("lzw_leading_clear", 2, 2, 2, checkLZW(2, e.w.done(), quad), nil)
	}
	// Truncated sub-block stream: the block length byte promises more than the file holds.
	frameWith("lzw_truncated_subblock", 2, 2, 2, nil, func(g *gifWriter) {
		g.raw(10, 0x44, 0x01)
	})
	// A frame whose data ends with no terminator at all.
	frameWith("lzw_no_terminator", 2, 2, 2, nil, func(g *gifWriter) {
		g.raw(byte(len(quadData)))
		g.blob(quadData)
	})
	// The block-terminator accommodations of golang.org/issue/16146.
	{
		mk := func(name string, extra ...[]byte) {
			var g gifWriter
			g.header("GIF89a", 2, 2, 1, 0, 0)
			g.blob(gct2)
			g.imageDescriptor(0, 0, 2, 2, -1, false)
			g.raw(2)
			g.raw(byte(len(quadData)))
			g.blob(quadData)
			for _, e := range extra {
				g.raw(byte(len(e)))
				g.blob(e)
			}
			g.raw(0)
			g.trailer()
			add(name, g.bytes())
		}
		mk("tail_one_byte_subblock", []byte{0x99})
		mk("tail_two_byte_subblock", []byte{0x99, 0x98})
		mk("tail_two_one_byte_subblocks", []byte{0x99}, []byte{0x98})
		// A junk byte *inside* the last sub-block, so the LZW reader leaves it buffered.
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.raw(byte(len(quadData) + 1))
		g.blob(quadData)
		g.raw(0x99)
		g.raw(0)
		g.trailer()
		add("tail_padded_last_subblock", g.bytes())
		var h gifWriter
		h.header("GIF89a", 2, 2, 1, 0, 0)
		h.blob(gct2)
		h.imageDescriptor(0, 0, 2, 2, -1, false)
		h.raw(2)
		h.raw(byte(len(quadData) + 1))
		h.blob(quadData)
		h.raw(0x99)
		h.raw(1, 0x98)
		h.raw(0)
		h.trailer()
		add("tail_padded_plus_subblock", h.bytes())
	}
	// Image data spanning several 255-byte sub-blocks.
	{
		pix := indexPlane(40, 40, 256, 930)
		var g gifWriter
		g.header("GIF89a", 40, 40, 7, 0, 0)
		g.blob(gct256)
		g.imageDescriptor(0, 0, 40, 40, -1, false)
		g.raw(8)
		g.subBlocks(goLZW(8, pix))
		g.trailer()
		add("multi_subblock", g.bytes())
	}
	// Saturated at the maximum code width, with no clear code anywhere.
	//
	// compress/lzw's *encoder* emits a clear code the moment its table fills, so a stream it
	// produced leaves the decoder at `hi == overflow && width == maxWidth` for exactly one code.
	// This one never clears: after 3839 literal codes the width reaches 12 and the table stops
	// growing, and the decoder then spends 36000 codes in the branch that sets `last` back to
	// `decoderInvalidCode` and undoes the `hi++`. A decoder that does not undo it walks `hi` past
	// the 4096-entry table and, eventually, past a uint16 — which no shorter stream can show,
	// because every code is readable at width 12 and so nothing is ever "invalid" there.
	{
		const dx, dy = 200, 200
		pix := indexPlane(dx, dy, 256, 933)
		e := newLZWEmit(8)
		var g gifWriter
		g.header("GIF89a", dx, dy, 7, 0, 0)
		g.blob(gct256)
		g.imageDescriptor(0, 0, dx, dy, -1, false)
		g.raw(8)
		g.subBlocks(checkLZW(8, e.literals(pix, true), pix))
		g.trailer()
		add("lzw_saturated_max_width", g.bytes())
	}
	// Long enough for the LZW code width to reach its 12-bit maximum and stay there.
	{
		pix := indexPlane(128, 128, 256, 931)
		var g gifWriter
		g.header("GIF89a", 128, 128, 7, 0, 0)
		g.blob(gct256)
		g.imageDescriptor(0, 0, 128, 128, -1, false)
		g.raw(8)
		g.subBlocks(goLZW(8, pix))
		g.trailer()
		add("lzw_max_width", g.bytes())
	}

	// Pixel values outside the palette. With a 256-entry palette the decoder skips the check.
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(3)
		g.subBlocks(goLZW(3, []byte{0, 1, 4, 3}))
		g.trailer()
		add("pixel_out_of_palette", g.bytes())
	}
	{
		// Exactly at the end of the palette: index 3 in a four-entry table is valid.
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(3)
		g.subBlocks(goLZW(3, []byte{0, 1, 3, 3}))
		g.trailer()
		add("pixel_at_palette_end", g.bytes())
	}
	{
		pix := indexPlane(4, 4, 256, 932)
		var g gifWriter
		g.header("GIF89a", 4, 4, 7, 0, 0)
		g.blob(gct256)
		g.imageDescriptor(0, 0, 4, 4, -1, false)
		g.raw(8)
		g.subBlocks(goLZW(8, pix))
		g.trailer()
		add("full_palette_no_check", g.bytes())
	}

	// Interlacing, at heights that exercise every pass and none.
	for _, dy := range []int{1, 2, 3, 5, 8, 9, 17} {
		const dx = 5
		pix := indexPlane(dx, dy, 4, uint64(940+dy))
		var g gifWriter
		g.header("GIF89a", dx, dy, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, dx, dy, -1, true)
		g.raw(2)
		g.subBlocks(goLZW(2, interlaceOrder(pix, dx, dy)))
		g.trailer()
		add(fmt.Sprintf("interlaced_%d", dy), g.bytes())
	}

	// Graphic control extensions.
	gceFrame := func(name string, disposal byte, delay, transparent int, size, term byte) {
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.extension(0xF9)
		flags := (disposal & 7) << 2
		ti := byte(0)
		if transparent >= 0 {
			flags |= 1
			ti = byte(transparent)
		}
		g.raw(size, flags, byte(delay), byte(delay>>8), ti, term)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add(name, g.bytes())
	}
	for d := byte(0); d <= 7; d++ {
		gceFrame(fmt.Sprintf("gce_disposal_%d", d), d, 0, -1, 4, 0)
	}
	gceFrame("gce_delay", 1, 0x1234, -1, 4, 0)
	gceFrame("gce_transparent_0", 0, 5, 0, 4, 0)
	gceFrame("gce_transparent_3", 0, 5, 3, 4, 0)
	// ti == len(palette): the first index the enlarging branch has to cover.
	gceFrame("gce_transparent_at_len", 0, 5, 4, 4, 0)
	gceFrame("gce_transparent_oob_7", 0, 5, 7, 4, 0)
	gceFrame("gce_transparent_oob_255", 0, 5, 255, 4, 0)
	gceFrame("gce_bad_size", 0, 0, -1, 5, 0)
	gceFrame("gce_bad_terminator", 0, 0, -1, 4, 3)
	// A transparent index with a *local* colour table: the table is mutated in place, not cloned.
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.gce(0, 0, 2)
		g.imageDescriptor(0, 0, 2, 2, 2, false)
		g.blob(gct8)
		g.raw(3)
		g.subBlocks(goLZW(3, quad))
		g.trailer()
		add("gce_transparent_local_table", g.bytes())
	}
	// Two frames, the first transparent through the global table: the second must see the global
	// table unchanged, and must not inherit hasTransparentIndex — but *does* inherit disposal.
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.gce(3, 7, 1)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("gce_scope_is_one_frame", g.bytes())
	}
	// A GCE whose extension block has extra sub-blocks after the six bytes: the decoder reads the
	// six and returns, so the next byte read is the sub-block length, not a section indicator.
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.raw(0x21, 0xF9, 4, 0, 0, 0, 0, 0)
		g.raw(0) // the terminator a stricter reader would consume as part of the extension
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add("gce_then_zero_byte", g.bytes())
	}

	// Extensions: comment, plain text, application (NETSCAPE and not), unknown labels.
	extFrame := func(name string, write func(*gifWriter)) {
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		write(&g)
		g.imageDescriptor(0, 0, 2, 2, -1, false)
		g.raw(2)
		g.subBlocks(quadData)
		g.trailer()
		add(name, g.bytes())
	}
	extFrame("ext_comment", func(g *gifWriter) {
		g.extension(0xFE)
		g.subBlocks([]byte("a comment, long enough to need one sub-block"))
	})
	extFrame("ext_comment_empty", func(g *gifWriter) {
		g.extension(0xFE)
		g.raw(0)
	})
	extFrame("ext_comment_many_blocks", func(g *gifWriter) {
		g.extension(0xFE)
		g.subBlocks(bytes.Repeat([]byte("x"), 600))
	})
	extFrame("ext_plain_text", func(g *gifWriter) {
		g.extension(0x01)
		g.raw(12, 0, 0, 0, 0, 0, 10, 0, 10, 1, 1, 1, 1) // 13 bytes, the first being the block size
		g.subBlocks([]byte("hello"))
	})
	extFrame("ext_netscape_loop_7", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("NETSCAPE2.0")
		g.raw(3, 1, 7, 0)
		g.raw(0)
	})
	extFrame("ext_netscape_loop_0", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("NETSCAPE2.0")
		g.raw(3, 1, 0, 0)
		g.raw(0)
	})
	extFrame("ext_netscape_loop_big", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("NETSCAPE2.0")
		g.raw(3, 1, 0x34, 0x12)
		g.raw(0)
	})
	extFrame("ext_netscape_empty_block", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("NETSCAPE2.0")
		g.raw(0)
	})
	extFrame("ext_netscape_block_len_2", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("NETSCAPE2.0")
		g.raw(2, 1, 9)
		g.raw(0)
	})
	extFrame("ext_netscape_first_byte_2", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("NETSCAPE2.0")
		g.raw(3, 2, 7, 0)
		g.raw(0)
	})
	extFrame("ext_app_other", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(11)
		g.str("OTHERAPP1.0")
		g.raw(3, 1, 7, 0)
		g.raw(0)
	})
	// Adobe's ten-byte application identifier, which the spec forbids and the decoder allows.
	extFrame("ext_app_size_10", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(10)
		g.str("NETSCAPE2")
		g.raw(3, 1, 7, 0)
		g.raw(0)
	})
	// A twelve-byte identifier whose first eleven bytes are NETSCAPE2.0: the comparison is over
	// all `size` bytes, so this is not a loop extension.
	extFrame("ext_app_size_12", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(12)
		g.str("NETSCAPE2.0!")
		g.raw(3, 1, 7, 0)
		g.raw(0)
	})
	extFrame("ext_app_size_0", func(g *gifWriter) {
		g.extension(0xFF)
		g.raw(0)
		g.raw(0)
	})
	for _, label := range []byte{0x00, 0x02, 0x80, 0xF8, 0xFA, 0xFD} {
		l := label
		extFrame(fmt.Sprintf("ext_unknown_%02x", l), func(g *gifWriter) {
			g.extension(l)
			g.raw(1, 0, 0)
		})
	}

	// Extensions cut short, which is the only way to reach the decoder's extension-read errors.
	truncAfterHeader := func(name string, write func(*gifWriter)) {
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		write(&g)
		add(name, g.bytes())
	}
	truncAfterHeader("ext_label_missing", func(g *gifWriter) { g.raw(0x21) })
	truncAfterHeader("ext_app_size_missing", func(g *gifWriter) { g.raw(0x21, 0xFF) })
	truncAfterHeader("ext_app_id_short", func(g *gifWriter) { g.raw(0x21, 0xFF, 11); g.str("NETS") })
	truncAfterHeader("ext_app_no_blocks", func(g *gifWriter) { g.raw(0x21, 0xFF, 11); g.str("NETSCAPE2.0") })
	truncAfterHeader("ext_netscape_block_short", func(g *gifWriter) {
		g.raw(0x21, 0xFF, 11)
		g.str("NETSCAPE2.0")
		g.raw(3, 1)
	})
	truncAfterHeader("ext_comment_block_short", func(g *gifWriter) { g.raw(0x21, 0xFE, 5, 'a', 'b') })
	truncAfterHeader("ext_comment_no_terminator", func(g *gifWriter) { g.raw(0x21, 0xFE, 2, 'a', 'b') })
	truncAfterHeader("ext_plain_text_short", func(g *gifWriter) { g.raw(0x21, 0x01, 12, 0, 0, 0) })
	truncAfterHeader("gce_truncated", func(g *gifWriter) { g.raw(0x21, 0xF9, 4, 0, 0) })
	truncAfterHeader("gce_label_only", func(g *gifWriter) { g.raw(0x21, 0xF9) })
	// A local colour table the file is too short to hold.
	truncAfterHeader("lct_truncated", func(g *gifWriter) {
		g.imageDescriptor(0, 0, 2, 2, 7, false)
		g.blob(gct256[:60])
	})
	// An image descriptor cut short.
	truncAfterHeader("descriptor_truncated", func(g *gifWriter) { g.raw(0x2C, 0, 0, 0, 0) })
	// The litWidth byte missing.
	truncAfterHeader("litwidth_missing", func(g *gifWriter) { g.imageDescriptor(0, 0, 2, 2, -1, false) })

	// Section indicators the decoder does not know, and a missing image.
	for _, c := range []byte{0x00, 0x2B, 0x3A, 0x3C, 0xFF} {
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.raw(c)
		add(fmt.Sprintf("block_type_%02x", c), g.bytes())
	}
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		g.trailer()
		add("missing_image_data", g.bytes())
	}
	{
		var g gifWriter
		g.header("GIF89a", 2, 2, 1, 0, 0)
		g.blob(gct2)
		add("header_only", g.bytes())
	}

	// Several frames: Decode stops after the first, DecodeAll walks them all. The second frame of
	// the "broken" and "no trailer" files is where the two answers part company.
	{
		frames := func(g *gifWriter, n int) {
			for i := 0; i < n; i++ {
				g.gce(byte(i%4), 10*i, -1)
				g.imageDescriptor(i%2, i%2, 2, 2, -1, false)
				g.raw(2)
				g.subBlocks(goLZW(2, indexPlane(2, 2, 4, uint64(950+i))))
			}
		}
		var g gifWriter
		g.header("GIF89a", 4, 4, 1, 2, 0)
		g.blob(gct2)
		frames(&g, 6)
		g.trailer()
		add("six_frames", g.bytes())

		var h gifWriter
		h.header("GIF89a", 4, 4, 1, 2, 0)
		h.blob(gct2)
		frames(&h, 1)
		h.imageDescriptor(0, 0, 2, 2, -1, false)
		h.raw(2)
		h.raw(3, 0x44, 0x01) // a sub-block that promises three bytes and delivers two
		add("second_frame_broken", h.bytes())

		var k gifWriter
		k.header("GIF89a", 4, 4, 1, 2, 0)
		k.blob(gct2)
		frames(&k, 2)
		add("frames_no_trailer", k.bytes())

		var m gifWriter
		m.header("GIF89a", 4, 4, 1, 2, 0)
		m.blob(gct2)
		frames(&m, 2)
		m.raw(0x2C, 0, 0) // a third descriptor, cut short
		add("third_descriptor_truncated", m.bytes())
	}

	// A logical screen far larger than the data: the decoder allocates the frame from the
	// descriptor before it reads a pixel, exactly as Go does.
	{
		var g gifWriter
		g.header("GIF89a", 4000, 4000, 1, 0, 0)
		g.blob(gct2)
		g.imageDescriptor(0, 0, 4000, 4000, -1, false)
		g.raw(2)
		g.raw(2, 0x44, 0x01)
		g.raw(0)
		g.trailer()
		add("big_frame_truncated", g.bytes())
	}
	return files
}

// --- the generated corpus -------------------------------------------------------------------

func gifCorpus() []namedFile {
	var files []namedFile

	// Single frames through gif.Encode, over paletted inputs (kept verbatim) and over inputs the
	// encoder must quantise onto the Plan 9 palette.
	seed := uint64(7000)
	for _, sz := range [][2]int{{1, 1}, {2, 3}, {7, 5}, {33, 17}, {120, 100}} {
		for _, n := range []int{1, 2, 3, 16, 200, 256} {
			spec := imgSpec{"paletted", sz[0], sz[1], "noise", "opaque", seed, n, ""}
			seed++
			var buf bytes.Buffer
			if err := gif.Encode(&buf, spec.build(), nil); err != nil {
				panic(err)
			}
			files = append(files, namedFile{
				fmt.Sprintf("go_paletted_%dx%d_p%d", sz[0], sz[1], n), buf.Bytes()})
		}
		spec := imgSpec{"paletted", sz[0], sz[1], "blocks", "mixed", seed, 64, ""}
		seed++
		var tb bytes.Buffer
		if err := gif.Encode(&tb, spec.build(), nil); err != nil {
			panic(err)
		}
		files = append(files, namedFile{
			fmt.Sprintf("go_paletted_alpha_%dx%d", sz[0], sz[1]), tb.Bytes()})
	}
	for _, kind := range []string{"nrgba", "gray", "ycbcr"} {
		spec := imgSpec{kind, 24, 18, "smooth", "opaque", seed, 0, "420"}
		seed++
		for _, n := range []int{4, 64, 256} {
			var buf bytes.Buffer
			if err := gif.Encode(&buf, spec.build(), &gif.Options{NumColors: n}); err != nil {
				panic(err)
			}
			files = append(files, namedFile{fmt.Sprintf("go_quantised_%s_%d", kind, n), buf.Bytes()})
		}
	}

	// Animations through gif.EncodeAll: frames with differing bounds, delays, the three disposal
	// methods, a loop count, a background index and a global palette.
	pal := func(n int, s uint64) color.Palette {
		p := make(color.Palette, n)
		for i := range p {
			p[i] = color.RGBA{
				uint8(hash4(s, i, 0, 0) >> 56),
				uint8(hash4(s, i, 0, 1) >> 56),
				uint8(hash4(s, i, 0, 2) >> 56), 0xFF}
		}
		return p
	}
	frame := func(r image.Rectangle, p color.Palette, s uint64) *image.Paletted {
		m := image.NewPaletted(r, p)
		for i := range m.Pix {
			m.Pix[i] = uint8(int(hash4(s, i, 0, 0)>>56) % len(p))
		}
		return m
	}
	encodeAll := func(name string, g *gif.GIF) {
		var buf bytes.Buffer
		if err := gif.EncodeAll(&buf, g); err != nil {
			panic(fmt.Errorf("%s: %w", name, err))
		}
		files = append(files, namedFile{name, buf.Bytes()})
	}
	{
		p := pal(16, 8000)
		g := &gif.GIF{
			Image: []*image.Paletted{
				frame(image.Rect(0, 0, 12, 9), p, 8001),
				frame(image.Rect(2, 1, 10, 7), p, 8002),
				frame(image.Rect(0, 0, 1, 1), p, 8003),
				frame(image.Rect(6, 4, 12, 9), p, 8004),
			},
			Delay:           []int{0, 5, 100, 4660},
			Disposal:        []byte{gif.DisposalNone, gif.DisposalBackground, gif.DisposalPrevious, 0},
			LoopCount:       3,
			Config:          image.Config{ColorModel: p, Width: 12, Height: 9},
			BackgroundIndex: 5,
		}
		encodeAll("go_anim_disposals", g)
	}
	{
		// Per-frame palettes, so every frame carries a local colour table.
		g := &gif.GIF{
			Image: []*image.Paletted{
				frame(image.Rect(0, 0, 8, 8), pal(4, 8100), 8101),
				frame(image.Rect(0, 0, 8, 8), pal(8, 8102), 8103),
				frame(image.Rect(1, 1, 5, 5), pal(256, 8104), 8105),
			},
			Delay:     []int{1, 2, 3},
			LoopCount: 0,
		}
		encodeAll("go_anim_local_tables", g)
	}
	{
		// LoopCount -1: show each frame once, which the encoder writes as no loop extension.
		p := pal(4, 8200)
		g := &gif.GIF{
			Image:     []*image.Paletted{frame(image.Rect(0, 0, 6, 6), p, 8201), frame(image.Rect(0, 0, 6, 6), p, 8202)},
			Delay:     []int{7, 7},
			LoopCount: -1,
		}
		encodeAll("go_anim_loop_minus_one", g)
	}
	{
		// A transparent palette entry, which makes the encoder write a graphic control extension
		// with the transparent-colour flag set.
		p := pal(8, 8300)
		p[3] = color.RGBA{}
		g := &gif.GIF{
			Image:     []*image.Paletted{frame(image.Rect(0, 0, 7, 7), p, 8301), frame(image.Rect(0, 0, 7, 7), p, 8302)},
			Delay:     []int{0, 0},
			Disposal:  []byte{gif.DisposalBackground, gif.DisposalBackground},
			LoopCount: 2,
		}
		encodeAll("go_anim_transparent", g)
	}
	{
		p := pal(64, 8400)
		imgs := make([]*image.Paletted, 20)
		delays := make([]int, 20)
		for i := range imgs {
			imgs[i] = frame(image.Rect(0, 0, 16, 16), p, uint64(8401+i))
			delays[i] = i
		}
		encodeAll("go_anim_twenty_frames", &gif.GIF{Image: imgs, Delay: delays, LoopCount: 1})
	}

	files = append(files, craftedGIFs()...)

	// Damaged copies of a real animation: truncations at every structural boundary, and bit flips.
	var base []byte
	for _, f := range files {
		if f.Name == "go_anim_disposals" {
			base = f.Data
		}
	}
	for _, cut := range []int{0, 1, 3, 5, 6, 7, 10, 12, 13, 14, 20, 50, len(base) / 3, len(base) / 2, len(base) - 2, len(base) - 1} {
		if cut >= 0 && cut < len(base) {
			files = append(files, namedFile{"cut_" + itoa(cut), base[:cut]})
		}
	}
	for _, pos := range []int{0, 4, 6, 10, 11, 12, 13, 20, 60, len(base) / 3, len(base) / 2, len(base) - 3, len(base) - 1} {
		if pos < 0 || pos >= len(base) {
			continue
		}
		bad := bytes.Clone(base)
		bad[pos] ^= 0x55
		files = append(files, namedFile{"flip_" + itoa(pos), bad})
	}
	files = append(files, namedFile{"trailing_garbage", append(bytes.Clone(base), "garbage"...)})
	return files
}

// --- the stage ------------------------------------------------------------------------------

// gifAllCase records what gif.DecodeConfig, gif.Decode and gif.DecodeAll answer for one corpus
// file, called directly rather than through the registry. The bytes live in the matching entry of
// the "decode" array, which this array parallels name for name.
type gifAllCase struct {
	Name string `json:"name"`
	// gif.DecodeConfig: {"w","h","model"} or {"err"}.
	Config map[string]any `json:"config"`
	// gif.Decode: describe(m) or {"err"} — the first frame only.
	Decode map[string]any `json:"decode"`
	// gif.DecodeAll: {"loop_count","background_index","w","h","model","frames"} or {"err"}.
	All map[string]any `json:"all"`
}

func gifConfigOf(data []byte) map[string]any {
	cfg, err := gif.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	return map[string]any{"w": cfg.Width, "h": cfg.Height, "model": modelName(cfg.ColorModel)}
}

func gifDecodeOf(data []byte) map[string]any {
	m, err := gif.Decode(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	return describe(m)
}

func gifDecodeAllOf(data []byte) map[string]any {
	g, err := gif.DecodeAll(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	frames := make([]map[string]any, len(g.Image))
	for i, m := range g.Image {
		d := describe(m)
		d["delay"] = g.Delay[i]
		d["disposal"] = g.Disposal[i]
		frames[i] = d
	}
	return map[string]any{
		"loop_count":       g.LoopCount,
		"background_index": g.BackgroundIndex,
		"w":                g.Config.Width,
		"h":                g.Config.Height,
		"model":            modelName(g.Config.ColorModel),
		"frames":           frames,
	}
}

func imagingGIFStage() (map[string]any, error) {
	corpus := gifCorpus()
	seen := map[string]bool{}
	dec := make([]decodeCase, 0, len(corpus))
	all := make([]gifAllCase, 0, len(corpus))
	for _, f := range corpus {
		if seen[f.Name] {
			return nil, fmt.Errorf("duplicate corpus name %q", f.Name)
		}
		seen[f.Name] = true
		dec = append(dec, decodeCase{Name: f.Name, B64: b64(f.Data), Config: configOf(f.Data), Image: imageOf(f.Data)})
		all = append(all, gifAllCase{
			Name:   f.Name,
			Config: gifConfigOf(f.Data),
			Decode: gifDecodeOf(f.Data),
			All:    gifDecodeAllOf(f.Data),
		})
	}
	return map[string]any{"decode": dec, "decode_all": all}, nil
}
