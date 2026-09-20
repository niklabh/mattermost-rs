package main

// The WebP stage of the imaging oracle. See behaviour_imaging.go for the generator and the
// conventions, and behaviour_imaging_codec.go for the `decodeCase`/`configOf`/`imageOf` shape
// every decode corpus uses.
//
// Two things make this stage different from the PNG and JPEG ones.
//
// First, there is no WebP encoder anywhere in Go, so the only real bitstreams available are
// golang.org/x/image's own pinned testdata. Every one of them is used, plus truncations and byte
// flips of the small ones, because a lossy frame's Y/Cb/Cr hashes are an all-or-nothing oracle: one
// wrong coefficient, one wrong filter threshold, one wrong prediction mode and the hash differs.
// The container and header branches, by contrast, are unreachable from any real file, so they are
// hand-assembled below from a real VP8/VP8L payload wrapped in a crafted RIFF.
//
// Second, `Config`/`Image` here record `webp.DecodeConfig`/`webp.Decode` *directly*, not
// `image.DecodeConfig`/`image.Decode`. The generic entry points sniff "RIFF????WEBPVP8" first and
// answer "image: unknown format" for everything that does not match it, which would hide exactly
// the container errors this corpus exists to pin. `Sniff` records what the generic entry point
// would have said, so whoever wires the Rust registry knows which inputs reach the decoder at all.

import (
	"bytes"
	"encoding/binary"
	"image"

	"golang.org/x/image/webp"
)

// webpDecodeCase is a decodeCase plus the format-sniffing answer. The embedded struct's fields are
// flattened into the same JSON object, so the Rust side reads it exactly like a PNG decode case.
type webpDecodeCase struct {
	decodeCase
	Sniff string `json:"sniff"`
}

// webpConfigOf records webp.DecodeConfig: dimensions and the colour model's identity.
func webpConfigOf(data []byte) map[string]any {
	cfg, err := webp.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	return map[string]any{"w": cfg.Width, "h": cfg.Height, "format": "webp", "model": modelName(cfg.ColorModel)}
}

// webpImageOf records webp.Decode. *image.NYCbCrA is not one of describe's cases — webp is the only
// decoder in the pipeline that returns one — so its planes are spelled out here, alpha included.
func webpImageOf(data []byte) map[string]any {
	m, err := webp.Decode(bytes.NewReader(data))
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	d := describe(m)
	d["format"] = "webp"
	if n, ok := m.(*image.NYCbCrA); ok {
		d["type"] = "nycbcra"
		d["ratio"] = n.SubsampleRatio.String()
		d["y_stride"], d["c_stride"], d["a_stride"] = n.YStride, n.CStride, n.AStride
		d["y_sha256"], d["cb_sha256"] = sha(n.Y), sha(n.Cb)
		d["cr_sha256"], d["a_sha256"] = sha(n.Cr), sha(n.A)
		d["y_len"], d["c_len"], d["a_len"] = len(n.Y), len(n.Cb), len(n.A)
	}
	return d
}

// webpSniffOf records what image.DecodeConfig makes of the bytes: the registered format name, or
// the error. It is the "RIFF????WEBPVP8" prefix test and nothing more.
func webpSniffOf(data []byte) string {
	_, format, err := image.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return err.Error()
	}
	return format
}

// --- assembling RIFF containers -----------------------------------------------------------------

// webpChunk is one RIFF chunk: a four-byte ID, a little-endian uint32 length, the data, and a pad
// byte when the length is odd.
func webpChunk(id string, data []byte) []byte {
	b := make([]byte, 0, 8+len(data)+1)
	b = append(b, id...)
	b = binary.LittleEndian.AppendUint32(b, uint32(len(data)))
	b = append(b, data...)
	if len(data)&1 == 1 {
		b = append(b, 0)
	}
	return b
}

// webpChunkUnpadded is webpChunk without the pad byte an odd length requires.
func webpChunkUnpadded(id string, data []byte) []byte {
	return webpChunk(id, data)[:8+len(data)]
}

// webpChunkLying is a chunk header declaring declLen bytes over data that is some other length.
func webpChunkLying(id string, declLen uint32, data []byte) []byte {
	b := append([]byte(id), 0, 0, 0, 0)
	binary.LittleEndian.PutUint32(b[4:], declLen)
	return append(b, data...)
}

// webpFile wraps chunks in "RIFF"+size+"WEBP" with the size the chunks imply.
func webpFile(chunks ...[]byte) []byte {
	var body []byte
	for _, c := range chunks {
		body = append(body, c...)
	}
	return riffFile("WEBP", uint32(4+len(body)), body)
}

// riffFile wraps a form type and a body in a RIFF header declaring size bytes, whatever the body's
// actual length is.
func riffFile(formType string, size uint32, body []byte) []byte {
	b := append([]byte("RIFF"), 0, 0, 0, 0)
	binary.LittleEndian.PutUint32(b[4:], size)
	b = append(b, formType...)
	return append(b, body...)
}

// vp8xChunk is the 10-byte extended-format header: a flags byte, three reserved bytes, and 24-bit
// little-endian canvas width-minus-one and height-minus-one.
func vp8xChunk(flags byte, widthMinusOne, heightMinusOne uint32) []byte {
	d := make([]byte, 10)
	d[0] = flags
	d[4], d[5], d[6] = byte(widthMinusOne), byte(widthMinusOne>>8), byte(widthMinusOne>>16)
	d[7], d[8], d[9] = byte(heightMinusOne), byte(heightMinusOne>>8), byte(heightMinusOne>>16)
	return d
}

// webpChunkData walks a real WebP file's chunks and returns the first chunk with the given ID.
func webpChunkData(file []byte, id string) []byte {
	b := file[12:]
	for len(b) >= 8 {
		n := int(binary.LittleEndian.Uint32(b[4:8]))
		if n < 0 || 8+n > len(b) {
			return nil
		}
		if string(b[0:4]) == id {
			return bytes.Clone(b[8 : 8+n])
		}
		b = b[8+n+n&1:]
	}
	return nil
}

// --- the corpus ---------------------------------------------------------------------------------

func imagingWebPStage() (map[string]any, error) {
	files := xImageTestdata("testdata", "webp")
	byName := map[string][]byte{}
	for _, f := range files {
		byName[f.Name] = f.Data
	}

	// Payload building blocks, lifted out of real files so the crafted containers below hold a
	// bitstream that really decodes: the smallest lossless payload and the smallest lossy frame in
	// the corpus. Their dimensions are needed whenever a VP8X header has to agree with the image
	// chunk that follows it.
	lossless := webpChunkData(byName["gopher-doc.1bpp.lossless.webp"], "VP8L")
	lossy := webpChunkData(byName["blue-purple-pink.lossy.webp"], "VP8 ")
	const (
		llW, llH = 75, 100
		lyW, lyH = 150, 100
	)

	var dec []webpDecodeCase
	add := func(name string, data []byte) {
		dec = append(dec, webpDecodeCase{
			decodeCase: decodeCase{
				Name:   name,
				B64:    b64(data),
				Config: webpConfigOf(data),
				Image:  webpImageOf(data),
			},
			Sniff: webpSniffOf(data),
		})
	}

	// 1. Every real file, whole.
	for _, f := range files {
		add(f.Name, f.Data)
	}

	// 2. Truncations. Small files get a dense sweep across the container boundaries (0..12 is the
	// RIFF header, 12..20 the first chunk header, then the bitstream); the large ones get the
	// cheap prefixes only, since a half-megabyte base64 blob per cut buys nothing the small files
	// do not already buy.
	smallCuts := []int{0, 1, 3, 4, 7, 8, 11, 12, 13, 15, 16, 19, 20, 21, 22, 24, 25, 30, 64, 200}
	for _, n := range []string{
		"gopher-doc.1bpp.lossless.webp",
		"blue-purple-pink.lossy.webp",
		"gopher-doc.with-alpha.lossless.webp",
		"yellow_rose.lossy-with-alpha.webp",
	} {
		data := byName[n]
		cuts := append([]int{}, smallCuts...)
		cuts = append(cuts, len(data)/2, len(data)-2, len(data)-1)
		for _, c := range cuts {
			if c >= 0 && c < len(data) {
				add("cut_"+itoa(c)+"_"+n, data[:c])
			}
		}
	}
	for _, n := range []string{
		"blue-purple-pink-large.no-filter.lossy.webp",
		"blue-purple-pink-large.simple-filter.lossy.webp",
		"blue-purple-pink-large.normal-filter.lossy.webp",
		"tux.lossless.webp",
		"video-001.lossy.webp",
	} {
		data := byName[n]
		for _, c := range []int{20, 25, 40, 500} {
			if c < len(data) {
				add("cut_"+itoa(c)+"_"+n, data[:c])
			}
		}
	}

	// 3. Byte flips inside the bitstream. Positions below 32 are left alone: a flip in the RIFF or
	// VP8/VP8L header changes the declared dimensions, and a 16383x16383 lossless image is a
	// gigabyte of pix for no extra coverage. Everything from 32 on exercises the entropy decoder,
	// the Huffman trees, the transforms and the loop filter, where a wrong answer is a wrong hash
	// rather than an error.
	for _, n := range []string{
		"gopher-doc.1bpp.lossless.webp",
		"gopher-doc.4bpp.lossless.webp",
		"blue-purple-pink.lossy.webp",
		"video-001.lossy.webp",
		"yellow_rose.lossy-with-alpha.webp",
		"blue-purple-pink.lossless.webp",
		"tux.lossless.webp",
	} {
		data := byName[n]
		for _, pos := range []int{32, 33, 40, 48, 57, 71, 96, 128, 160, 224, 301, 512, 777, len(data) / 4, len(data) / 3, len(data) / 2, 2 * len(data) / 3, len(data) - 40, len(data) - 3} {
			if pos < 32 || pos >= len(data) {
				continue
			}
			bad := bytes.Clone(data)
			bad[pos] ^= 0x5b
			add("flip_"+itoa(pos)+"_"+n, bad)
		}
	}

	// 4. Crafted containers: the RIFF layer.
	add("crafted_empty", nil)
	add("crafted_riff_3", []byte("RIF"))
	add("crafted_riff_7", []byte("RIFF\x00\x00\x00"))
	add("crafted_not_riff", append([]byte("XIFF\x04\x00\x00\x00WEBP"), webpChunk("VP8L", lossless)...))
	add("crafted_riff_size_0", riffFile("WEBP", 0, nil))
	add("crafted_riff_size_3", riffFile("WEBP", 3, nil))
	add("crafted_riff_size_4_no_form", riffFile("", 4, nil))
	add("crafted_riff_size_4_short_form", riffFile("WE", 4, nil))
	add("crafted_form_wave", riffFile("WAVE", 4, nil))
	add("crafted_form_webp_no_chunks", riffFile("WEBP", 4, nil))
	// A RIFF whose size field lies. Too large is harmless — the size is only an upper bound on
	// what Next will read — and too small makes the first chunk a subchunk that overruns it.
	add("crafted_riff_size_too_big", riffFile("WEBP", 0x00ffffff, webpChunk("VP8L", lossless)))
	add("crafted_riff_size_too_small", riffFile("WEBP", 24, webpChunk("VP8L", lossless)))
	// totalLen is under a chunk header's eight bytes.
	add("crafted_chunk_header_short_total", riffFile("WEBP", 9, []byte("VP8L\x01")))
	// totalLen allows a chunk header but the bytes are not there.
	add("crafted_chunk_header_short_data", riffFile("WEBP", 64, []byte("VP8L\x01")))
	// A chunk that declares more data than is present: Next drains what it can and reports it.
	add("crafted_chunk_data_short", riffFile("WEBP", 4+8+100, webpChunkLying("JUNK", 100, []byte("0123456789"))))
	add("crafted_chunk_len_over_total", riffFile("WEBP", 64, webpChunkLying("JUNK", 100, []byte("0123456789"))))
	// An odd-length chunk with and without its pad byte, followed by a real image chunk.
	add("crafted_odd_chunk_padded", webpFile(webpChunk("JUNK", []byte("abc")), webpChunk("VP8L", lossless)))
	add("crafted_odd_chunk_unpadded", webpFile(webpChunkUnpadded("JUNK", []byte("abc")), webpChunk("VP8L", lossless)))
	// An odd final chunk whose pad byte the RIFF size promises and the file does not hold.
	add("crafted_missing_pad_byte", riffFile("WEBP", 4+8+3+1, webpChunkUnpadded("JUNK", []byte("abc"))))
	// The same, with the RIFF size stopping before the pad: Next has no byte left to skip.
	add("crafted_pad_past_total", riffFile("WEBP", 4+8+3, webpChunkUnpadded("JUNK", []byte("abc"))))
	// Chunks the decoder does not know are skipped, wherever they sit.
	add("crafted_unknown_chunks_before", webpFile(
		webpChunk("ICCP", []byte("profile")),
		webpChunk("EXIF", []byte("MM\x00\x2a")),
		webpChunk("LIST", []byte("movi")),
		webpChunk("VP8L", lossless),
	))
	// Two image chunks: the first one wins and the second is never read.
	add("crafted_two_vp8l", webpFile(webpChunk("VP8L", lossless), webpChunk("VP8L", lossless)))
	add("crafted_vp8l_then_vp8x", webpFile(webpChunk("VP8L", lossless), webpChunk("VP8X", vp8xChunk(0, llW-1, llH-1))))

	// 5. Crafted containers: the VP8X extended format.
	add("crafted_vp8x_only", webpFile(webpChunk("VP8X", vp8xChunk(0, llW-1, llH-1))))
	add("crafted_vp8x_only_alpha", webpFile(webpChunk("VP8X", vp8xChunk(0x10, llW-1, llH-1))))
	add("crafted_vp8x_lossless", webpFile(webpChunk("VP8X", vp8xChunk(0, llW-1, llH-1)), webpChunk("VP8L", lossless)))
	add("crafted_vp8x_lossless_wrong_w", webpFile(webpChunk("VP8X", vp8xChunk(0, llW, llH-1)), webpChunk("VP8L", lossless)))
	add("crafted_vp8x_lossless_wrong_h", webpFile(webpChunk("VP8X", vp8xChunk(0, llW-1, llH)), webpChunk("VP8L", lossless)))
	add("crafted_vp8x_lossy", webpFile(webpChunk("VP8X", vp8xChunk(0, lyW-1, lyH-1)), webpChunk("VP8 ", lossy)))
	add("crafted_vp8x_lossy_wrong_w", webpFile(webpChunk("VP8X", vp8xChunk(0, lyW, lyH-1)), webpChunk("VP8 ", lossy)))
	add("crafted_vp8x_lossy_wrong_h", webpFile(webpChunk("VP8X", vp8xChunk(0, lyW-1, lyH)), webpChunk("VP8 ", lossy)))
	add("crafted_vp8x_twice", webpFile(
		webpChunk("VP8X", vp8xChunk(0, llW-1, llH-1)),
		webpChunk("VP8X", vp8xChunk(0, llW-1, llH-1)),
		webpChunk("VP8L", lossless),
	))
	add("crafted_vp8x_len_9", webpFile(webpChunk("VP8X", vp8xChunk(0, llW-1, llH-1)[:9]), webpChunk("VP8L", lossless)))
	add("crafted_vp8x_len_11", webpFile(webpChunk("VP8X", append(vp8xChunk(0, llW-1, llH-1), 0)), webpChunk("VP8L", lossless)))
	// The canvas dimensions must multiply to at most MaxInt32.
	add("crafted_vp8x_huge", webpFile(webpChunk("VP8X", vp8xChunk(0, 0xffffff, 0xffffff))))
	add("crafted_vp8x_max_ok", webpFile(webpChunk("VP8X", vp8xChunk(0, 0xffff, 0x7ffe))))
	// Every flag bit but alpha is ignored.
	add("crafted_vp8x_other_flags", webpFile(webpChunk("VP8X", vp8xChunk(0x2e, llW-1, llH-1)), webpChunk("VP8L", lossless)))
	// The alpha bit with no ALPH chunk: the VP8L branch never looks at wantAlpha.
	add("crafted_vp8x_alpha_bit_lossless", webpFile(webpChunk("VP8X", vp8xChunk(0x10, llW-1, llH-1)), webpChunk("VP8L", lossless)))
	// The alpha bit with no ALPH chunk, then a lossy frame: the VP8 branch does look at it.
	add("crafted_vp8x_alpha_bit_lossy", webpFile(webpChunk("VP8X", vp8xChunk(0x10, lyW-1, lyH-1)), webpChunk("VP8 ", lossy)))
	// A VP8 chunk whose declared length does not fit an int32.
	add("crafted_vp8_len_negative", riffFile("WEBP", 0xffffffff, webpChunkLying("VP8 ", 0x80000000, lossy)))

	// 6. Crafted containers: the ALPH chunk. The uncompressed cases carry a full 150x100 alpha
	// plane, one byte per pixel, under each of the four unfilter modes; the compressed case reuses
	// a real VP8L stream of the right size as the alpha image.
	alphaRaw := make([]byte, lyW*lyH)
	for i := range alphaRaw {
		alphaRaw[i] = uint8(hash4(97, i%lyW, i/lyW, 0) >> 56)
	}
	vp8xAlpha := webpChunk("VP8X", vp8xChunk(0x10, lyW-1, lyH-1))
	for filter := 0; filter < 4; filter++ {
		head := byte(filter << 2)
		add("crafted_alph_raw_filter"+itoa(filter), webpFile(
			vp8xAlpha,
			webpChunk("ALPH", append([]byte{head}, alphaRaw...)),
			webpChunk("VP8 ", lossy),
		))
	}
	// Pre-processing bits (6..7) are read and ignored; the compression field is the low two bits.
	add("crafted_alph_preprocessing", webpFile(
		vp8xAlpha,
		webpChunk("ALPH", append([]byte{0xc0}, alphaRaw...)),
		webpChunk("VP8 ", lossy),
	))
	add("crafted_alph_compression_2", webpFile(
		vp8xAlpha,
		webpChunk("ALPH", append([]byte{0x02}, alphaRaw...)),
		webpChunk("VP8 ", lossy),
	))
	add("crafted_alph_compression_3", webpFile(
		vp8xAlpha,
		webpChunk("ALPH", append([]byte{0x03}, alphaRaw...)),
		webpChunk("VP8 ", lossy),
	))
	add("crafted_alph_short", webpFile(
		vp8xAlpha,
		webpChunk("ALPH", append([]byte{0x00}, alphaRaw[:500]...)),
		webpChunk("VP8 ", lossy),
	))
	add("crafted_alph_empty", webpFile(vp8xAlpha, webpChunk("ALPH", nil), webpChunk("VP8 ", lossy)))
	add("crafted_alph_no_vp8x", webpFile(webpChunk("ALPH", append([]byte{0x00}, alphaRaw...)), webpChunk("VP8 ", lossy)))
	add("crafted_alph_twice", webpFile(
		vp8xAlpha,
		webpChunk("ALPH", append([]byte{0x00}, alphaRaw...)),
		webpChunk("ALPH", append([]byte{0x00}, alphaRaw...)),
		webpChunk("VP8 ", lossy),
	))
	// An ALPH chunk followed by a lossless frame: `alpha != nil` rejects it.
	add("crafted_alph_then_vp8l", webpFile(
		webpChunk("VP8X", vp8xChunk(0x10, llW-1, llH-1)),
		webpChunk("ALPH", append([]byte{0x00}, make([]byte, llW*llH)...)),
		webpChunk("VP8L", lossless),
	))
	// The real compressed-alpha file's own ALPH payload, re-wrapped: same bytes, crafted frame.
	if alph := webpChunkData(byName["yellow_rose.lossy-with-alpha.webp"], "ALPH"); alph != nil {
		ry := webpChunkData(byName["yellow_rose.lossy-with-alpha.webp"], "VP8 ")
		vx := webpChunkData(byName["yellow_rose.lossy-with-alpha.webp"], "VP8X")
		add("crafted_alph_compressed_rewrapped", webpFile(
			webpChunk("VP8X", vx),
			webpChunk("ALPH", alph),
			webpChunk("VP8 ", ry),
		))
		// The same alpha under a VP8X whose dimensions no longer match the alpha plane's: the
		// synthesized VP8L header carries the VP8X dimensions, so the inner decode disagrees.
		bad := bytes.Clone(vx)
		bad[4]++
		add("crafted_alph_compressed_wrong_dims", webpFile(
			webpChunk("VP8X", bad),
			webpChunk("ALPH", alph),
			webpChunk("VP8 ", ry),
		))
	}

	// 7. Crafted bitstream headers: VP8L and VP8, reached through a well-formed container.
	llBad := func(mutate func([]byte)) []byte {
		b := bytes.Clone(lossless)
		mutate(b)
		return webpFile(webpChunk("VP8L", b))
	}
	add("crafted_vp8l_bad_magic", llBad(func(b []byte) { b[0] = 0x2e }))
	add("crafted_vp8l_version_1", llBad(func(b []byte) { b[4] |= 0x20 }))
	add("crafted_vp8l_empty", webpFile(webpChunk("VP8L", nil)))
	add("crafted_vp8l_1", webpFile(webpChunk("VP8L", lossless[:1])))
	add("crafted_vp8l_4", webpFile(webpChunk("VP8L", lossless[:4])))
	add("crafted_vp8l_5", webpFile(webpChunk("VP8L", lossless[:5])))
	// A 1x1 lossless image: magic, widthMinusOne=0, heightMinusOne=0, no alpha hint, version 0,
	// then a bit stream too short to hold any pixel. Header parses, pixels do not.
	add("crafted_vp8l_1x1_header_only", webpFile(webpChunk("VP8L", []byte{0x2f, 0x00, 0x00, 0x00, 0x00})))
	lyBad := func(mutate func([]byte)) []byte {
		b := bytes.Clone(lossy)
		mutate(b)
		return webpFile(webpChunk("VP8 ", b))
	}
	add("crafted_vp8_bad_sync", lyBad(func(b []byte) { b[3] = 0x9c }))
	add("crafted_vp8_interframe", lyBad(func(b []byte) { b[0] |= 1 }))
	add("crafted_vp8_version_3", lyBad(func(b []byte) { b[0] = b[0]&^0x0e | 0x06 }))
	add("crafted_vp8_hidden", lyBad(func(b []byte) { b[0] &^= 0x10 }))
	add("crafted_vp8_scale", lyBad(func(b []byte) { b[7] |= 0x40; b[9] |= 0x80 }))
	add("crafted_vp8_zero_size", lyBad(func(b []byte) { b[6], b[7], b[8], b[9] = 0, 0, 0, 0 }))
	add("crafted_vp8_1x1", lyBad(func(b []byte) { b[6], b[7], b[8], b[9] = 1, 0, 1, 0 }))
	add("crafted_vp8_empty", webpFile(webpChunk("VP8 ", nil)))
	// A three-byte inter-frame header with a zero-length first partition: everything up to the
	// Golden/AltRef check parses, so this is the one input that reaches it.
	add("crafted_vp8_golden_altref", webpFile(webpChunk("VP8 ", []byte{0x01, 0x00, 0x00})))
	add("crafted_vp8_2", webpFile(webpChunk("VP8 ", lossy[:2])))
	add("crafted_vp8_3", webpFile(webpChunk("VP8 ", lossy[:3])))
	add("crafted_vp8_9", webpFile(webpChunk("VP8 ", lossy[:9])))
	add("crafted_vp8_10", webpFile(webpChunk("VP8 ", lossy[:10])))
	// A first-partition length that overruns the chunk, and one that claims the whole chunk and
	// leaves nothing for the coefficient partitions.
	add("crafted_vp8_firstpart_huge", lyBad(func(b []byte) {
		b[0] |= 0xe0
		b[1], b[2] = 0xff, 0xff
	}))
	add("crafted_vp8_firstpart_zero", lyBad(func(b []byte) {
		b[0] &^= 0xe0
		b[1], b[2] = 0, 0
	}))
	// A VP8 chunk header that lies about its length. The decoder's remaining-bytes count comes
	// from that header, so the coefficient partitions are sized from it: at 16 MiB or more it is
	// rejected outright, and below that the read simply runs out.
	add("crafted_vp8_chunklen_16mib", riffFile("WEBP", 0x02000000, webpChunkLying("VP8 ", 0x01100000, lossy)))
	add("crafted_vp8_chunklen_8mib", riffFile("WEBP", 0x02000000, webpChunkLying("VP8 ", 0x00800000, lossy)))

	// 8. Crafted VP8L bit-streams. The header is exactly 40 bits (magic, 14+14 dimensions, an
	// ignored alpha hint and a 3-bit version), so byte five holds the transform loop's first bit
	// and, after it, decodePix's colour-cache parameters — all reachable by hand.
	vp8lBits := func(b byte) []byte { return webpFile(webpChunk("VP8L", []byte{0x2f, 0, 0, 0, 0, b})) }
	add("crafted_vp8l_cc_bits_0", vp8lBits(0x02))
	add("crafted_vp8l_cc_bits_12", vp8lBits(0x32))
	add("crafted_vp8l_cc_bits_1", vp8lBits(0x06))
	add("crafted_vp8l_repeated_transform", vp8lBits(0x2d))
	add("crafted_vp8l_subtract_green_only", vp8lBits(0x05))

	return map[string]any{"decode": dec}, nil
}
