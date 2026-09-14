package main

// Behavioural oracle for Go's `mime.TypeByExtension`, written to fixtures/behaviour_mime.json.
//
// `model.NewInfo` puts this function's answer on the wire as `FileInfo.mime_type`, and the answer
// is host-dependent: after the builtin table Go loads the first of `/usr/local/share/mime/globs2`
// and `/usr/share/mime/globs2` that opens, or — only when neither does — the `mime.types` files.
// So the fixture records **the input as well as the output**: the lines of the file Go read that
// name a corpus extension, in file order, so the Rust test rebuilds the same table from the same
// lines on any host rather than reading whichever files it happens to have. Filtering by
// extension is safe because every rule in the loader is per extension.
//
// Determinism: the corpus is fixed; the recorded lines are whatever this host's database holds,
// which is the point — regenerating on a different host is expected to move them.

import (
	"encoding/json"
	"mime"
	"os"
	"path/filepath"
	"strings"
)

type mimeExtCase struct {
	Ext  string `json:"ext"`
	Type string `json:"type"`
}

func writeMimeBehaviourFixture(outDir string) error {
	exts := []string{
		// Builtins, and their upper-case spellings for the lowercase fallback.
		".txt", ".TXT", ".Txt", ".text", ".png", ".PNG", ".jpg", ".jpeg", ".JPG", ".jPg", ".jfif",
		".gif", ".svg", ".webp", ".bmp", ".tif", ".tiff", ".ico", ".avif", ".apng", ".xbm",
		".pdf", ".csv", ".html", ".htm", ".json", ".xml", ".xsl", ".xbl", ".js", ".mjs", ".css",
		".mp4", ".mp3", ".wav", ".ogg", ".oga", ".ogv", ".opus", ".m4a", ".webm", ".flac",
		".zip", ".gz", ".docx", ".xlsx", ".pptx", ".doc", ".xls", ".ppt", ".rtf", ".xhtml", ".xht",
		".apk", ".exe", ".bin", ".com", ".wasm", ".ics", ".vtt", ".eml", ".eps", ".ai", ".ps", ".rdf",
		// Host-database entries, if the host has them.
		".md", ".Md", ".mD", ".markdown", ".rs", ".go", ".py", ".c", ".C", ".h", ".cpp", ".java",
		".sh", ".yaml", ".yml", ".toml", ".log", ".conf", ".ini", ".tar", ".tgz", ".bz2", ".xz",
		".zst", ".7z", ".rar", ".jar", ".deb", ".rpm", ".iso", ".mkv", ".mov", ".avi", ".m4v",
		".aac", ".heic", ".heif", ".psd", ".xcf", ".odt", ".ods", ".odp", ".epub", ".mobi",
		".patch", ".diff", ".sql", ".ts", ".tsx", ".jsx", ".vue", ".woff", ".woff2", ".ttf", ".otf",
		".swf", ".dll", ".so", ".class", ".pem", ".crt", ".key", ".gpg", ".asc", ".torrent",
		".vcf", ".msg", ".txt2", ".Rs",
		// Degenerate inputs `filepath.Ext` can produce.
		"", ".", "txt", ".zzzzzz", ". ", ".a b",
	}

	cases := make([]mimeExtCase, 0, len(exts))
	wanted := map[string]bool{}
	for _, ext := range exts {
		cases = append(cases, mimeExtCase{Ext: ext, Type: mime.TypeByExtension(ext)})
		wanted[strings.ToLower(ext)] = true
	}

	globsLoaded := false
	var globsLines []string
	for _, name := range []string{"/usr/local/share/mime/globs2", "/usr/share/mime/globs2"} {
		data, err := os.ReadFile(name)
		if err != nil {
			continue
		}
		globsLoaded = true
		for _, line := range strings.Split(string(data), "\n") {
			fields := strings.Split(line, ":")
			if len(fields) < 3 || len(fields[2]) < 3 || fields[2][0] != '*' || fields[2][1] != '.' {
				continue
			}
			if wanted[strings.ToLower(fields[2][1:])] {
				globsLines = append(globsLines, line)
			}
		}
		break
	}

	var typesLines []string
	if !globsLoaded {
		for _, name := range []string{"/etc/mime.types", "/etc/apache2/mime.types", "/etc/apache/mime.types", "/etc/httpd/conf/mime.types"} {
			data, err := os.ReadFile(name)
			if err != nil {
				continue
			}
			for _, line := range strings.Split(string(data), "\n") {
				fields := strings.Fields(line)
				if len(fields) <= 1 {
					continue
				}
				for _, ext := range fields[1:] {
					if wanted["."+strings.ToLower(ext)] {
						typesLines = append(typesLines, line)
						break
					}
				}
			}
		}
	}

	if globsLines == nil {
		globsLines = []string{}
	}
	if typesLines == nil {
		typesLines = []string{}
	}
	out := map[string]any{
		"globs2_loaded":     globsLoaded,
		"globs2_lines":      globsLines,
		"mime_types_lines":  typesLines,
		"type_by_extension": cases,
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_mime.json"), append(data, '\n'), 0o644)
}
