package main

// `plugingen env <root>`: Go's plugin.Environment over <root>/plugins, unpacking webapps into
// <root>/webapp, driven through the script below. It is the Go half of crates/mm-plugin's
// tests/environment.rs, which builds the bundles, runs the same script over mm_plugin's
// Environment, and compares the two transcripts.
//
// The transcript is a JSON array on stdout, one object per step, with <root> written as $ROOT so
// the two runs can use different directories.

import (
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
	"github.com/mattermost/mattermost/server/public/plugin/plugintest"
	"github.com/mattermost/mattermost/server/public/shared/mlog"
)

// EnvActivations is the order the script activates plugins in: every bundle the test builds,
// the running one twice, and ids no bundle has.
var EnvActivations = []string{
	"ok", "ok", "refuse", "webapp", "yamlplugin", "nocomponent", "minversion", "badminversion",
	"missingexe", "escape", "noarch", "dotpath", "badjson", "dup", "hidden", "absent",
}

// EnvOkManifestV2 replaces the running plugin's manifest before it is restarted.
const EnvOkManifestV2 = `{"id":"ok","name":"OK","description":"runs","version":"1.0.1","server":{"executable":"server/env_plugin"}}`

func errString(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

// hooksErr is HooksForPlugin's error: after Deactivate the supervisor is still registered, and
// only the state keeps the hooks out of reach.
func hooksErr(env *plugin.Environment, id string) string {
	_, err := env.HooksForPlugin(id)
	return errString(err)
}

func runEnv(root string) error {
	logger, err := mlog.NewLogger()
	if err != nil {
		return err
	}
	pluginDir := filepath.Join(root, "plugins")
	webappDir := filepath.Join(root, "webapp")
	env, err := plugin.NewEnvironment(
		func(*model.Manifest) plugin.API { return &plugintest.API{} },
		&appDriver{Driver: &plugintest.Driver{}}, pluginDir, webappDir, logger, noMetrics{},
	)
	if err != nil {
		return err
	}

	var steps []map[string]any
	step := func(entry map[string]any) { steps = append(steps, entry) }
	statuses := func(label string) {
		st, err := env.Statuses()
		step(map[string]any{"step": "statuses", "label": label, "statuses": st, "error": errString(err)})
	}

	available, err := env.Available()
	var ids []string
	for _, info := range available {
		ids = append(ids, info.Manifest.Id+" "+info.Path)
	}
	step(map[string]any{"step": "available", "ids": ids, "error": errString(err)})

	for _, id := range EnvActivations {
		manifest, activated, err := env.Activate(id)
		entry := map[string]any{"step": "activate", "id": id, "activated": activated, "error": errString(err)}
		if manifest != nil {
			entry["manifest"] = manifest.Id
		}
		step(entry)
	}
	statuses("after activation")

	var files []string
	_ = filepath.WalkDir(webappDir, func(path string, d fs.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			rel, _ := filepath.Rel(webappDir, path)
			files = append(files, rel)
		}
		return nil
	})
	sort.Strings(files)
	step(map[string]any{"step": "webapp", "files": files})

	hooks := map[string]any{}
	for _, id := range []string{"ok", "webapp", "refuse", "absent"} {
		_, err := env.HooksForPlugin(id)
		hooks[id] = errString(err)
	}
	active := []string{}
	for _, info := range env.Active() {
		active = append(active, info.Manifest.Id)
	}
	sort.Strings(active)
	public, publicErr := env.PublicFilesPath("ok")
	_, absentPublic := env.PublicFilesPath("refuse")
	manifest, manifestErr := env.GetManifest("webapp")
	_, absentManifest := env.GetManifest("absent")
	step(map[string]any{
		"step": "lookups", "hooks": hooks, "active": active,
		"public": public, "public_error": errString(publicErr), "public_refused": errString(absentPublic),
		"manifest": manifest.Name, "manifest_error": errString(manifestErr), "manifest_absent": errString(absentManifest),
		"is_active": map[string]bool{"ok": env.IsActive("ok"), "refuse": env.IsActive("refuse")},
		"state":     map[string]int{"ok": env.GetPluginState("ok"), "refuse": env.GetPluginState("refuse"), "absent": env.GetPluginState("absent")},
	})

	// A new version on disk: activating again must register it, not keep the old bundle.
	if err := os.WriteFile(filepath.Join(pluginDir, "ok", "plugin.json"), []byte(EnvOkManifestV2), 0o644); err != nil {
		return err
	}
	step(map[string]any{
		"step":       "deactivate",
		"ok":         env.Deactivate("ok"),
		"ok_again":   env.Deactivate("ok"),
		"webapp":     env.Deactivate("webapp"),
		"refuse":     env.Deactivate("refuse"),
		"absent":     env.Deactivate("absent"),
		"is_active":  env.IsActive("ok"),
		"hooks_ok":   hooksErr(env, "ok"),
		"restart_ok": errString(env.RestartPlugin("ok")),
	})
	statuses("after deactivation and restart")
	versions := []string{}
	for _, info := range env.Active() {
		versions = append(versions, info.Manifest.Id+" "+info.Manifest.Version)
	}
	sort.Strings(versions)
	step(map[string]any{"step": "active after restart", "active": versions})

	env.RemovePlugin("refuse")
	statuses("after removing refuse")

	env.Shutdown()
	statuses("after shutdown")

	out, err := json.MarshalIndent(steps, "", "  ")
	if err != nil {
		return err
	}
	text := strings.ReplaceAll(string(out), root, "$ROOT")
	_, err = os.Stdout.WriteString(text + "\n")
	return err
}
