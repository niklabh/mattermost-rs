package main

// The codec stages of the imaging oracle: compress/flate + compress/zlib, image/png, image/jpeg.
// See behaviour_imaging.go for the generator and the conventions.

import (
	"bytes"
	"compress/flate"
	"compress/zlib"
	"encoding/binary"
	"fmt"
	"hash/adler32"
	"hash/crc32"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"sort"
)

// --- flate --------------------------------------------------------------------------------------

type streamSpec struct {
	Kind string `json:"kind"` // zeros noise text runs small rows
	Len  int    `json:"len"`
	Seed uint64 `json:"seed"`
}

var streamWords = []string{
	"the", "channel", "mattermost", "post", "a", "of", "image", "preview",
	"thumbnail", "and", "user", "team", "to", "in", "file", "is",
}

func (s streamSpec) build() []byte {
	out := make([]byte, 0, s.Len)
	switch s.Kind {
	case "zeros":
		out = out[:s.Len]
	case "noise":
		for i := 0; i < s.Len; i++ {
			out = append(out, uint8(hash4(s.Seed, i, 0, 0)>>56))
		}
	case "small":
		for i := 0; i < s.Len; i++ {
			out = append(out, 'a'+uint8(hash4(s.Seed, i, 0, 0)>>62))
		}
	case "text":
		for n := 0; len(out) < s.Len; n++ {
			out = append(out, streamWords[hash4(s.Seed, n, 0, 0)>>60]...)
			out = append(out, ' ')
		}
		out = out[:s.Len]
	case "runs":
		for n := 0; len(out) < s.Len; n++ {
			b := uint8(hash4(s.Seed, n, 0, 0) >> 56)
			run := 1 + int(hash4(s.Seed, n, 1, 0)>>58)
			for j := 0; j < run; j++ {
				out = append(out, b)
			}
		}
		out = out[:s.Len]
	case "records":
		// A fixed 4-byte prefix and one noise byte per 5-byte record: every prefix hashes alike,
		// so the hash chain is longer than level 9's 4096 candidates and no match ever reaches
		// "nice" — the chain limit is what decides which match wins.
		for i := 0; i < s.Len; i++ {
			if i%5 == 4 {
				out = append(out, uint8(hash4(s.Seed, i, 0, 0)>>56))
			} else {
				out = append(out, "mmrs"[i%5])
			}
		}
	case "rows":
		for i := 0; i < s.Len; i++ {
			out = append(out, sample("blocks", s.Seed, (i/3)%97, i/291, i%3, 97, 1000))
		}
	default:
		panic("unknown stream kind " + s.Kind)
	}
	return out
}

type flateCase struct {
	Input streamSpec `json:"input"`
	// The deflate level passed to flate.NewWriter (or zlib.NewWriterLevel).
	Level int `json:"level"`
	// "flate" or "zlib".
	Wrapper string `json:"wrapper"`
	// Write the input in pieces of this size (0: one Write).
	Chunk  int            `json:"chunk"`
	Input_ string         `json:"input_sha256"`
	Output map[string]any `json:"output"`
}

func deflateWith(wrapper string, level, chunk int, in []byte) []byte {
	var buf bytes.Buffer
	var w io.WriteCloser
	var err error
	if wrapper == "zlib" {
		w, err = zlib.NewWriterLevel(&buf, level)
	} else {
		w, err = flate.NewWriter(&buf, level)
	}
	if err != nil {
		panic(err)
	}
	rest := in
	for len(rest) > 0 {
		n := len(rest)
		if chunk > 0 && chunk < n {
			n = chunk
		}
		if _, err := w.Write(rest[:n]); err != nil {
			panic(err)
		}
		rest = rest[n:]
	}
	if err := w.Close(); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

// inflateCase is a stream fed to flate.NewReader (or zlib.NewReader) and read to EOF.
type inflateCase struct {
	Name    string `json:"name"`
	Wrapper string `json:"wrapper"`
	B64     string `json:"b64"`
	// What io.ReadAll returned: the bytes produced before the error, and the error text.
	OutSHA string `json:"out_sha256"`
	OutLen int    `json:"out_len"`
	Err    any    `json:"err"`
}

func inflateWith(wrapper string, data []byte) ([]byte, error) {
	var r io.Reader
	if wrapper == "zlib" {
		zr, err := zlib.NewReader(bytes.NewReader(data))
		if err != nil {
			return nil, err
		}
		r = zr
	} else {
		r = flate.NewReader(bytes.NewReader(data))
	}
	return io.ReadAll(r)
}

// bitWriter builds hand-made deflate streams for the inflate corpus, LSB first as RFC 1951 says.
type bitWriter struct {
	out   []byte
	acc   uint32
	nbits uint
}

func (b *bitWriter) bits(v uint32, n uint) {
	b.acc |= v << b.nbits
	b.nbits += n
	for b.nbits >= 8 {
		b.out = append(b.out, byte(b.acc))
		b.acc >>= 8
		b.nbits -= 8
	}
}

// huff writes a Huffman code, which RFC 1951 packs MSB first.
func (b *bitWriter) huff(code uint32, n uint) {
	var rev uint32
	for i := uint(0); i < n; i++ {
		rev = rev<<1 | (code>>i)&1
	}
	b.bits(rev, n)
}

func (b *bitWriter) done() []byte {
	if b.nbits > 0 {
		b.out = append(b.out, byte(b.acc))
	}
	return b.out
}

// fixedLit writes literal/length symbol s with the fixed Huffman code.
func (b *bitWriter) fixedLit(s int) {
	switch {
	case s < 144:
		b.huff(uint32(0x30+s), 8)
	case s < 256:
		b.huff(uint32(0x190+s-144), 9)
	case s < 280:
		b.huff(uint32(s-256), 7)
	default:
		b.huff(uint32(0xc0+s-280), 8)
	}
}

func handMadeInflateCorpus() []namedFile {
	var files []namedFile
	add := func(name string, data []byte) { files = append(files, namedFile{name, data}) }

	// A fixed-Huffman block: "ab", then a copy of length 3 at distance 2, then end.
	{
		var b bitWriter
		b.bits(1, 1) // BFINAL
		b.bits(1, 2) // fixed
		b.fixedLit('a')
		b.fixedLit('b')
		b.fixedLit(257) // length 3
		b.huff(1, 5)    // distance code 1 = distance 2
		b.fixedLit(256)
		add("fixed_ok", b.done())
	}
	// The same copy reaching back further than anything written.
	{
		var b bitWriter
		b.bits(1, 1)
		b.bits(1, 2)
		b.fixedLit('a')
		b.fixedLit(257)
		b.huff(4, 5) // distance code 4 = distance 5..6
		b.bits(0, 1)
		b.fixedLit(256)
		add("fixed_distance_too_far", b.done())
	}
	// Distance codes 30 and 31 are reserved.
	{
		var b bitWriter
		b.bits(1, 1)
		b.bits(1, 2)
		b.fixedLit('a')
		b.fixedLit(257)
		b.huff(30, 5)
		b.fixedLit(256)
		add("fixed_distance_code_30", b.done())
	}
	// Length codes 286 and 287 are reserved.
	{
		var b bitWriter
		b.bits(1, 1)
		b.bits(1, 2)
		b.fixedLit('a')
		b.fixedLit(286)
		b.fixedLit(256)
		add("fixed_length_code_286", b.done())
	}
	// Block type 3 is reserved.
	{
		var b bitWriter
		b.bits(1, 1)
		b.bits(3, 2)
		add("block_type_3", b.done())
	}
	// A stored block whose NLEN is not ^LEN.
	add("stored_bad_nlen", []byte{0x01, 0x03, 0x00, 0x00, 0x00, 'a', 'b', 'c'})
	add("stored_ok", []byte{0x01, 0x03, 0x00, 0xfc, 0xff, 'a', 'b', 'c'})
	add("stored_short", []byte{0x01, 0x03, 0x00, 0xfc, 0xff, 'a'})
	add("stored_empty_final", []byte{0x01, 0x00, 0x00, 0xff, 0xff})
	add("not_final_then_eof", []byte{0x00, 0x00, 0x00, 0xff, 0xff})
	add("empty", nil)
	// Dynamic headers.
	dyn := func(name string, hlit, hdist int, clens []int, body func(*bitWriter)) {
		var b bitWriter
		b.bits(1, 1)
		b.bits(2, 2)
		b.bits(uint32(hlit-257), 5)
		b.bits(uint32(hdist-1), 5)
		b.bits(uint32(len(clens)-4), 4)
		for _, l := range clens {
			b.bits(uint32(l), 3)
		}
		body(&b)
		add(name, b.done())
	}
	// Code-length alphabet with every length 0: no codes at all.
	dyn("dynamic_empty_codelen_code", 257, 1, []int{0, 0, 0, 0}, func(*bitWriter) {})
	// Over-subscribed code-length code: four symbols of length 1.
	dyn("dynamic_oversubscribed_codelen", 257, 1, []int{1, 1, 1, 1}, func(*bitWriter) {})
	// Code-length code: symbols 16,17,18,0 order... give 18 and 0 length 1 (complete), then use a
	// repeat-previous (16) with no previous length.
	dyn("dynamic_repeat_with_no_previous", 257, 1, []int{1, 0, 1, 1}, func(b *bitWriter) {
		// order is 16,17,18,0: 16->len1, 17->0, 18->len1, 0->len1: three codes of length 1 is
		// over-subscribed, so this is rejected as a bad header before any repeat is read.
	})
	dyn("dynamic_16_first", 257, 1, []int{1, 0, 0, 1}, func(b *bitWriter) {
		// 16 and 0 each have length 1: 0 is code 0, 16 is code 1 (codes assigned by symbol value).
		b.huff(1, 1) // 16 with nothing before it
		b.bits(0, 2)
	})
	return files
}

func imagingFlateStage() (map[string]any, error) {
	var cases []flateCase
	add := func(in streamSpec, level int, wrapper string, chunk int) {
		data := in.build()
		cases = append(cases, flateCase{
			Input: in, Level: level, Wrapper: wrapper, Chunk: chunk,
			Input_: sha(data),
			Output: encoded(deflateWith(wrapper, level, chunk, data)),
		})
	}
	kinds := []string{"zeros", "noise", "text", "runs", "small", "rows"}
	lens := []int{0, 1, 3, 4, 5, 17, 100, 258, 259, 1000, 4096, 16384, 32768, 65535, 65536, 65537, 100000, 262144}
	seed := uint64(1)
	for _, k := range kinds {
		for _, n := range lens {
			add(streamSpec{k, n, seed}, 9, "flate", 0)
			seed++
		}
	}
	for _, k := range kinds {
		add(streamSpec{k, 1 << 20, seed}, 9, "flate", 0)
		seed++
	}
	// Other levels: 0 (stored), 2 and 3 (the fastSkipHashing path), 4-8 (lazy with other
	// parameters), -1 (default = 6).
	for _, level := range []int{0, 2, 3, 4, 5, 6, 7, 8, -1} {
		for _, k := range []string{"noise", "text", "runs", "rows"} {
			for _, n := range []int{5, 1000, 70000, 300000} {
				add(streamSpec{k, n, seed}, level, "flate", 0)
				seed++
			}
		}
	}
	// Chain-limited streams: level 8's 1024 and level 9's 4096 candidates are both exhausted.
	for _, level := range []int{4, 6, 8, 9} {
		for _, n := range []int{70000, 200000} {
			add(streamSpec{"records", n, seed}, level, "flate", 0)
			seed++
		}
	}
	// Chunked writes, which is how the PNG encoder feeds the deflater (one row per Write).
	for _, chunk := range []int{1, 7, 97, 291, 4000, 70000} {
		for _, k := range []string{"text", "rows", "noise"} {
			add(streamSpec{k, 200000, seed}, 9, "flate", chunk)
			seed++
		}
	}
	// The zlib wrapper: header byte per level, and the Adler-32 trailer.
	for _, level := range []int{0, 2, 5, 6, 7, 9, -1} {
		for _, n := range []int{0, 1, 5000, 70000} {
			add(streamSpec{"rows", n, seed}, level, "zlib", 0)
			seed++
		}
	}

	// Inflate: every hand-made stream, and damaged copies of real ones.
	var inflate []inflateCase
	addInflate := func(name, wrapper string, data []byte) {
		out, err := inflateWith(wrapper, data)
		inflate = append(inflate, inflateCase{
			Name: name, Wrapper: wrapper, B64: b64(data),
			OutSHA: sha(out), OutLen: len(out), Err: imgErr(err),
		})
	}
	for _, f := range handMadeInflateCorpus() {
		addInflate(f.Name, "flate", f.Data)
	}
	for i, spec := range []streamSpec{{"text", 3000, 900}, {"rows", 6000, 901}, {"noise", 1500, 902}, {"runs", 4000, 903}} {
		for _, wrapper := range []string{"flate", "zlib"} {
			good := deflateWith(wrapper, 9, 0, spec.build())
			name := func(s string) string { return wrapper + "_" + spec.Kind + "_" + s }
			addInflate(name("ok"), wrapper, good)
			for _, cut := range []int{0, 1, 2, 10, len(good) / 2, len(good) - 4, len(good) - 1} {
				if cut >= 0 && cut < len(good) {
					addInflate(name("truncated_"+itoa(cut)), wrapper, good[:cut])
				}
			}
			for _, pos := range []int{0, 1, 5, 17, len(good) / 2, len(good) - 3} {
				if pos < 0 || pos >= len(good) {
					continue
				}
				for _, bit := range []uint{0, 7} {
					bad := bytes.Clone(good)
					bad[pos] ^= 1 << bit
					addInflate(name("flip_"+itoa(pos)+"_"+itoa(int(bit))), wrapper, bad)
				}
			}
			addInflate(name("trailing_garbage"), wrapper, append(bytes.Clone(good), 1, 2, 3))
			_ = i
		}
	}
	// zlib header errors.
	for _, h := range [][]byte{{0x78, 0x9c}, {0x78, 0x9d}, {0x79, 0x9c}, {0x88, 0x9c}, {0x78, 0xbb, 0, 0, 0, 1}, {0x78}, {}} {
		addInflate("zlib_header_"+hexs(h), "zlib", h)
	}

	return map[string]any{"deflate": cases, "inflate": inflate}, nil
}

func hexs(b []byte) string {
	const digits = "0123456789abcdef"
	var s []byte
	for _, c := range b {
		s = append(s, digits[c>>4], digits[c&15])
	}
	if len(s) == 0 {
		return "empty"
	}
	return string(s)
}

// --- png ----------------------------------------------------------------------------------------

type encodeCase struct {
	Spec imgSpec `json:"spec"`
	// The generated input, hashed — the Rust generator mirror is checked against this first.
	Input map[string]any `json:"input"`
	// PNG: "best" (Mattermost's encoder) or "default". JPEG: the quality.
	Level   string         `json:"level,omitempty"`
	Quality *int           `json:"quality,omitempty"`
	Output  map[string]any `json:"output,omitempty"`
	Err     any            `json:"err"`
}

func pngSpecs() []imgSpec {
	var specs []imgSpec
	seed := uint64(100)
	add := func(kind string, w, h int, pattern, alpha string, pal int, ratio string) {
		specs = append(specs, imgSpec{kind, w, h, pattern, alpha, seed, pal, ratio})
		seed++
	}
	sizes := [][2]int{{1, 1}, {2, 3}, {7, 5}, {33, 17}, {120, 100}, {128, 128}, {300, 200}}
	for _, sz := range sizes {
		w, h := sz[0], sz[1]
		add("gray", w, h, "smooth", "opaque", 0, "")
		add("gray16", w, h, "gradient", "opaque", 0, "")
		add("nrgba", w, h, "blocks", "opaque", 0, "")
		add("nrgba", w, h, "smooth", "mixed", 0, "")
		add("nrgba", w, h, "noise", "binary", 0, "")
		add("nrgba", w, h, "gradient", "soft", 0, "")
		add("rgba", w, h, "blocks", "opaque", 0, "")
		add("rgba", w, h, "smooth", "mixed", 0, "")
		add("nrgba64", w, h, "gradient", "opaque", 0, "")
		add("nrgba64", w, h, "noise", "mixed", 0, "")
		add("rgba64", w, h, "smooth", "opaque", 0, "")
		add("rgba64", w, h, "blocks", "mixed", 0, "")
		add("ycbcr", w, h, "smooth", "opaque", 0, "420")
		add("cmyk", w, h, "blocks", "opaque", 0, "")
		for _, n := range []int{1, 2, 3, 4, 5, 16, 17, 256} {
			add("paletted", w, h, "noise", "opaque", n, "")
		}
		add("paletted", w, h, "blocks", "mixed", 200, "")
	}
	// Large enough to cross the encoder's 32 KiB IDAT buffer many times, and the deflater's
	// window shift and 16384-token block limit.
	add("nrgba", 1000, 700, "noise", "opaque", 0, "")
	add("nrgba", 1000, 700, "smooth", "opaque", 0, "")
	add("nrgba", 1024, 768, "blocks", "mixed", 0, "")
	add("gray", 2000, 900, "gradient", "opaque", 0, "")
	add("paletted", 800, 600, "blocks", "opaque", 256, "")
	add("nrgba", 1920, 1080, "smooth", "opaque", 0, "")
	return specs
}

// pngCorpus is every decoder input for the png stage, in a stable order.
func pngCorpus() []namedFile {
	var files []namedFile
	for _, dir := range []string{"image/png/testdata/pngsuite", "image/testdata"} {
		matches, _ := filepath.Glob(filepath.Join(runtime.GOROOT(), "src", dir, "*.png"))
		sort.Strings(matches)
		for _, m := range matches {
			data, err := os.ReadFile(m)
			if err != nil {
				panic(err)
			}
			files = append(files, namedFile{filepath.Base(m), data})
		}
	}
	// Go-encoded images of every type, at both levels Mattermost's pipeline meets.
	seed := uint64(5000)
	for _, spec := range []imgSpec{
		{"gray", 40, 30, "smooth", "opaque", 0, 0, ""},
		{"gray16", 40, 30, "gradient", "opaque", 0, 0, ""},
		{"nrgba", 40, 30, "blocks", "mixed", 0, 0, ""},
		{"nrgba", 40, 30, "blocks", "opaque", 0, 0, ""},
		{"rgba", 40, 30, "smooth", "mixed", 0, 0, ""},
		{"nrgba64", 40, 30, "noise", "mixed", 0, 0, ""},
		{"rgba64", 40, 30, "noise", "opaque", 0, 0, ""},
		{"paletted", 40, 30, "noise", "opaque", 0, 2, ""},
		{"paletted", 40, 30, "noise", "opaque", 0, 16, ""},
		{"paletted", 40, 30, "noise", "mixed", 0, 256, ""},
		{"nrgba", 200, 150, "smooth", "opaque", 0, 0, ""},
	} {
		spec.Seed = seed
		seed++
		m := spec.build()
		for _, lvl := range []png.CompressionLevel{png.BestCompression, png.DefaultCompression, png.NoCompression} {
			enc := png.Encoder{CompressionLevel: lvl}
			data := mustEncode(func(b *bytes.Buffer) error { return enc.Encode(b, m) })
			files = append(files, namedFile{"go_" + spec.Kind + "_" + spec.Alpha + "_" + itoa(spec.Palette) + "_l" + itoa(int(-lvl)) + "_" + itoa(spec.W), data})
		}
	}
	// Hand-built PNGs for the branches Go's encoder never produces.
	files = append(files, craftedPNGs()...)
	// Damaged copies.
	base := mustEncode(func(b *bytes.Buffer) error {
		return (&png.Encoder{CompressionLevel: png.BestCompression}).Encode(b, imgSpec{"nrgba", 64, 48, "blocks", "mixed", 77, 0, ""}.build())
	})
	for _, cut := range []int{0, 7, 8, 12, 20, 33, 40, 60, len(base) / 2, len(base) - 13, len(base) - 12, len(base) - 1} {
		files = append(files, namedFile{"cut_" + itoa(cut), base[:cut]})
	}
	for _, pos := range []int{0, 9, 16, 29, 32, 41, 45, 50, len(base) / 2, len(base) - 20, len(base) - 14, len(base) - 5} {
		bad := bytes.Clone(base)
		bad[pos] ^= 0x10
		files = append(files, namedFile{"flip_" + itoa(pos), bad})
	}
	files = append(files, namedFile{"trailing_garbage", append(bytes.Clone(base), "garbage"...)})
	return files
}

func pngChunkBytes(kind string, data []byte) []byte {
	var b bytes.Buffer
	_ = binary.Write(&b, binary.BigEndian, uint32(len(data)))
	b.WriteString(kind)
	b.Write(data)
	_ = binary.Write(&b, binary.BigEndian, crc32.ChecksumIEEE(append([]byte(kind), data...)))
	return b.Bytes()
}

func zlibOf(raw []byte) []byte {
	var b bytes.Buffer
	w, _ := zlib.NewWriterLevel(&b, zlib.BestCompression)
	_, _ = w.Write(raw)
	_ = w.Close()
	return b.Bytes()
}

// storedZlib wraps raw in a zlib stream of one final stored block, so the IDAT payload is exactly
// the bytes a test says it is.
func storedZlib(raw []byte) []byte {
	n := uint16(len(raw))
	out := []byte{0x78, 0x01, 0x01, byte(n), byte(n >> 8), byte(^n), byte(^n >> 8)}
	out = append(out, raw...)
	return binary.BigEndian.AppendUint32(out, adler32.Checksum(raw))
}

// rawPNGWithIDAT is assemblePNG with the IDAT payload given verbatim rather than compressed.
func rawPNGWithIDAT(w, h uint32, depth, ct byte, before [][2]string, idat []byte) []byte {
	ihdr := make([]byte, 13)
	binary.BigEndian.PutUint32(ihdr[0:4], w)
	binary.BigEndian.PutUint32(ihdr[4:8], h)
	ihdr[8], ihdr[9] = depth, ct
	var b bytes.Buffer
	b.WriteString("\x89PNG\r\n\x1a\n")
	b.Write(pngChunkBytes("IHDR", ihdr))
	for _, c := range before {
		b.Write(pngChunkBytes(c[0], []byte(c[1])))
	}
	b.Write(pngChunkBytes("IDAT", idat))
	b.Write(pngChunkBytes("IEND", nil))
	return b.Bytes()
}

// assemblePNG builds a PNG from an IHDR and a raw (already filtered) scanline stream plus extra
// chunks placed before IDAT.
func assemblePNG(w, h uint32, depth, ct, interlace byte, before [][2]string, raw []byte, after [][2]string) []byte {
	ihdr := make([]byte, 13)
	binary.BigEndian.PutUint32(ihdr[0:4], w)
	binary.BigEndian.PutUint32(ihdr[4:8], h)
	ihdr[8], ihdr[9], ihdr[12] = depth, ct, interlace
	var b bytes.Buffer
	b.WriteString("\x89PNG\r\n\x1a\n")
	b.Write(pngChunkBytes("IHDR", ihdr))
	for _, c := range before {
		b.Write(pngChunkBytes(c[0], []byte(c[1])))
	}
	b.Write(pngChunkBytes("IDAT", zlibOf(raw)))
	for _, c := range after {
		b.Write(pngChunkBytes(c[0], []byte(c[1])))
	}
	b.Write(pngChunkBytes("IEND", nil))
	return b.Bytes()
}

func craftedPNGs() []namedFile {
	var files []namedFile
	add := func(name string, data []byte) { files = append(files, namedFile{"crafted_" + name, data}) }
	// 4x2 truecolour 8-bit rows, filter byte first.
	rows := func(filter byte, bpr int, h int) []byte {
		var raw []byte
		for y := 0; y < h; y++ {
			raw = append(raw, filter)
			for i := 0; i < bpr; i++ {
				raw = append(raw, uint8(hash4(31, i, y, 0)>>56))
			}
		}
		return raw
	}
	for f := byte(0); f <= 5; f++ {
		add("filter_"+itoa(int(f)), assemblePNG(4, 2, 8, 2, 0, nil, rows(f, 12, 2), nil))
	}
	add("gray_trns", assemblePNG(4, 2, 8, 0, 0, [][2]string{{"tRNS", "\x00\x7f"}}, rows(0, 4, 2), nil))
	add("gray4_trns", assemblePNG(4, 2, 4, 0, 0, [][2]string{{"tRNS", "\x00\x05"}}, rows(0, 2, 2), nil))
	add("gray16_trns", assemblePNG(4, 2, 16, 0, 0, [][2]string{{"tRNS", "\x12\x34"}}, rows(0, 8, 2), nil))
	add("rgb_trns", assemblePNG(4, 2, 8, 2, 0, [][2]string{{"tRNS", "\x00\x10\x00\x20\x00\x30"}}, rows(0, 12, 2), nil))
	add("rgb16_trns", assemblePNG(4, 2, 16, 2, 0, [][2]string{{"tRNS", "\x00\x10\x00\x20\x00\x30"}}, rows(0, 24, 2), nil))
	add("rgb_trns_bad_len", assemblePNG(4, 2, 8, 2, 0, [][2]string{{"tRNS", "\x00\x10"}}, rows(0, 12, 2), nil))
	pal := "\xff\x00\x00\x00\xff\x00\x00\x00\xff"
	add("palette_ok", assemblePNG(4, 2, 8, 3, 0, [][2]string{{"PLTE", pal}}, []byte{0, 0, 1, 2, 1, 0, 2, 1, 0, 0}, nil))
	add("palette_index_oob", assemblePNG(4, 2, 8, 3, 0, [][2]string{{"PLTE", pal}}, []byte{0, 0, 1, 2, 7, 0, 2, 1, 0, 0}, nil))
	add("palette_trns", assemblePNG(4, 2, 8, 3, 0, [][2]string{{"PLTE", pal}, {"tRNS", "\x80\x00"}}, []byte{0, 0, 1, 2, 1, 0, 2, 1, 0, 0}, nil))
	add("palette_trns_too_long", assemblePNG(4, 2, 8, 3, 0, [][2]string{{"PLTE", pal}, {"tRNS", "\x80\x00\x01\x02"}}, []byte{0, 0, 1, 2, 1, 0, 2, 1, 0, 0}, nil))
	add("palette_missing", assemblePNG(4, 2, 8, 3, 0, nil, []byte{0, 0, 1, 2, 1, 0, 2, 1, 0, 0}, nil))
	add("palette_bad_len", assemblePNG(4, 2, 8, 3, 0, [][2]string{{"PLTE", "\x01\x02"}}, []byte{0, 0, 1, 2, 1, 0, 2, 1, 0, 0}, nil))
	add("palette_after_idat", assemblePNG(4, 2, 8, 2, 0, nil, rows(0, 12, 2), [][2]string{{"PLTE", pal}}))
	add("palette_on_truecolor", assemblePNG(4, 2, 8, 2, 0, [][2]string{{"PLTE", pal}}, rows(0, 12, 2), nil))
	add("palette_1bit", assemblePNG(9, 2, 1, 3, 0, [][2]string{{"PLTE", pal[:6]}}, []byte{0, 0xa5, 0x80, 0, 0x5a, 0x00}, nil))
	add("palette_2bit_oob", assemblePNG(3, 1, 2, 3, 0, [][2]string{{"PLTE", pal[:6]}}, []byte{0, 0xfc}, nil))
	add("too_little_data", assemblePNG(4, 2, 8, 2, 0, nil, rows(0, 12, 1), nil))
	add("too_much_data", assemblePNG(4, 2, 8, 2, 0, nil, rows(0, 12, 3), nil))
	add("zero_width", assemblePNG(0, 2, 8, 2, 0, nil, nil, nil))
	add("bad_depth", assemblePNG(4, 2, 3, 2, 0, nil, rows(0, 12, 2), nil))
	add("bad_colortype", assemblePNG(4, 2, 8, 5, 0, nil, rows(0, 12, 2), nil))
	add("bad_interlace", assemblePNG(4, 2, 8, 2, 2, nil, rows(0, 12, 2), nil))
	add("ancillary_before", assemblePNG(4, 2, 8, 2, 0, [][2]string{{"tEXt", "k\x00v"}, {"gAMA", "\x00\x00\xb1\x8f"}}, rows(1, 12, 2), nil))
	add("unknown_critical", assemblePNG(4, 2, 8, 2, 0, [][2]string{{"ABCD", "x"}}, rows(1, 12, 2), nil))
	add("exif_chunk", assemblePNG(4, 2, 8, 2, 0, [][2]string{{"eXIf", "MM\x00\x2a\x00\x00\x00\x08\x00\x00"}}, rows(2, 12, 2), nil))
	// Low-depth gray with tRNS: the transparent level is in the sample's own depth and is scaled
	// like the samples.
	add("gray1_trns", rawPNGWithIDAT(3, 1, 1, 0, [][2]string{{"tRNS", "\x00\x01"}}, storedZlib([]byte{0, 0xa0})))
	add("gray2_trns", rawPNGWithIDAT(3, 1, 2, 0, [][2]string{{"tRNS", "\x00\x02"}}, storedZlib([]byte{0, 0x9c})))
	add("gray4_trns_stored", rawPNGWithIDAT(3, 1, 4, 0, [][2]string{{"tRNS", "\x00\x07"}}, storedZlib([]byte{0, 0x71, 0x70})))
	// Bytes after the zlib stream inside IDAT: what zlib's 4096-byte read-ahead already took is
	// swallowed; anything left in the chunk is "too much pixel data".
	add("idat_trailing_100", rawPNGWithIDAT(2, 1, 8, 0, nil, append(storedZlib([]byte{0, 1, 2}), make([]byte, 100)...)))
	add("idat_trailing_5000", rawPNGWithIDAT(2, 1, 8, 0, nil, append(storedZlib([]byte{0, 1, 2}), make([]byte, 5000)...)))
	// Interlaced (Adam7) 8-bit RGB, 9x9, built by the reference algorithm: each pass's rows,
	// filter 0.
	{
		const w, h = 9, 9
		var raw []byte
		passes := [][4]int{{0, 0, 8, 8}, {4, 0, 8, 8}, {0, 4, 4, 8}, {2, 0, 4, 4}, {0, 2, 2, 4}, {1, 0, 2, 2}, {0, 1, 1, 2}}
		for _, p := range passes {
			xo, yo, xs, ys := p[0], p[1], p[2], p[3]
			pw := (w - xo + xs - 1) / xs
			ph := (h - yo + ys - 1) / ys
			if pw <= 0 || ph <= 0 {
				continue
			}
			for y := 0; y < ph; y++ {
				raw = append(raw, 0)
				for x := 0; x < pw; x++ {
					for k := 0; k < 3; k++ {
						raw = append(raw, uint8(hash4(41, xo+x*xs, yo+y*ys, k)>>56))
					}
				}
			}
		}
		add("interlaced_rgb", assemblePNG(w, h, 8, 2, 1, nil, raw, nil))
	}
	// Two IDAT chunks, and an IDAT split with a chunk between (Go rejects the second IDAT run).
	{
		raw := rows(0, 12, 4)
		z := zlibOf(raw)
		mid := len(z) / 2
		ihdr := make([]byte, 13)
		binary.BigEndian.PutUint32(ihdr[0:4], 4)
		binary.BigEndian.PutUint32(ihdr[4:8], 4)
		ihdr[8], ihdr[9] = 8, 2
		var b bytes.Buffer
		b.WriteString("\x89PNG\r\n\x1a\n")
		b.Write(pngChunkBytes("IHDR", ihdr))
		b.Write(pngChunkBytes("IDAT", z[:mid]))
		b.Write(pngChunkBytes("IDAT", z[mid:]))
		b.Write(pngChunkBytes("IEND", nil))
		add("two_idats", b.Bytes())
		var c bytes.Buffer
		c.WriteString("\x89PNG\r\n\x1a\n")
		c.Write(pngChunkBytes("IHDR", ihdr))
		c.Write(pngChunkBytes("IDAT", z[:mid]))
		c.Write(pngChunkBytes("tEXt", []byte("k\x00v")))
		c.Write(pngChunkBytes("IDAT", z[mid:]))
		c.Write(pngChunkBytes("IEND", nil))
		add("split_idats", c.Bytes())
		var d bytes.Buffer
		d.WriteString("\x89PNG\r\n\x1a\n")
		d.Write(pngChunkBytes("IHDR", ihdr))
		d.Write(pngChunkBytes("IDAT", z))
		add("no_iend", d.Bytes())
	}
	return files
}

type decodeCase struct {
	Name   string         `json:"name"`
	B64    string         `json:"b64"`
	Config map[string]any `json:"config"`
	Image  map[string]any `json:"image"`
}

// configOf records image.DecodeConfig: dimensions, the colour model's identity and the format.
func configOf(data []byte) map[string]any {
	cfg, format, err := image.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	return map[string]any{"w": cfg.Width, "h": cfg.Height, "format": format, "model": modelName(cfg.ColorModel)}
}

func modelName(m color.Model) any {
	switch m {
	case color.RGBAModel:
		return "rgba"
	case color.RGBA64Model:
		return "rgba64"
	case color.NRGBAModel:
		return "nrgba"
	case color.NRGBA64Model:
		return "nrgba64"
	case color.AlphaModel:
		return "alpha"
	case color.Alpha16Model:
		return "alpha16"
	case color.GrayModel:
		return "gray"
	case color.Gray16Model:
		return "gray16"
	case color.YCbCrModel:
		return "ycbcr"
	case color.NYCbCrAModel:
		return "nycbcra"
	case color.CMYKModel:
		return "cmyk"
	}
	if p, ok := m.(color.Palette); ok {
		pal := make([][]any, len(p))
		for i, c := range p {
			pal[i] = paletteEntry(c)
		}
		return map[string]any{"palette": pal}
	}
	return fmt.Sprintf("%T", m)
}

func imageOf(data []byte) map[string]any {
	m, format, err := image.Decode(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	d := describe(m)
	d["format"] = format
	return d
}

func imagingPNGStage() (map[string]any, error) {
	var enc []encodeCase
	for _, spec := range pngSpecs() {
		m := spec.build()
		levels := []string{"best"}
		if spec.W <= 33 || spec.W == 300 {
			levels = append(levels, "default")
		}
		for _, lvl := range levels {
			e := png.Encoder{CompressionLevel: png.BestCompression}
			if lvl == "default" {
				e.CompressionLevel = png.DefaultCompression
			}
			var buf bytes.Buffer
			err := e.Encode(&buf, m)
			c := encodeCase{Spec: spec, Input: describe(m), Level: lvl, Err: imgErr(err)}
			if err == nil {
				c.Output = encoded(buf.Bytes())
			}
			enc = append(enc, c)
		}
	}
	var dec []decodeCase
	for _, f := range pngCorpus() {
		dec = append(dec, decodeCase{Name: f.Name, B64: b64(f.Data), Config: configOf(f.Data), Image: imageOf(f.Data)})
	}
	return map[string]any{"encode": enc, "decode": dec}, nil
}

// --- jpeg ---------------------------------------------------------------------------------------

func jpegSpecs() []imgSpec {
	var specs []imgSpec
	seed := uint64(2000)
	add := func(kind string, w, h int, pattern, alpha string, pal int, ratio string) {
		specs = append(specs, imgSpec{kind, w, h, pattern, alpha, seed, pal, ratio})
		seed++
	}
	sizes := [][2]int{{1, 1}, {3, 5}, {8, 8}, {9, 9}, {16, 16}, {17, 15}, {120, 100}, {333, 201}}
	for _, sz := range sizes {
		w, h := sz[0], sz[1]
		add("gray", w, h, "smooth", "opaque", 0, "")
		add("nrgba", w, h, "blocks", "opaque", 0, "")
		add("nrgba", w, h, "smooth", "mixed", 0, "")
		add("rgba", w, h, "noise", "opaque", 0, "")
		add("rgba", w, h, "gradient", "mixed", 0, "")
		for _, r := range []string{"444", "422", "420", "440", "411", "410"} {
			add("ycbcr", w, h, "smooth", "opaque", 0, r)
		}
		add("paletted", w, h, "noise", "mixed", 50, "")
		add("cmyk", w, h, "blocks", "opaque", 0, "")
		add("gray16", w, h, "gradient", "opaque", 0, "")
		add("nrgba64", w, h, "noise", "mixed", 0, "")
	}
	add("nrgba", 1920, 1080, "smooth", "opaque", 0, "")
	add("ycbcr", 1600, 1200, "blocks", "opaque", 0, "420")
	return specs
}

func jpegCorpus() []namedFile {
	var files []namedFile
	matches, _ := filepath.Glob(filepath.Join(runtime.GOROOT(), "src", "image", "testdata", "*.jpeg"))
	sort.Strings(matches)
	for _, m := range matches {
		data, err := os.ReadFile(m)
		if err != nil {
			panic(err)
		}
		files = append(files, namedFile{filepath.Base(m), data})
	}
	seed := uint64(6000)
	for _, spec := range []imgSpec{
		{"nrgba", 40, 30, "smooth", "opaque", 0, 0, ""},
		{"gray", 41, 29, "blocks", "opaque", 0, 0, ""},
		{"ycbcr", 57, 33, "noise", "opaque", 0, 0, "444"},
		{"cmyk", 16, 16, "blocks", "opaque", 0, 0, ""},
		{"nrgba", 640, 480, "smooth", "opaque", 0, 0, ""},
	} {
		spec.Seed = seed
		seed++
		m := spec.build()
		for _, q := range []int{90, 50, 100, 1} {
			data := mustEncode(func(b *bytes.Buffer) error { return jpeg.Encode(b, m, &jpeg.Options{Quality: q}) })
			files = append(files, namedFile{"go_" + spec.Kind + "_q" + itoa(q) + "_" + itoa(spec.W), data})
		}
	}
	base := mustEncode(func(b *bytes.Buffer) error {
		return jpeg.Encode(b, imgSpec{"nrgba", 64, 48, "blocks", "opaque", 88, 0, ""}.build(), &jpeg.Options{Quality: 90})
	})
	for _, cut := range []int{0, 1, 2, 3, 4, 20, 100, 300, 400, len(base) / 2, len(base) - 3, len(base) - 2, len(base) - 1} {
		if cut < len(base) {
			files = append(files, namedFile{"cut_" + itoa(cut), base[:cut]})
		}
	}
	for _, pos := range []int{2, 3, 5, 25, 90, 160, 200, 330, len(base) / 2, len(base) - 50, len(base) - 2} {
		bad := bytes.Clone(base)
		bad[pos] ^= 0x21
		files = append(files, namedFile{"flip_" + itoa(pos), bad})
	}
	files = append(files, namedFile{"trailing_garbage", append(bytes.Clone(base), "garbage"...)})
	return files
}

func imagingJPEGStage() (map[string]any, error) {
	var enc []encodeCase
	for _, spec := range jpegSpecs() {
		m := spec.build()
		qs := []int{90}
		if spec.W < 1000 {
			qs = []int{90, 75, 50, 100, 1, 0, 101, -5}
		}
		for _, q := range qs {
			q := q
			var buf bytes.Buffer
			err := jpeg.Encode(&buf, m, &jpeg.Options{Quality: q})
			c := encodeCase{Spec: spec, Input: describe(m), Quality: &q, Err: imgErr(err)}
			if err == nil {
				c.Output = encoded(buf.Bytes())
			}
			enc = append(enc, c)
		}
	}
	var dec []decodeCase
	for _, f := range jpegCorpus() {
		dec = append(dec, decodeCase{Name: f.Name, B64: b64(f.Data), Config: configOf(f.Data), Image: imageOf(f.Data)})
	}
	return map[string]any{"encode": enc, "decode": dec}, nil
}
