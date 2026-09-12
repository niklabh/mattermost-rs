package main

// Behavioural oracle for the two emoji *writes*: `POST /api/v4/emoji` and
// `DELETE /api/v4/emoji/{emoji_id}`.
//
// Three of the things `createEmoji` does are re-implementations of Go library behaviour rather
// than of Mattermost code, and each of them decides an HTTP status:
//
//   - `image.DecodeConfig` — the width and height that pick "refuse", "resize" or "write through".
//     `crates/mm-app/src/imaging.rs` measures PNG headers itself and hands every other format to
//     Go, so the corpus below has to say which PNGs Go accepts and what dimensions it reads.
//   - `mime.ParseMediaType` and `mime/multipart`'s form reader — which parts become values, which
//     become files, and which are silently dropped. `crates/mm-api/src/multipart.rs`.
//   - `model.NewInfo(filename).MimeType`, which alone decides whether the GIF frame walk runs.
//
// Reading those three out of the standard library and reasoning about them is exactly the way to
// produce a confident, wrong port, so they are measured instead.

import (
	"bytes"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"hash/crc32"
	"image"
	"image/color"
	"image/png"
	"mime"
	"mime/multipart"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/v8/channels/app"

	// The registry `image.DecodeConfig` walks is the whole binary's. These are the six decoders
	// `channels/app` has registered; without them the corpus would see a smaller registry than
	// the server does.
	_ "golang.org/x/image/bmp"
	_ "golang.org/x/image/tiff"
	_ "golang.org/x/image/webp"
	_ "image/gif"
	_ "image/jpeg"
)

func writeEmojiUploadBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":         emojiUploadConstants(),
		"decode_config":     emojiDecodeConfigAll(),
		"mime_by_extension": emojiMimeByExtensionAll(),
		"filepath_base":     emojiFilepathBaseAll(),
		"parse_media_type":  emojiParseMediaTypeAll(),
		"read_form":         emojiReadFormAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_emoji_upload.json"), append(blob, '\n'), 0o644)
}

func emojiUploadConstants() map[string]any {
	return map[string]any{
		"max_emoji_file_size":       app.MaxEmojiFileSize,
		"max_emoji_width":           app.MaxEmojiWidth,
		"max_emoji_height":          app.MaxEmojiHeight,
		"max_emoji_original_width":  app.MaxEmojiOriginalWidth,
		"max_emoji_original_height": app.MaxEmojiOriginalHeight,
		"max_emoji_gif_frames":      app.MaxEmojiGIFFrames,
	}
}

// --- image.DecodeConfig ------------------------------------------------------------------------

// pngChunk frames one PNG chunk: length, type, data, CRC over type+data.
func pngChunk(kind string, data []byte, corruptCRC bool) []byte {
	var b bytes.Buffer
	_ = binary.Write(&b, binary.BigEndian, uint32(len(data)))
	b.WriteString(kind)
	b.Write(data)
	sum := crc32.ChecksumIEEE(append([]byte(kind), data...))
	if corruptCRC {
		sum ^= 0xffffffff
	}
	_ = binary.Write(&b, binary.BigEndian, sum)
	return b.Bytes()
}

// rawPNG emits a signature plus a single IHDR chunk with the fields spelled out, which is all
// `DecodeConfig` reads for a non-paletted image. `length` overrides the IHDR length field so the
// "bad IHDR length" branch is reachable.
func rawPNG(width, height uint32, depth, colorType, compression, filter, interlace byte, corruptCRC bool, length int) []byte {
	ihdr := make([]byte, 13)
	binary.BigEndian.PutUint32(ihdr[0:4], width)
	binary.BigEndian.PutUint32(ihdr[4:8], height)
	ihdr[8] = depth
	ihdr[9] = colorType
	ihdr[10] = compression
	ihdr[11] = filter
	ihdr[12] = interlace

	chunk := pngChunk("IHDR", ihdr, corruptCRC)
	if length >= 0 {
		binary.BigEndian.PutUint32(chunk[0:4], uint32(length))
	}

	var b bytes.Buffer
	b.WriteString("\x89PNG\r\n\x1a\n")
	b.Write(chunk)
	b.Write(pngChunk("IEND", nil, false))
	return b.Bytes()
}

// realPNG is a genuinely encoded image, so the whole file — IDAT and all — is what Go sees.
func realPNG(w, h int) []byte {
	img := image.NewRGBA(image.Rect(0, 0, w, h))
	img.Set(0, 0, color.RGBA{R: 9, G: 8, B: 7, A: 255})
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

// realPalettedPNG exercises the branch `DecodeConfig` does *not* stop at IHDR for.
func realPalettedPNG(w, h int) []byte {
	img := image.NewPaletted(image.Rect(0, 0, w, h), color.Palette{color.Black, color.White})
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

// emojiDecodeConfigAll records width, height and format for a corpus built around the decisions
// `uploadEmojiImage` makes: 128 (resize threshold) and 1028 (refusal threshold), each side of
// each, plus every PNG header field `parseIHDR` can refuse on.
func emojiDecodeConfigAll() []map[string]any {
	corpus := []struct {
		name  string
		bytes []byte
	}{
		{"png_1x1", realPNG(1, 1)},
		{"png_128x128", realPNG(128, 128)},
		{"png_129x128", realPNG(129, 128)},
		{"png_128x129", realPNG(128, 129)},
		{"png_paletted_8x8", realPalettedPNG(8, 8)},

		// Header-only PNGs: DecodeConfig returns as soon as IHDR is read for a non-paletted type.
		{"png_header_64x64", rawPNG(64, 64, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_1028x1028", rawPNG(1028, 1028, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_1029x1028", rawPNG(1029, 1028, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_1028x1029", rawPNG(1028, 1029, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_2000x2000", rawPNG(2000, 2000, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_interlaced", rawPNG(64, 64, 8, 6, 0, 0, 1, false, -1)},
		{"png_header_grayscale_1bit", rawPNG(64, 64, 1, 0, 0, 0, 0, false, -1)},
		{"png_header_grayscale_16bit", rawPNG(64, 64, 16, 0, 0, 0, 0, false, -1)},
		{"png_header_truecolor_16bit", rawPNG(64, 64, 16, 2, 0, 0, 0, false, -1)},
		{"png_header_gray_alpha", rawPNG(64, 64, 8, 4, 0, 0, 0, false, -1)},

		// Every refusal `parseIHDR` can make.
		{"png_header_zero_width", rawPNG(0, 64, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_zero_height", rawPNG(64, 0, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_negative_width", rawPNG(0x80000001, 64, 8, 6, 0, 0, 0, false, -1)},
		{"png_header_bad_compression", rawPNG(64, 64, 8, 6, 1, 0, 0, false, -1)},
		{"png_header_bad_filter", rawPNG(64, 64, 8, 6, 0, 1, 0, false, -1)},
		{"png_header_bad_interlace", rawPNG(64, 64, 8, 6, 0, 0, 2, false, -1)},
		{"png_header_bad_depth", rawPNG(64, 64, 3, 6, 0, 0, 0, false, -1)},
		{"png_header_bad_colour_type", rawPNG(64, 64, 8, 7, 0, 0, 0, false, -1)},
		{"png_header_truecolor_1bit", rawPNG(64, 64, 1, 2, 0, 0, 0, false, -1)},
		{"png_header_bad_crc", rawPNG(64, 64, 8, 6, 0, 0, 0, true, -1)},
		{"png_header_bad_length", rawPNG(64, 64, 8, 6, 0, 0, 0, false, 12)},
		{"png_signature_only", []byte("\x89PNG\r\n\x1a\n")},
		{"png_truncated_ihdr", rawPNG(64, 64, 8, 6, 0, 0, 0, false, -1)[:20]},

		// Not PNG at all: the port measures none of these and hands them to Go.
		{"empty", []byte{}},
		{"garbage", []byte("not an image at all")},
		{"gif_magic_only", []byte("GIF89a")},
		{"jpeg_magic_only", []byte("\xff\xd8")},
	}

	rows := make([]map[string]any, 0, len(corpus))
	for _, tc := range corpus {
		row := map[string]any{
			"name":         tc.name,
			"bytes_base64": base64.StdEncoding.EncodeToString(tc.bytes),
		}
		cfg, format, err := image.DecodeConfig(bytes.NewReader(tc.bytes))
		if err != nil {
			row["ok"] = false
			row["format"] = ""
			row["width"] = 0
			row["height"] = 0
			row["err_is_format"] = errors.Is(err, image.ErrFormat)
		} else {
			row["ok"] = true
			row["format"] = format
			row["width"] = cfg.Width
			row["height"] = cfg.Height
			row["err_is_format"] = false
		}
		rows = append(rows, row)
	}
	return rows
}

// --- model.NewInfo(name).MimeType ---------------------------------------------------------------

// emojiMimeByExtensionAll pins the *one* thing the filename decides in `uploadEmojiImage`:
// `isGIF`. `mime.TypeByExtension` consults the host's /etc/mime.types on Linux as well as Go's
// built-in table, so this corpus is a statement about the machine that generated it — which is
// why the Rust side only ever trusts `.png`, whose built-in mapping cannot be displaced.
func emojiMimeByExtensionAll() []map[string]any {
	names := []string{
		"e.png", "e.PNG", "e.Png", "e.gif", "e.GIF", "e.jpg", "e.jpeg", "e.webp", "e.bmp",
		"e", "", ".png", "e.png.gif", "e.gif.png", "a/b/c.png", "../../e.png",
		"e.tar.gz", "e.txt", "e.",
	}
	rows := make([]map[string]any, 0, len(names))
	for _, name := range names {
		info := model.NewInfo(name)
		rows = append(rows, map[string]any{
			"filename":  name,
			"extension": info.Extension,
			"mime_type": info.MimeType,
			"is_gif":    info.MimeType == "image/gif",
		})
	}
	return rows
}

// --- filepath.Base -------------------------------------------------------------------------------

// emojiFilepathBaseAll pins `Part.FileName`'s last step. A filename that survives the multipart
// reader is `filepath.Base`d, so the extension the GIF test reads can differ from what the client
// sent.
func emojiFilepathBaseAll() []map[string]any {
	corpus := []string{
		"", "e.png", "/e.png", "a/b/e.png", "a/b/", "/", "//", "///",
		"../../etc/passwd", "./e.png", "a/b/.", "..", ".",
		"C:\\windows\\e.png", "e.png/", "e.png//",
	}
	rows := make([]map[string]any, 0, len(corpus))
	for _, in := range corpus {
		rows = append(rows, map[string]any{"in": in, "out": filepath.Base(in)})
	}
	return rows
}

// --- mime.ParseMediaType -------------------------------------------------------------------------

// emojiParseMediaTypeAll covers both headers the multipart port parses: the request's
// `Content-Type` (for the boundary) and each part's `Content-Disposition` (for `name` and
// `filename`). An `ok:false` row is a part Go **drops**, not a request Go refuses — except on
// Content-Type, where it is `ErrNotMultipart`.
func emojiParseMediaTypeAll() []map[string]any {
	corpus := []string{
		"multipart/form-data; boundary=abc",
		"multipart/form-data; boundary=\"abc\"",
		"multipart/form-data;boundary=abc",
		"multipart/form-data; boundary=abc; charset=utf-8",
		"MULTIPART/FORM-DATA; BOUNDARY=AbC",
		"multipart/form-data",
		"multipart/mixed; boundary=abc",
		"application/json",
		"",
		"form-data; name=\"emoji\"",
		"form-data; name=emoji",
		"form-data; name=\"image\"; filename=\"e.png\"",
		"form-data; name=\"image\"; filename=\"\"",
		"form-data; name=\"\"; filename=\"e.png\"",
		"form-data; filename=\"e.png\"",
		"form-data; name=\"a b\"; filename=\"a b.png\"",
		"form-data; name=\"quo\\\"te\"",
		"attachment; name=\"emoji\"",
		"form-data; name",
		"form-data; name=",
		"form-data; name=\"unterminated",
		"form-data; NAME=\"emoji\"",
		"form-data; name=\"emoji\"; name=\"second\"",
	}
	rows := make([]map[string]any, 0, len(corpus))
	for _, in := range corpus {
		row := map[string]any{"in": in}
		mediaType, params, err := mime.ParseMediaType(in)
		if err != nil {
			row["ok"] = false
			row["media_type"] = ""
			row["params"] = map[string]string{}
		} else {
			row["ok"] = true
			row["media_type"] = mediaType
			// Sorted by construction: JSON objects are emitted with sorted keys.
			row["params"] = params
		}
		rows = append(rows, row)
	}
	return rows
}

// --- multipart.Reader.ReadForm -------------------------------------------------------------------

type formCase struct {
	name     string
	boundary string
	body     string
}

// emojiReadFormAll runs whole bodies through `multipart.NewReader(...).ReadForm`, the call
// `ParseMultipartForm` makes, and records the `Form` it produced or the fact that it failed.
//
// The interesting rows are the ones that *drop* a part rather than refusing the body: no `name`,
// a disposition that is not `form-data`, a `Content-Disposition` that does not parse. A port that
// refused any of them would 400 a request Go answers 200 to.
func emojiReadFormAll() []map[string]any {
	const b = "BOUNDARY"
	part := func(disposition, body string) string {
		return "--" + b + "\r\nContent-Disposition: " + disposition + "\r\n\r\n" + body + "\r\n"
	}
	close := "--" + b + "--\r\n"

	corpus := []formCase{
		{"one_value", b, part(`form-data; name="emoji"`, `{"name":"x"}`) + close},
		{"one_file", b, part(`form-data; name="image"; filename="e.png"`, "\x89PNG") + close},
		{
			"the_shape_the_webapp_sends", b,
			"--" + b + "\r\nContent-Disposition: form-data; name=\"image\"; filename=\"e.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNGbytes\r\n" +
				part(`form-data; name="emoji"`, `{"name":"x","creator_id":"y"}`) + close,
		},
		{"empty_filename_is_a_value", b, part(`form-data; name="image"; filename=""`, "data") + close},
		{"no_name_is_dropped", b, part(`form-data; filename="e.png"`, "data") + close},
		{"empty_name_is_dropped", b, part(`form-data; name=""`, "data") + close},
		{"not_form_data_is_dropped", b, part(`attachment; name="emoji"`, "data") + close},
		{"unparseable_disposition_is_dropped", b, part(`form-data; name="unterminated`, "data") + close},
		{"no_disposition_at_all_is_dropped", b, "--" + b + "\r\n\r\ndata\r\n" + close},
		{"repeated_name_keeps_both", b, part(`form-data; name="emoji"`, "first") + part(`form-data; name="emoji"`, "second") + close},
		{"filename_is_based", b, part(`form-data; name="image"; filename="../../e.png"`, "d") + close},
		{"preamble_is_discarded", b, "this is a preamble\r\n" + part(`form-data; name="emoji"`, "v") + close},
		{"epilogue_is_discarded", b, part(`form-data; name="emoji"`, "v") + close + "trailing junk\r\n"},
		{"empty_body_part", b, part(`form-data; name="emoji"`, "") + close},
		{"binary_body_with_crlf", b, part(`form-data; name="image"; filename="e.bin"`, "a\r\nb\r\nc") + close},
		{"lf_only_delimiters", b, "--" + b + "\nContent-Disposition: form-data; name=\"emoji\"\n\nv\n--" + b + "--\n"},
		{"no_closing_delimiter", b, part(`form-data; name="emoji"`, "v")},
		{"no_delimiter_at_all", b, "just some bytes\r\n"},
		{"empty", b, ""},
		{"extra_header_is_kept", b, "--" + b + "\r\nContent-Disposition: form-data; name=\"emoji\"\r\nX-Thing: 1\r\n\r\nv\r\n" + close},
		{"boundary_with_trailing_space", b, "--" + b + " \r\nContent-Disposition: form-data; name=\"emoji\"\r\n\r\nv\r\n" + close},
	}

	rows := make([]map[string]any, 0, len(corpus))
	for _, tc := range corpus {
		row := map[string]any{
			"name":         tc.name,
			"boundary":     tc.boundary,
			"body_base64":  base64.StdEncoding.EncodeToString([]byte(tc.body)),
		}
		form, err := multipart.NewReader(strings.NewReader(tc.body), tc.boundary).ReadForm(1 << 20)
		if err != nil {
			row["ok"] = false
			row["values"] = map[string][]string{}
			row["files"] = []map[string]any{}
			rows = append(rows, row)
			continue
		}
		row["ok"] = true
		values := map[string][]string{}
		for k, v := range form.Value {
			values[k] = v
		}
		row["values"] = values

		files := []map[string]any{}
		names := make([]string, 0, len(form.File))
		for k := range form.File {
			names = append(names, k)
		}
		sort.Strings(names)
		for _, k := range names {
			for i, fh := range form.File[k] {
				files = append(files, map[string]any{
					"name":     k,
					"index":    i,
					"filename": fh.Filename,
					"size":     fh.Size,
				})
			}
		}
		row["files"] = files
		_ = form.RemoveAll()
		rows = append(rows, row)
	}
	_ = fmt.Sprint()
	return rows
}
