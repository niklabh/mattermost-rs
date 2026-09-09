package main

// Behavioural oracle for the **local file backend and the file-bytes wire format**, written to
// fixtures/behaviour_filestore.json.
//
// Three things a Rust port has to reimplement, none of which has a Rust equivalent:
//
//   - `path/filepath`'s `Base`, `Dir` and the two- and three-element `Join`. `LocalFileBackend`
//     builds every real path with `filepath.Join(b.directory, path)`, and `ListExports` /
//     `ListImports` reduce a listing with `filepath.Base`. Join *cleans*, so a `..` inside the
//     path argument escapes the backend's directory — recorded rather than assumed, because that
//     is a security property and a guess either way would be silent.
//   - `web.WriteFileResponse`, which is `setHeaders` plus `http.ServeContent`. This is the whole
//     wire format of `GET /files/{file_id}` and its four siblings: content type coercion,
//     `Content-Disposition` with `url.PathEscape`d filenames, `Range` handling, `Last-Modified`,
//     and the conditional-request answers. Driven end to end through `httptest` so the corpus
//     records what a client actually receives, not what the source looks like it does.
//   - `LocalFileBackend` itself, run against a real temporary directory: what `ListDirectory`
//     returns for a missing path, what `ListDirectoryRecursively` does with nesting, and whether
//     `FileExists` distinguishes a missing file from a broken one.
//
// Determinism: fixed corpora, a fixed modification time, and a temporary directory whose name is
// never recorded. No rand, no time.Now — see [D-032].

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/gif"
	"image/jpeg"
	"image/png"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	_ "golang.org/x/image/bmp"
	_ "golang.org/x/image/tiff"
	_ "golang.org/x/image/webp"

	"github.com/mattermost/mattermost/server/v8/channels/app"
	"github.com/mattermost/mattermost/server/v8/platform/shared/filestore"
	"github.com/mattermost/mattermost/server/v8/platform/shared/web"
)

func writeFilestoreBehaviourFixture(outDir string) error {
	backend, cleanup, err := localBackendCorpus()
	if err != nil {
		return err
	}
	defer cleanup()

	out := map[string]any{
		"filepath_base":       filepathBaseAll(),
		"filepath_dir":        filepathDirAll(),
		"filepath_join2":      filepathJoin2All(),
		"filepath_join3":      filepathJoin3All(),
		"write_file_response": writeFileResponseAll(),
		"local_backend":       backend,
		"image_decode_config": imageDecodeConfigAll(),
		"public_link_hash":    publicLinkHashAll(),
		"unsafe_content_types": []string(func() []string {
			s := make([]string, 0, len(web.UnsafeContentTypes))
			s = append(s, web.UnsafeContentTypes[:]...)
			return s
		}()),
		"media_content_types": []string(func() []string {
			s := make([]string, 0, len(web.MediaContentTypes))
			s = append(s, web.MediaContentTypes[:]...)
			return s
		}()),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_filestore.json"), append(blob, '\n'), 0o644)
}

// --- path/filepath -----------------------------------------------------------------------------

// filepathCorpus covers what `Base` and `Dir` branch on — trailing separators, runs of them, a
// bare separator, a relative name with no separator at all — plus the export/import shapes the
// two listing functions reduce.
var filepathCorpus = []string{
	"", ".", "..", "/", "//", "///",
	"a", "a/", "/a", "/a/", "a/b", "a/b/", "/a/b/c",
	"a//b", "a/./b", "a/../b",
	"./export.zip", "data/export.zip", "/mattermost/data/export.zip",
	"import/bulk.zip.tmp", "export.zip/", "/export.zip//",
}

func filepathBaseAll() []map[string]any {
	rows := make([]map[string]any, 0, len(filepathCorpus))
	for _, in := range filepathCorpus {
		rows = append(rows, map[string]any{"in": in, "out": filepath.Base(in)})
	}
	return rows
}

func filepathDirAll() []map[string]any {
	rows := make([]map[string]any, 0, len(filepathCorpus))
	for _, in := range filepathCorpus {
		rows = append(rows, map[string]any{"in": in, "out": filepath.Dir(in)})
	}
	return rows
}

// join2Corpus is the shape `LocalFileBackend` uses on every call: the configured directory and a
// caller-supplied relative path. The `..` rows are the ones that matter — `Join` cleans, so the
// result can name a file *outside* the directory, and a port that concatenated instead would
// differ exactly there.
var join2Corpus = [][2]string{
	{"", ""}, {"", "a"}, {"./data", ""}, {"./data", "a"},
	{"./data", "users/abc/profile.png"},
	{"./data", "/users/abc/profile.png"},
	{"./data/", "users//abc///profile.png"},
	{"/mattermost/data", "emoji/xyz/image"},
	{"./data", "../escape"}, {"./data", "../../escape"},
	{"./data", "a/../b"}, {"./data", "./a"},
	{"/", "a"}, {"/", "../a"},
	{"./data", "brand/image"},
	{"./data", "teams/tid/teamIcon.png"},
}

func filepathJoin2All() []map[string]any {
	rows := make([]map[string]any, 0, len(join2Corpus))
	for _, pair := range join2Corpus {
		rows = append(rows, map[string]any{
			"a":   pair[0],
			"b":   pair[1],
			"out": filepath.Join(pair[0], pair[1]),
		})
	}
	return rows
}

// join3Corpus is `filepath.Join(*Config().ExportSettings.Directory, name)` reached through a
// backend directory — three elements, because that is what `downloadExport` composes before the
// backend joins its own directory on the front.
var join3Corpus = [][3]string{
	{"./data", "export", "job.zip"},
	{"./data", "export", "../../etc/passwd"},
	{"./data", "", "job.zip"},
	{"", "export", "job.zip"},
	{"./data", "export/", "/job.zip"},
}

func filepathJoin3All() []map[string]any {
	rows := make([]map[string]any, 0, len(join3Corpus))
	for _, triple := range join3Corpus {
		rows = append(rows, map[string]any{
			"a":   triple[0],
			"b":   triple[1],
			"c":   triple[2],
			"out": filepath.Join(triple[0], triple[1], triple[2]),
		})
	}
	return rows
}

// --- web.WriteFileResponse ----------------------------------------------------------------------

// responseBody is the 26 bytes every WriteFileResponse case serves. Fixed, distinctive and long
// enough that a range can name a middle slice that is wrong under an off-by-one.
var responseBody = []byte("abcdefghijklmnopqrstuvwxyz")

// modTimeFixed is the `time.Unix(0, UpdateAt*1e6)` a real FileInfo produces, pinned to one
// instant so the recorded `Last-Modified` never moves. 2021-03-04T05:06:07Z.
var modTimeFixed = time.Unix(1614834367, 0).UTC()

type writeFileResponseCase struct {
	Name          string
	Filename      string
	ContentType   string
	ContentSize   int64
	ModTime       time.Time
	WebserverMode string
	ForceDownload bool
	Method        string
	ReqHeaders    map[string]string
}

// writeFileResponseCorpus walks every branch in `setHeaders` and the ones in `http.ServeContent`
// a file route can reach.
//
// `setHeaders`: empty content type becomes octet-stream; an unsafe type is rewritten to
// text/plain by *prefix*, so a charset parameter still matches; a media type is served inline and
// everything else as an attachment, unless forceDownload overrides. The filename is
// `url.PathEscape`d into both the plain and the `filename*=UTF-8''` form — which is why a space,
// a quote and a non-ASCII name are all here.
//
// `ServeContent`: zero and Unix-epoch modtimes suppress `Last-Modified` entirely; a
// contentSize > 0 pre-sets `Content-Length` and gzip mode moves it to
// `X-Uncompressed-Content-Length`; and the Range family covers a prefix, a suffix, a middle
// slice, an unsatisfiable start, two ranges (multipart, whose boundary is random and therefore
// only its *status* is compared) and a malformed header.
var writeFileResponseCorpus = []writeFileResponseCase{
	{Name: "plain_text_attachment", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "empty_content_type", Filename: "blob.bin", ContentType: "", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "image_inline", Filename: "pic.png", ContentType: "image/png", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "image_forced_download", Filename: "pic.png", ContentType: "image/png", ContentSize: 26, ModTime: modTimeFixed, ForceDownload: true, Method: http.MethodGet},
	{Name: "unsafe_html", Filename: "page.html", ContentType: "text/html", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "unsafe_html_charset", Filename: "page.html", ContentType: "text/html; charset=utf-8", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "unsafe_javascript", Filename: "app.js", ContentType: "application/javascript", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "media_prefix_match", Filename: "clip.mp4", ContentType: "video/mp4; codecs=avc1", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "filename_with_space", Filename: "my report.pdf", ContentType: "application/pdf", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "filename_with_quote", Filename: `he said "hi".txt`, ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "filename_unicode", Filename: "réunion-日本.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "filename_with_slash", Filename: "a/b.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "filename_empty", Filename: "", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "size_zero", Filename: "thumb.png", ContentType: "image/png", ContentSize: 0, ModTime: modTimeFixed, Method: http.MethodGet},
	{Name: "gzip_mode", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, WebserverMode: "gzip", Method: http.MethodGet},
	{Name: "modtime_zero", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, Method: http.MethodGet},
	{Name: "modtime_unix_epoch", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: time.Unix(0, 0).UTC(), Method: http.MethodGet},
	{Name: "head_request", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodHead},
	{Name: "range_prefix", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=0-4"}},
	{Name: "range_middle", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=10-19"}},
	{Name: "range_open_ended", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=20-"}},
	{Name: "range_suffix", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=-5"}},
	{Name: "range_past_end", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=20-99"}},
	{Name: "range_unsatisfiable", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=100-200"}},
	{Name: "range_malformed", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "chunks=1-2"}},
	{Name: "range_multiple", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=0-4,10-14"}},
	{Name: "range_head", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodHead, ReqHeaders: map[string]string{"Range": "bytes=0-4"}},
	{Name: "if_modified_since_equal", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"If-Modified-Since": modTimeFixed.UTC().Format(http.TimeFormat)}},
	{Name: "if_modified_since_older", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"If-Modified-Since": modTimeFixed.Add(-time.Hour).UTC().Format(http.TimeFormat)}},
	{Name: "if_unmodified_since_older", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"If-Unmodified-Since": modTimeFixed.Add(-time.Hour).UTC().Format(http.TimeFormat)}},
	{Name: "if_none_match_star", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"If-None-Match": "*"}},
	{Name: "if_match_star", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"If-Match": "*"}},
	{Name: "if_range_matching_date", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=0-4", "If-Range": modTimeFixed.UTC().Format(http.TimeFormat)}},
	{Name: "if_range_stale_date", Filename: "notes.txt", ContentType: "text/plain", ContentSize: 26, ModTime: modTimeFixed, Method: http.MethodGet, ReqHeaders: map[string]string{"Range": "bytes=0-4", "If-Range": modTimeFixed.Add(-time.Hour).UTC().Format(http.TimeFormat)}},
}

func writeFileResponseAll() []map[string]any {
	rows := make([]map[string]any, 0, len(writeFileResponseCorpus))
	for _, tc := range writeFileResponseCorpus {
		req := httptest.NewRequest(tc.Method, "/api/v4/files/fileid", nil)
		for k, v := range tc.ReqHeaders {
			req.Header.Set(k, v)
		}
		rec := httptest.NewRecorder()

		web.WriteFileResponse(
			tc.Filename,
			tc.ContentType,
			tc.ContentSize,
			tc.ModTime,
			tc.WebserverMode,
			strings.NewReader(string(responseBody)),
			tc.ForceDownload,
			rec,
			req,
		)

		res := rec.Result()
		headers := map[string]string{}
		for k, v := range res.Header {
			headers[k] = strings.Join(v, ", ")
		}

		row := map[string]any{
			"name":            tc.Name,
			"filename":        tc.Filename,
			"content_type_in": tc.ContentType,
			"content_size":    tc.ContentSize,
			"modtime_unix_ms": modTimeMillis(tc.ModTime),
			"webserver_mode":  tc.WebserverMode,
			"force_download":  tc.ForceDownload,
			"method":          tc.Method,
			"req_headers":     sortedHeaderPairs(tc.ReqHeaders),
			"status":          res.StatusCode,
			"headers":         headers,
			"body_base64":     base64.StdEncoding.EncodeToString(rec.Body.Bytes()),
		}
		rows = append(rows, row)
	}
	return rows
}

// modTimeMillis renders the modification time the way a FileInfo carries it — epoch
// milliseconds — with the zero time recorded as a JSON null rather than as a very negative
// number, since "no modification time" is the branch that suppresses `Last-Modified`.
func modTimeMillis(t time.Time) any {
	if t.IsZero() {
		return nil
	}
	return t.UnixNano() / int64(time.Millisecond)
}

func sortedHeaderPairs(h map[string]string) []map[string]string {
	keys := make([]string, 0, len(h))
	for k := range h {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	rows := make([]map[string]string, 0, len(keys))
	for _, k := range keys {
		rows = append(rows, map[string]string{"name": k, "value": h[k]})
	}
	return rows
}

// --- LocalFileBackend ---------------------------------------------------------------------------

// localBackendCorpus builds a real directory tree under a temporary root, drives
// `LocalFileBackend` over it, and records the answers. The tree is fixed; only its root is
// temporary, and the root never reaches the fixture.
//
//	<root>/
//	  brand/image                 6 bytes
//	  emoji/abc/image             3 bytes
//	  export/                     (a directory, so it appears in a non-recursive listing)
//	  export/job.zip              4 bytes
//	  export/nested/deep.zip      2 bytes
//	  users/uid/profile.png       5 bytes
func localBackendCorpus() (map[string]any, func(), error) {
	root, err := os.MkdirTemp("", "mmrs-filestore-oracle")
	if err != nil {
		return nil, func() {}, err
	}
	cleanup := func() { os.RemoveAll(root) }

	files := map[string]string{
		"brand/image":            "brandi",
		"emoji/abc/image":        "gif",
		"export/job.zip":         "zipz",
		"export/nested/deep.zip": "dz",
		"users/uid/profile.png":  "pngpn",
	}
	names := make([]string, 0, len(files))
	for name := range files {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		full := filepath.Join(root, name)
		if err := os.MkdirAll(filepath.Dir(full), 0o750); err != nil {
			return nil, cleanup, err
		}
		if err := os.WriteFile(full, []byte(files[name]), 0o600); err != nil {
			return nil, cleanup, err
		}
	}

	backend, err := filestore.NewFileBackend(filestore.FileBackendSettings{
		DriverName: "local",
		Directory:  root,
	})
	if err != nil {
		return nil, cleanup, err
	}

	out := map[string]any{
		"driver_name": backend.DriverName(),
		"tree":        files,
	}

	// ReadFile / FileExists / FileSize over present, absent, directory and escaping paths.
	readCorpus := []string{
		"brand/image", "emoji/abc/image", "users/uid/profile.png",
		"missing/file", "export", "", "/brand/image", "./brand/image",
		"emoji/abc/../abc/image", "../etc/passwd",
		// ENOTDIR rather than ENOENT: `brand/image` is a file, so it cannot be a directory
		// component. `os.IsNotExist` is **false** for ENOTDIR — `syscall.Errno.Is` maps only
		// ENOENT to `ErrNotExist` — so `FileExists` surfaces an *error* here rather than
		// answering `false, nil`. Recorded because the opposite is the intuitive reading.
		"brand/image/deeper",
	}
	readRows := make([]map[string]any, 0, len(readCorpus))
	for _, p := range readCorpus {
		row := map[string]any{"path": p}

		data, rerr := backend.ReadFile(p)
		if rerr != nil {
			row["read_ok"] = false
			row["read_error_contains"] = errorShape(rerr)
		} else {
			row["read_ok"] = true
			row["read_bytes"] = string(data)
		}

		exists, eerr := backend.FileExists(p)
		if eerr != nil {
			row["exists_ok"] = false
			row["exists_error_contains"] = errorShape(eerr)
		} else {
			row["exists_ok"] = true
			row["exists"] = exists
		}

		size, serr := backend.FileSize(p)
		if serr != nil {
			row["size_ok"] = false
		} else {
			row["size_ok"] = true
			row["size"] = size
		}

		readRows = append(readRows, row)
	}
	out["reads"] = readRows

	// ListDirectory and ListDirectoryRecursively. Both return paths **prefixed with the argument**
	// and never with the backend directory, and both answer an empty list — not an error — for a
	// path that does not exist.
	listCorpus := []string{"", ".", "export", "export/", "emoji", "users", "missing", "/export",
		// A *file*, not a directory. `os.ReadDir` fails with ENOTDIR, which `os.IsNotExist`
		// does **not** match, so both listings return an error — unlike a path that simply does
		// not exist, which returns an empty slice and no error.
		"brand/image"}
	listRows := make([]map[string]any, 0, len(listCorpus))
	for _, p := range listCorpus {
		row := map[string]any{"path": p}
		flat, ferr := backend.ListDirectory(p)
		row["list_ok"] = ferr == nil
		sort.Strings(flat)
		row["list"] = flat

		deep, derr := backend.ListDirectoryRecursively(p)
		row["list_recursive_ok"] = derr == nil
		sort.Strings(deep)
		row["list_recursive"] = deep
		listRows = append(listRows, row)
	}
	out["lists"] = listRows

	// TestConnection writes and removes /testfile. Recorded as the answer plus the fact that it
	// leaves nothing behind, which is the part a port can get wrong invisibly.
	tcErr := backend.TestConnection()
	leftover, _ := backend.FileExists(filestore.TestFilePath)
	out["test_connection"] = map[string]any{
		"ok":                tcErr == nil,
		"testfile_leftover": leftover,
	}

	return out, cleanup, nil
}

// errorShape reduces a wrapped `*os.PathError` to the parts a port can reproduce: the operation
// and the errno text. The absolute path is deliberately dropped — it contains the temporary root,
// which would make the fixture non-deterministic.
func errorShape(err error) string {
	msg := err.Error()
	switch {
	case strings.Contains(msg, "no such file or directory"):
		return "no such file or directory"
	case strings.Contains(msg, "not a directory"):
		return "not a directory"
	case strings.Contains(msg, "is a directory"):
		return "is a directory"
	default:
		return fmt.Sprintf("<unclassified: %T>", err)
	}
}


// --- image.DecodeConfig ---------------------------------------------------------------------

// imageDecodeConfigAll records what `image.DecodeConfig` says about a corpus of byte strings.
//
// `GetEmojiImage` (app/emoji.go:286) calls it purely for the **format name**, which becomes the
// `Content-Type: image/<name>` of `GET /emoji/{emoji_id}/image`. The name comes from the
// registered decoder whose magic prefix matched, and the set of registered decoders is a property
// of the whole binary, not of emoji.go: png, jpeg and gif from the standard library, plus bmp,
// tiff and webp from `golang.org/x/image` (registered by `app/imaging/decode.go`). Those six
// imports are reproduced above so this corpus sees the same registry the server does.
//
// Two answers are recorded per case, and the second is the one that matters: `err_is_format`
// says the error was `image.ErrFormat` — *no registered magic matched* — as opposed to a decoder
// having claimed the bytes and then failed on them. A Rust port that sniffs magic bytes can
// reproduce the first exactly and the second not at all, so the corpus has to distinguish them.
func imageDecodeConfigAll() []map[string]any {
	real1x1 := func(encode func(*bytes.Buffer) error) []byte {
		var buf bytes.Buffer
		if err := encode(&buf); err != nil {
			panic(err)
		}
		return buf.Bytes()
	}

	img := image.NewRGBA(image.Rect(0, 0, 1, 1))
	img.Set(0, 0, color.RGBA{R: 1, G: 2, B: 3, A: 255})
	paletted := image.NewPaletted(image.Rect(0, 0, 1, 1), color.Palette{color.Black, color.White})

	corpus := []struct {
		name  string
		bytes []byte
	}{
		{"png_1x1", real1x1(func(b *bytes.Buffer) error { return png.Encode(b, img) })},
		{"jpeg_1x1", real1x1(func(b *bytes.Buffer) error { return jpeg.Encode(b, img, nil) })},
		{"gif_1x1", real1x1(func(b *bytes.Buffer) error { return gif.Encode(b, paletted, nil) })},

		// Magic prefixes with nothing behind them. Each should be claimed by its decoder and then
		// fail — err_is_format false — which is what tells a sniffing port that the prefix is the
		// right one.
		{"png_magic_only", []byte("\x89PNG\r\n\x1a\n")},
		{"jpeg_magic_only", []byte("\xff\xd8")},
		{"gif87_magic_only", []byte("GIF87a")},
		{"gif89_magic_only", []byte("GIF89a")},
		{"bmp_magic_only", []byte("BM\x00\x00\x00\x00")},
		{"tiff_le_magic_only", []byte("II*\x00")},
		{"tiff_be_magic_only", []byte("MM\x00*")},
		{"webp_magic_only", []byte("RIFF\x00\x00\x00\x00WEBPVP8 ")},

		// Nothing registered claims these.
		{"empty", []byte{}},
		{"garbage", []byte("not an image at all")},
		{"svg", []byte(`<svg xmlns="http://www.w3.org/2000/svg"></svg>`)},
		{"zip", []byte("PK\x03\x04")},
		{"pdf", []byte("%PDF-1.7\n")},
		// One byte short of the PNG signature: a prefix match must be exact.
		{"png_truncated_magic", []byte("\x89PNG\r\n\x1a")},
	}

	rows := make([]map[string]any, 0, len(corpus))
	for _, tc := range corpus {
		row := map[string]any{
			"name":         tc.name,
			"bytes_base64": base64.StdEncoding.EncodeToString(tc.bytes),
		}
		_, format, err := image.DecodeConfig(bytes.NewReader(tc.bytes))
		if err != nil {
			row["ok"] = false
			row["format"] = ""
			row["err_is_format"] = errors.Is(err, image.ErrFormat)
		} else {
			row["ok"] = true
			row["format"] = format
			row["err_is_format"] = false
		}
		rows = append(rows, row)
	}
	return rows
}


// --- app.GeneratePublicLinkHash -----------------------------------------------------------------

// publicLinkHashAll records `app.GeneratePublicLinkHash` (app/file.go:606) — SHA-256 over the
// salt **then** the file id, base64'd with `RawURLEncoding`.
//
// Three details a reimplementation can get wrong and none of them would show up in a smoke test,
// because a wrong hash simply means every public link 400s: the operand order (salt first), the
// alphabet (URL-safe, `-` and `_`), and the padding (**raw**, no `=`). `getPublicFile` compares
// the result with `subtle.ConstantTimeCompare`, so this is a security boundary as well as a wire
// format.
func publicLinkHashAll() []map[string]any {
	corpus := [][2]string{
		{"", ""},
		{"fileid", ""},
		{"", "salt"},
		{"c5r6bi1z4jbcbjbwyj7uzcgtqy", "mmrsparityxxxxxxxxxxxxxxxxxxxxxx"},
		{"c5r6bi1z4jbcbjbwyj7uzcgtqy", "a-different-salt"},
		// Swapped, to record that the order matters — the two rows must differ.
		{"mmrsparityxxxxxxxxxxxxxxxxxxxxxx", "c5r6bi1z4jbcbjbwyj7uzcgtqy"},
		{"日本", "🧂"},
	}
	rows := make([]map[string]any, 0, len(corpus))
	for _, pair := range corpus {
		rows = append(rows, map[string]any{
			"file_id": pair[0],
			"salt":    pair[1],
			"hash":    app.GeneratePublicLinkHash(pair[0], pair[1]),
		})
	}
	return rows
}
