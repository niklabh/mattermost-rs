package main

// The bundles `parity::plugin_startup` starts a Go server and a Rust plugin host from, written to
// fixtures/behaviour_plugin_startup.json. The suite lays them out identically for both servers —
// some in the file store's `plugins/`, some in a `prepackaged_plugins` directory beside the
// server — starts each, and compares what start-up made of them (channels/app/plugin.go
// `initPlugins`: the signature-checked file-store sync, `processPrepackagedPlugins` and
// `persistTransitionallyPrepackagedPlugins`).
//
// Every bundle is webapp-only, so nothing is launched and the two hosts cannot step on each other.
// Signatures are made with the test key of behaviour_plugin_signature.go (whose public half the
// suite plants as a configuration file), or with its stranger key where a signature must not
// verify. Deterministic for the same reasons as that file: fixed key, time and tar headers.

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"time"
)

type startupBundle struct {
	// Where the suite puts it: "store" (the file store's plugins/), "prepackaged", or a path
	// under prepackaged.
	Place string `json:"place"`
	// The file name, `<id>.tar.gz`; its signature, when there is one, is `<file>.sig`.
	File      string `json:"file"`
	Bundle    string `json:"bundle"`
	Signature string `json:"signature,omitempty"`
}

// startupTar is a bundle with `plugin/plugin.json`, `plugin/dist/main.js` and any extra files.
func startupTar(manifest string, extra map[string]string) []byte {
	files := map[string]string{"plugin.json": manifest, "dist/main.js": "// " + manifest[:24] + "\n"}
	for k, v := range extra {
		files[k] = v
	}
	names := make([]string, 0, len(files))
	for name := range files {
		names = append(names, name)
	}
	sort.Strings(names)

	var raw bytes.Buffer
	gz := gzip.NewWriter(&raw)
	tw := tar.NewWriter(gz)
	mtime := time.Unix(1700000000, 0).UTC()
	dirs := map[string]bool{}
	add := func(name string, typ byte, mode int64, body string) {
		hdr := &tar.Header{Name: name, Typeflag: typ, Mode: mode, Size: int64(len(body)), ModTime: mtime, Format: tar.FormatUSTAR}
		if err := tw.WriteHeader(hdr); err != nil {
			panic(err)
		}
		if _, err := tw.Write([]byte(body)); err != nil {
			panic(err)
		}
	}
	add("plugin/", tar.TypeDir, 0o755, "")
	for _, name := range names {
		if dir := filepath.Dir(name); dir != "." && !dirs[dir] {
			dirs[dir] = true
			add("plugin/"+dir+"/", tar.TypeDir, 0o755, "")
		}
		add("plugin/"+name, tar.TypeReg, 0o644, files[name])
	}
	if err := tw.Close(); err != nil {
		panic(err)
	}
	if err := gz.Close(); err != nil {
		panic(err)
	}
	return raw.Bytes()
}

func webappManifest(id, name, version, extra string) string {
	return `{"id":"` + id + `","name":"` + name + `","version":"` + version + `"` + extra + `,"webapp":{"bundle_path":"dist/main.js"}}`
}

const startupIcon = `<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8"/></svg>`

func writePluginStartupBehaviourFixture(outDir string) error {
	ours := sigEntity(pluginSigTestKey)
	stranger := sigEntity(pluginSigStrangerKey)
	b64 := base64.StdEncoding.EncodeToString

	type spec struct {
		place, id, name, version, extra string
		files                           map[string]string
		// "ours", "ours-armored", "stranger" or "" for none.
		sign string
	}
	specs := []spec{
		// The file store. Signed with our key: installed. A stranger's, or none: skipped when
		// RequirePluginSignature is on.
		{"store", "mmrs.startup.signed", "MMRS Startup Signed", "1.0.0", "", nil, "ours"},
		{"store", "mmrs.startup.armored", "MMRS Startup Armored", "1.0.1", "", nil, "ours-armored"},
		{"store", "mmrs.startup.stranger", "MMRS Startup Stranger", "1.0.2", "", nil, "stranger"},
		{"store", "mmrs.startup.unsigned", "MMRS Startup Unsigned", "1.0.3", "", nil, ""},
		// Enabled by SetDefaults; the prepackaged copy is newer, so it replaces this one.
		{"store", "com.mattermost.calls", "MMRS Calls Old", "1.0.0", "", nil, "ours"},
		// Enabled by SetDefaults; the prepackaged copy is older, so its install is skipped.
		{"store", "mattermost-ai", "MMRS AI New", "2.0.0", "", nil, "ours"},
		// Transitional and enabled by the suite; the prepackaged copy is older: not persisted.
		{"store", "jenkins", "MMRS Jenkins New", "2.0.0", "", nil, "ours"},

		// The prepackaged directory.
		{"prepackaged", "playbooks", "MMRS Playbooks Prepackaged", "9.9.9", "", nil, "ours"},
		{"prepackaged", "com.mattermost.calls", "MMRS Calls New", "2.0.0", "", nil, "ours"},
		{"prepackaged", "mattermost-ai", "MMRS AI Old", "1.0.0", "", nil, "ours"},
		{"prepackaged", "mmrs.startup.offered", "MMRS Startup Offered", "0.5.0",
			`,"description":"offered, not installed","homepage_url":"https://example.invalid/home","release_notes_url":"https://example.invalid/notes","icon_path":"assets/icon.svg"`,
			map[string]string{"assets/icon.svg": startupIcon}, "ours"},
		{"prepackaged", "mmrs.startup.badicon", "MMRS Startup Bad Icon", "0.6.0", `,"icon_path":"assets/icon.png"`,
			map[string]string{"assets/icon.png": "not an svg"}, "ours"},
		{"prepackaged", "mmrs.startup.preunsigned", "MMRS Startup Prepackaged Unsigned", "0.7.0", "", nil, ""},
		{"prepackaged", "mmrs.startup.prestranger", "MMRS Startup Prepackaged Stranger", "0.8.0", "", nil, "stranger"},
		{"prepackaged/nested", "mmrs.startup.nested", "MMRS Startup Nested", "0.9.0", "", nil, "ours"},
		// Transitionally prepackaged: enabled and absent from the store (persisted), enabled and
		// older than the store's (not persisted), and not enabled (dropped).
		{"prepackaged", "jitsi", "MMRS Jitsi", "1.0.0", "", nil, "ours"},
		{"prepackaged", "jenkins", "MMRS Jenkins Old", "1.0.0", "", nil, "ours"},
		{"prepackaged", "mattermost-autolink", "MMRS Autolink", "1.0.0", "", nil, "ours"},
	}

	bundles := make([]startupBundle, 0, len(specs))
	for _, s := range specs {
		bundle := startupTar(webappManifest(s.id, s.name, s.version, s.extra), s.files)
		entry := startupBundle{Place: s.place, File: s.id + ".tar.gz", Bundle: b64(bundle)}
		switch s.sign {
		case "ours":
			entry.Signature = b64(sigSign(ours, bundle, false, false))
		case "ours-armored":
			entry.Signature = b64(sigSign(ours, bundle, true, false))
		case "stranger":
			entry.Signature = b64(sigSign(stranger, bundle, false, false))
		}
		bundles = append(bundles, entry)
	}

	out, err := json.MarshalIndent(map[string]any{"bundles": bundles}, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_plugin_startup.json"), append(out, '\n'), 0o644)
}
