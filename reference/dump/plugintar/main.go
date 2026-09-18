package main

// `plugintar corpus <dir>` writes a corpus of plugin bundles, one `<case>.tar.gz` per case.
// `plugintar extract <corpus> <work>` runs Go's own `extractTarGz` over each into
// `<work>/<case>/out` and prints, as JSON, whether it failed and every entry under
// `<work>/<case>` afterwards, so an escape into a sibling directory shows too.
//
// `extractTarGz` is unexported in `channels/app`, so the test that runs this copies
// `server/channels/app/extract_plugin_tar.go` in beside it as `zz_extract_plugin_tar.go`, with
// only the package clause changed. The oracle is the pinned source, not a transcription.
//
// It is the Go half of `mm-app`'s `plugin_install::go_parity`.

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

type entry struct {
	Name     string
	Typeflag byte
	Mode     int64
	Body     string
	Linkname string
	Format   tar.Format
	PAX      map[string]string
}

func file(name, body string, mode int64) entry {
	return entry{Name: name, Typeflag: tar.TypeReg, Mode: mode, Body: body}
}

func dir(name string, mode int64) entry {
	return entry{Name: name, Typeflag: tar.TypeDir, Mode: mode}
}

func tarball(entries []entry) []byte {
	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	for _, e := range entries {
		hdr := &tar.Header{
			Name:       e.Name,
			Typeflag:   e.Typeflag,
			Mode:       e.Mode,
			Size:       int64(len(e.Body)),
			Linkname:   e.Linkname,
			Format:     e.Format,
			PAXRecords: e.PAX,
		}
		if e.Typeflag != tar.TypeReg && e.Typeflag != tar.TypeRegA {
			hdr.Size = 0
		}
		if err := tw.WriteHeader(hdr); err != nil {
			panic(fmt.Sprintf("%s: %v", e.Name, err))
		}
		if hdr.Size > 0 {
			if _, err := tw.Write([]byte(e.Body)); err != nil {
				panic(err)
			}
		}
	}
	if err := tw.Close(); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

func gz(data []byte) []byte {
	var buf bytes.Buffer
	zw := gzip.NewWriter(&buf)
	if _, err := zw.Write(data); err != nil {
		panic(err)
	}
	if err := zw.Close(); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

// rawTypeflag rewrites the typeflag byte of the header block at `offset` and fixes its checksum,
// for flags archive/tar will not write (the legacy regular file, '\x00').
func rawTypeflag(data []byte, offset int, flag byte) {
	block := data[offset : offset+512]
	block[156] = flag
	copy(block[148:156], "        ")
	sum := 0
	for _, b := range block {
		sum += int(b)
	}
	copy(block[148:156], fmt.Sprintf("%06o\x00 ", sum))
}

func corpus() map[string][]byte {
	long := strings.Repeat("d", 60) + "/" + strings.Repeat("f", 90) + ".txt"
	cases := map[string][]byte{
		"plain": gz(tarball([]entry{
			dir("p/", 0o755),
			file("p/plugin.json", `{"id":"p"}`, 0o644),
			file("p/server/bin", "#!/bin/sh\n", 0o755),
			file("p/a/b/c.txt", "deep", 0o644),
		})),
		"links": gz(tarball([]entry{
			{Name: "l", Typeflag: tar.TypeSymlink, Linkname: "/etc/passwd"},
			file("r", "target", 0o644),
			{Name: "h", Typeflag: tar.TypeLink, Linkname: "r"},
			{Name: "fifo", Typeflag: tar.TypeFifo, Mode: 0o644},
			file("after.txt", "after", 0o644),
		})),
		"escape": gz(tarball([]entry{
			file("ok.txt", "ok", 0o644),
			file("../escaped.txt", "out", 0o644),
			file("never.txt", "never", 0o644),
		})),
		// The destination is `<case>/out`: a name leading to `<case>/outx` passes Go's
		// strings.HasPrefix check and is written outside the destination.
		"sibling": gz(tarball([]entry{
			file("../outx/f.txt", "sibling", 0o644),
			file("in.txt", "in", 0o644),
		})),
		"absolute": gz(tarball([]entry{file("/abs/f.txt", "abs", 0o644)})),
		"dotdir":   gz(tarball([]entry{file("./x/./y.txt", "dot", 0o644), dir("./", 0o755)})),
		"paxlong":  gz(tarball([]entry{{Name: long, Typeflag: tar.TypeReg, Mode: 0o644, Body: "pax", Format: tar.FormatPAX}})),
		"gnulong":  gz(tarball([]entry{{Name: long, Typeflag: tar.TypeReg, Mode: 0o644, Body: "gnu", Format: tar.FormatGNU}})),
		"paxrecords": gz(tarball([]entry{{
			Name: "short.txt", Typeflag: tar.TypeReg, Mode: 0o644, Body: "renamed",
			PAX: map[string]string{"path": "renamed/by/pax.txt"},
		}})),
		"global": gz(tarball([]entry{
			{Name: "global", Typeflag: tar.TypeXGlobalHeader, PAX: map[string]string{"comment": "g"}},
			file("g.txt", "after global", 0o644),
		})),
		"modes": gz(tarball([]entry{
			file("setuid", "s", 0o4755),
			file("private", "p", 0o600),
			file("open", "o", 0o777),
			file("none", "n", 0),
			dir("closed/", 0o700),
			file("closed/in.txt", "in", 0o644),
		})),
		"dup": gz(tarball([]entry{
			file("d.txt", "the long first content", 0o644),
			file("d.txt", "short", 0o600),
		})),
		"dirtwice": gz(tarball([]entry{dir("t/", 0o755), dir("t/", 0o755), file("t/f", "f", 0o644)})),
		"fileoverdir": gz(tarball([]entry{
			file("z", "file", 0o644),
			dir("z/", 0o755),
			file("z/w", "never", 0o644),
		})),
		"diroverfile": gz(tarball([]entry{dir("y/", 0o755), file("y", "onto a dir", 0o644)})),
		"empty":       gz(tarball(nil)),
		// A directory whose parent was never declared: os.Mkdir is not MkdirAll.
		"orphandir": gz(tarball([]entry{file("first.txt", "first", 0o644), dir("a/b/", 0o755), file("never.txt", "never", 0o644)})),
		"notgzip":   []byte("this is not gzip at all"),
		"nottar":    gz([]byte("gzip around something that is not a tar archive, long enough to fill a block?")),
	}

	// The legacy regular file ('\x00'), and the legacy directory: '\x00' with a trailing slash.
	rega := tarball([]entry{dir("legacy/", 0o755), file("rega.txt", "rega", 0o644)})
	rawTypeflag(rega, 0, 0)
	rawTypeflag(rega, 512, 0)
	cases["rega"] = gz(rega)

	// A tar cut off in the middle of its second file: the first is written, the second fails.
	whole := tarball([]entry{file("first.txt", "first", 0o644), file("second.txt", strings.Repeat("x", 2000), 0o644)})
	cases["truncated"] = gz(whole[:512+512+512+700])

	// One tar stream split across two gzip members, which Go's reader joins.
	two := tarball([]entry{file("m1.txt", "member one", 0o644), file("m2.txt", "member two", 0o644)})
	cases["multistream"] = append(gz(two[:1024]), gz(two[1024:])...)

	// Trailing garbage after the end-of-archive blocks is never read.
	cases["trailing"] = gz(append(tarball([]entry{file("t.txt", "t", 0o644)}), []byte("garbage after the end")...))
	return cases
}

type found struct {
	Path    string `json:"path"`
	Type    string `json:"type"`
	Mode    string `json:"mode"`
	Content string `json:"content,omitempty"`
}

type outcome struct {
	Failed  bool    `json:"failed"`
	Error   string  `json:"error,omitempty"`
	Entries []found `json:"entries"`
}

func walk(root string) []found {
	out := []found{}
	_ = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil || path == root {
			return nil
		}
		rel, _ := filepath.Rel(root, path)
		info, err := os.Lstat(path)
		if err != nil {
			return nil
		}
		f := found{Path: rel, Mode: fmt.Sprintf("%04o", info.Mode().Perm())}
		switch {
		case info.IsDir():
			f.Type = "dir"
		case info.Mode()&fs.ModeSymlink != 0:
			f.Type = "symlink"
		default:
			f.Type = "file"
			body, _ := os.ReadFile(path)
			f.Content = string(body)
		}
		out = append(out, f)
		return nil
	})
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
	return out
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: plugintar corpus <dir> | extract <corpus> <work>")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "corpus":
		for name, data := range corpus() {
			if err := os.WriteFile(filepath.Join(os.Args[2], name+".tar.gz"), data, 0o644); err != nil {
				panic(err)
			}
		}
	case "extract":
		names, err := filepath.Glob(filepath.Join(os.Args[2], "*.tar.gz"))
		if err != nil {
			panic(err)
		}
		results := map[string]outcome{}
		for _, path := range names {
			name := strings.TrimSuffix(filepath.Base(path), ".tar.gz")
			root := filepath.Join(os.Args[3], name)
			dst := filepath.Join(root, "out")
			if err := os.MkdirAll(dst, 0o755); err != nil {
				panic(err)
			}
			data, err := os.ReadFile(path)
			if err != nil {
				panic(err)
			}
			err = extractTarGz(bytes.NewReader(data), dst)
			o := outcome{Failed: err != nil, Entries: walk(root)}
			if err != nil {
				o.Error = err.Error()
			}
			results[name] = o
		}
		out, err := json.MarshalIndent(results, "", "  ")
		if err != nil {
			panic(err)
		}
		os.Stdout.Write(append(out, '\n'))
	default:
		fmt.Fprintln(os.Stderr, "unknown mode", os.Args[1])
		os.Exit(2)
	}
}
