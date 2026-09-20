package main

// The end-to-end stage of the imaging oracle: each input file run through the exact call sequence
// of every Mattermost write path that stores derived image bytes. The per-stage fixtures say which
// stage broke; this one says whether the stages compose the way the server composes them.
//
//   upload     UploadFileTask.preprocessImage + postprocessImage (app/file.go) — POST /files.
//              DecodeConfig, the orientation read through a *non-seekable* reader, Decode, upright,
//              then thumbnail + preview (PNG when the decoder said "png", JPEG q90 otherwise) and
//              the 16×16 mini preview (always JPEG q90).
//   handle     HandleImages/prepareImage (app/file.go) — the completing chunk of POST /uploads.
//              The orientation read through a *seekable* reader; thumbnail + preview only.
//   mini       generateMiniPreview (app/file.go) — createPost/draft for an info with no preview.
//   profile    AdjustImage (app/user.go) — POST /users/{id}/image. FillCenter 128×128, PNG.
//   emoji      uploadEmojiImage's non-GIF resize (app/emoji.go) — POST /emoji. image.Decode (no
//              resolution guard), Fit 128×128, PNG; "write_through" when it fits already.
//   brand      SaveBrandImage (app/brand.go) — POST /brand/image. Decode, PNG.

import (
	"bytes"
	"encoding/binary"
	"image"
	"image/color"
	"image/gif"
	"image/jpeg"
	"image/png"
	"io"

	mmimaging "github.com/mattermost/mattermost/server/v8/channels/app/imaging"
	"golang.org/x/image/bmp"
)

// pipelineGIFPalette is a fixed 256-entry palette for the animated inline case: a deterministic
// ramp rather than Plan 9's table, so the frames differ from each other in the low bits that a
// Lanczos resize is most likely to round differently.
func pipelineGIFPalette() color.Palette {
	p := make(color.Palette, 256)
	for i := range p {
		p[i] = color.RGBA{R: uint8(i), G: uint8(255 - i), B: uint8(i * 7 % 256), A: 0xff}
	}
	return p
}

// Mattermost's defaults: FileSettings.MaxImageResolution and the file.go constants.
const pipelineMaxRes = 7680 * 4320

func pipelineDecoder() *mmimaging.Decoder {
	d, err := mmimaging.NewDecoder(mmimaging.DecoderOptions{ConcurrencyLevel: 1, MaxDecodedResolution: pipelineMaxRes})
	if err != nil {
		panic(err)
	}
	return d
}

func pipelineEncoder() *mmimaging.Encoder {
	e, err := mmimaging.NewEncoder(mmimaging.EncoderOptions{ConcurrencyLevel: 1})
	if err != nil {
		panic(err)
	}
	return e
}

func encodeDerived(e *mmimaging.Encoder, m image.Image, imgType string) map[string]any {
	var buf bytes.Buffer
	var err error
	if imgType == "png" {
		err = e.EncodePNG(&buf, m)
	} else {
		err = e.EncodeJPEG(&buf, m, 90)
	}
	if err != nil {
		return map[string]any{"err": err.Error()}
	}
	d := encoded(buf.Bytes())
	b := m.Bounds()
	d["w"], d["h"] = b.Dx(), b.Dy()
	return d
}

func encodePNGOf(e *mmimaging.Encoder, m image.Image) map[string]any {
	var buf bytes.Buffer
	if err := e.EncodePNG(&buf, m); err != nil {
		return map[string]any{"err": err.Error()}
	}
	d := encoded(buf.Bytes())
	b := m.Bounds()
	d["w"], d["h"] = b.Dx(), b.Dy()
	return d
}

type pipelineCase struct {
	// Where the input bytes live: another stage's decode corpus ("png", "jpeg", "exif") by name,
	// or "inline" with the bytes in B64.
	Source string `json:"source"`
	Name   string `json:"name"`
	B64    string `json:"b64,omitempty"`

	Config            map[string]any `json:"config"`
	OrientationStream map[string]any `json:"orientation_stream"`
	OrientationSeeker map[string]any `json:"orientation_seeker"`
	Decode            map[string]any `json:"decode"`
	Upload            map[string]any `json:"upload,omitempty"`
	Handle            map[string]any `json:"handle,omitempty"`
	Mini              any            `json:"mini,omitempty"`
	Profile           map[string]any `json:"profile,omitempty"`
	Emoji             map[string]any `json:"emoji,omitempty"`
	Brand             map[string]any `json:"brand,omitempty"`
}

func runPipeline(source, name string, data []byte) pipelineCase {
	c := pipelineCase{Source: source, Name: name}
	dec, enc := pipelineDecoder(), pipelineEncoder()

	cfg, format, err := dec.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		c.Config = map[string]any{"err": err.Error()}
	} else {
		c.Config = map[string]any{"w": cfg.Width, "h": cfg.Height, "format": format}
	}
	oStream, errStream := mmimaging.GetImageOrientation(io.MultiReader(bytes.NewReader(data)), format)
	c.OrientationStream = map[string]any{"orientation": oStream, "err": errStream != nil}

	img, imgType, err := dec.Decode(bytes.NewReader(data))
	if err != nil {
		c.Decode = map[string]any{"err": err.Error()}
	} else {
		c.Decode = map[string]any{"format": imgType}
	}
	oSeeker, errSeeker := mmimaging.GetImageOrientation(bytes.NewReader(data), imgType)
	c.OrientationSeeker = map[string]any{"orientation": oSeeker, "err": errSeeker != nil}

	if img != nil {
		// upload: the stream orientation, computed in preprocess from DecodeConfig's format.
		up := mmimaging.MakeImageUpright(img, oStream)
		mini, merr := mmimaging.GenerateMiniPreviewImage(up, 16, 16, 90)
		c.Upload = map[string]any{
			"thumb":   encodeDerived(enc, mmimaging.GenerateThumbnail(up, 120, 100), imgType),
			"preview": encodeDerived(enc, mmimaging.GeneratePreview(up, 1920), imgType),
		}
		if merr != nil {
			c.Upload["mini_err"] = merr.Error()
		} else {
			c.Upload["mini"] = b64(mini)
		}
		// handle / mini / profile: the seeker orientation from Decode's format.
		hs := mmimaging.MakeImageUpright(img, oSeeker)
		c.Handle = map[string]any{
			"thumb":   encodeDerived(enc, mmimaging.GenerateThumbnail(hs, 120, 100), imgType),
			"preview": encodeDerived(enc, mmimaging.GeneratePreview(hs, 1920), imgType),
		}
		if m, err := mmimaging.GenerateMiniPreviewImage(hs, 16, 16, 90); err == nil {
			c.Mini = b64(m)
		} else {
			c.Mini = map[string]any{"err": err.Error()}
		}
		c.Profile = encodePNGOf(enc, mmimaging.FillCenter(hs, 128, 128))
		c.Brand = encodePNGOf(enc, img)
	}

	// emoji: image.Decode directly, no orientation, Fit only past 128 in either direction.
	if cfg, _, err := image.DecodeConfig(bytes.NewReader(data)); err == nil {
		if cfg.Width <= 128 && cfg.Height <= 128 {
			c.Emoji = map[string]any{"write_through": true}
		} else if m, _, err := image.Decode(bytes.NewReader(data)); err != nil {
			c.Emoji = map[string]any{"err": err.Error()}
		} else {
			c.Emoji = encodePNGOf(enc, mmimaging.Fit(m, 128, 128))
		}
	}
	return c
}

// withEXIFOrientation returns a JPEG with an APP1 EXIF segment carrying orientation o (IFD0,
// little-endian SHORT) inserted after SOI.
func withEXIFOrientation(j []byte, o uint16) []byte {
	le := binary.LittleEndian
	app1 := exifAPP1(tiffSpec{le: true, ifds: [][]ifdEntry{{orientationEntry(le, 3, o)}}}.build())
	out := append([]byte{0xff, 0xd8}, app1...)
	return append(out, j[2:]...)
}

func imagingPipelineStage() (map[string]any, error) {
	var cases []pipelineCase
	for _, f := range pngCorpus() {
		cases = append(cases, runPipeline("png", f.Name, f.Data))
	}
	for _, f := range jpegCorpus() {
		cases = append(cases, runPipeline("jpeg", f.Name, f.Data))
	}
	// A slice of the GIF and BMP corpora rather than all of it: those two stages carry 430 files
	// between them, and what this stage adds over them is the *composition* — Lanczos, the two
	// encoders, the orientation read — which depends on the decoded image's type, palette and
	// geometry, not on which header grammar produced it. The names below are one per decoded
	// shape (each colour-table size, each bit depth, top-down and bottom-up, interlaced, animated,
	// transparent, quantised), which is what the composition can tell apart.
	corpusSlice := func(source string, byName map[string][]byte, want []string) {
		for _, n := range want {
			data, ok := byName[n]
			if !ok {
				panic(source + ": no corpus file " + n)
			}
			cases = append(cases, runPipeline(source, n, data))
		}
	}
	named := func(files []namedFile) map[string][]byte {
		m := map[string][]byte{}
		for _, f := range files {
			m[f.Name] = f.Data
		}
		return m
	}
	// The BMP stage builds its cases inline rather than through a `[]namedFile`, so its bytes
	// come back out of the stage's own output.
	bmpStage, err := imagingBMPStage()
	if err != nil {
		return nil, err
	}
	bmpByName := map[string][]byte{}
	for _, c := range bmpStage["decode"].([]bmpCase) {
		data, derr := decodeB64(c.B64)
		if derr != nil {
			return nil, derr
		}
		bmpByName[c.Name] = data
	}
	corpusSlice("gif", named(gifCorpus()), []string{
		"go_paletted_120x100_p2", "go_paletted_120x100_p16", "go_paletted_120x100_p256",
		"go_paletted_alpha_120x100", "go_quantised_nrgba_256", "go_quantised_gray_4",
		"go_quantised_ycbcr_64", "go_anim_disposals", "go_anim_local_tables",
		"go_anim_transparent", "crafted_interlaced_17", "crafted_gct_bits_0",
		"crafted_gce_transparent_oob_255", "crafted_one_by_one", "crafted_six_frames",
	})
	corpusSlice("bmp", bmpByName, []string{
		"bmp_1bpp.bmp", "bmp_4bpp.bmp", "bmp_8bpp.bmp", "colormap.bmp", "video-001.bmp",
		"yellow_rose-small.bmp", "yellow_rose-small-v5.bmp",
		"go_gray_opaque_13x9", "go_paletted_mixed_40x30", "go_rgba_mixed_5x4",
		"go_nrgba_mixed_64x48", "crafted_baseline_24_topdown", "crafted_baseline_32_topdown",
		"crafted_baseline_8_bottomup", "crafted_paletted_1bpp_33x2_topdown",
	})

	exif, eerr := imagingEXIFStage()
	if eerr != nil {
		return nil, eerr
	}
	for _, c := range exif["cases"].([]exifCase) {
		data, _ := decodeB64(c.B64)
		cases = append(cases, runPipeline("exif", c.Name, data))
	}

	// Photo-sized inputs, inline: the sizes real clients send, across the preview boundary, with
	// every rotation that swaps the axes.
	inline := func(name string, data []byte) {
		pc := runPipeline("inline", name, data)
		pc.B64 = b64(data)
		cases = append(cases, pc)
	}
	photo := imgSpec{"ycbcr", 2400, 1600, "gradient", "opaque", 8001, 0, "420"}.build()
	photoJPEG := mustEncode(func(b *bytes.Buffer) error { return jpeg.Encode(b, photo, &jpeg.Options{Quality: 85}) })
	inline("photo_2400x1600", photoJPEG)
	for _, o := range []uint16{3, 6, 8} {
		inline("photo_2400x1600_o"+itoa(int(o)), withEXIFOrientation(photoJPEG, o))
	}
	portrait := mustEncode(func(b *bytes.Buffer) error {
		return jpeg.Encode(b, imgSpec{"nrgba", 900, 1950, "gradient", "opaque", 8002, 0, ""}.build(), &jpeg.Options{Quality: 90})
	})
	inline("portrait_900x1950", portrait)
	inline("portrait_900x1950_o6", withEXIFOrientation(portrait, 6))
	screenshot := mustEncode(func(b *bytes.Buffer) error {
		return (&png.Encoder{}).Encode(b, imgSpec{"nrgba", 1440, 900, "gradient", "opaque", 8003, 0, ""}.build())
	})
	inline("screenshot_1440x900", screenshot)
	inline("blocks_400x300", mustEncode(func(b *bytes.Buffer) error {
		return (&png.Encoder{}).Encode(b, imgSpec{"nrgba", 400, 300, "blocks", "opaque", 8005, 0, ""}.build())
	}))
	alpha := mustEncode(func(b *bytes.Buffer) error {
		return (&png.Encoder{}).Encode(b, imgSpec{"nrgba", 1000, 300, "gradient", "mixed", 8004, 0, ""}.build())
	})
	inline("alpha_1000x300", alpha)
	// Either side of GeneratePreview's `w > width` and GenerateThumbnail's `width > height`.
	for _, sz := range [][2]int{{1920, 40}, {1921, 40}, {130, 130}} {
		inline("edge_"+itoa(sz[0])+"x"+itoa(sz[1]), mustEncode(func(b *bytes.Buffer) error {
			return (&png.Encoder{}).Encode(b, imgSpec{"nrgba", sz[0], sz[1], "gradient", "opaque", 8006, 0, ""}.build())
		}))
	}
	// A 1920-wide JPEG: GeneratePreview hands back the decoded Y'CbCr image itself, which the
	// JPEG encoder converts differently from the NRGBA a resize would produce.
	inline("edge_1920x40_jpeg", mustEncode(func(b *bytes.Buffer) error {
		return jpeg.Encode(b, imgSpec{"ycbcr", 1920, 40, "smooth", "opaque", 8007, 0, "420"}.build(), &jpeg.Options{Quality: 85})
	}))
	// Photo-sized GIF and BMP, so the two new decoders meet a Lanczos resize that actually
	// resamples rather than one that fits in a thumbnail. The GIF is quantised by Go's own
	// encoder (a *image.Paletted with 256 colours out the other side); the BMP round-trips an
	// NRGBA at 24 bits and a Gray at 8.
	bigPaletted := imgSpec{"nrgba", 1400, 900, "gradient", "opaque", 8008, 0, ""}.build()
	inline("gif_1400x900", mustEncode(func(b *bytes.Buffer) error {
		return gif.Encode(b, bigPaletted, &gif.Options{NumColors: 256})
	}))
	inline("bmp_1400x900_24", mustEncode(func(b *bytes.Buffer) error {
		return bmp.Encode(b, bigPaletted)
	}))
	inline("bmp_2000x120_gray", mustEncode(func(b *bytes.Buffer) error {
		return bmp.Encode(b, imgSpec{"gray", 2000, 120, "smooth", "opaque", 8009, 0, ""}.build())
	}))
	// Animated: `image.Decode` hands back frame 0 alone, so the derived files come from it and
	// not from a composite. Twenty frames, so a port that composited would differ visibly.
	animated := make([]*image.Paletted, 0, 20)
	delays := make([]int, 0, 20)
	for i := range 20 {
		m := image.NewPaletted(image.Rect(0, 0, 300, 200), pipelineGIFPalette())
		for y := range 200 {
			for x := range 300 {
				m.SetColorIndex(x, y, sample("blocks", uint64(8010+i), x, y, 0, 300, 200))
			}
		}
		animated = append(animated, m)
		delays = append(delays, 4)
	}
	inline("gif_animated_300x200", mustEncode(func(b *bytes.Buffer) error {
		return gif.EncodeAll(b, &gif.GIF{Image: animated, Delay: delays, LoopCount: 3})
	}))

	// The 1×1 PNG every parity suite uploads.
	inline("parity_1x1", []byte{
		137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
		8, 2, 0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207,
		192, 0, 0, 3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
	})
	return map[string]any{"cases": cases}, nil
}
