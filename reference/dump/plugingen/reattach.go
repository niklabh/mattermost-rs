package main

// `plugingen reattach <root>`: Go's plugin.Environment.Reattach over the bundles
// crates/mm-plugin's tests/environment.rs builds under <root>/plugins, driven through the script
// below. The Rust half is `reattach_script` in that file; the two transcripts must be equal, and
// so must what the plugin processes logged.
//
// The plugins reattached to are launched here, by a go-plugin client of our own, exactly as a
// developer's `pluginctl` would launch one elsewhere: the environment only ever sees the
// ReattachConfig.

import (
	"encoding/json"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"

	goplugin "github.com/hashicorp/go-plugin"
	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
	"github.com/mattermost/mattermost/server/public/plugin/plugintest"
	"github.com/mattermost/mattermost/server/public/shared/mlog"
)

// ReattachManifests are the manifests the script reattaches under, by name.
var ReattachManifests = map[string]string{
	// No server component: the one error Reattach returns.
	"webonly": `{"id":"webonly","version":"1.0.0","webapp":{"bundle_path":"main.js"}}`,
	// A failed version check is swallowed, and the plugin is marked running.
	"minversion": `{"id":"reminv","version":"1.0.0","min_server_version":"99.0.0","server":{"executable":"x"}}`,
	// A process that is gone: the start fails, and that is swallowed too.
	"dead": `{"id":"dead","version":"1.0.0","server":{"executable":"x"},"webapp":{"bundle_path":"main.js"}}`,
	// The two launched here.
	"ok":     `{"id":"ok","name":"OK","version":"1.0.0","server":{"executable":"server/env_plugin"}}`,
	"refuse": `{"id":"refuse","name":"Refuses","version":"1.0.0","server":{"executable":"server/env_plugin_refuse"}}`,
}

// launch starts an executable as a Mattermost plugin and returns its client, which the caller
// kills at the end.
func launch(path string) (*goplugin.Client, *model.PluginReattachConfig, error) {
	client := goplugin.NewClient(&goplugin.ClientConfig{
		HandshakeConfig: goplugin.HandshakeConfig{
			ProtocolVersion:  1,
			MagicCookieKey:   "MATTERMOST_PLUGIN",
			MagicCookieValue: "Securely message teams, anywhere.",
		},
		Plugins: map[string]goplugin.Plugin{},
		Cmd:     exec.Command(path),
	})
	if _, err := client.Client(); err != nil {
		return nil, nil, err
	}
	return client, model.NewPluginReattachConfig(client.ReattachConfig()), nil
}

func runReattach(root string) error {
	logger, err := mlog.NewLogger()
	if err != nil {
		return err
	}
	pluginDir := filepath.Join(root, "plugins")
	env, err := plugin.NewEnvironment(
		func(*model.Manifest) plugin.API { return &plugintest.API{} },
		&appDriver{Driver: &plugintest.Driver{}}, pluginDir, filepath.Join(root, "webapp"), logger, noMetrics{},
	)
	if err != nil {
		return err
	}
	manifest := func(name string) *model.Manifest {
		var m model.Manifest
		if err := json.Unmarshal([]byte(ReattachManifests[name]), &m); err != nil {
			panic(err)
		}
		return &m
	}

	// A pid that no longer exists, and a socket nobody listens on.
	gone := exec.Command("true")
	if err := gone.Run(); err != nil {
		return err
	}
	dead := &model.PluginReattachConfig{
		Protocol: "netrpc", ProtocolVersion: 1, Pid: gone.Process.Pid,
		Addr: net.UnixAddr{Name: filepath.Join(root, "nobody.sock"), Net: "unix"},
	}

	okClient, okConfig, err := launch(filepath.Join(pluginDir, "ok", "server", "env_plugin"))
	if err != nil {
		return err
	}
	defer okClient.Kill()
	refuseClient, refuseConfig, err := launch(filepath.Join(pluginDir, "refuse", "server", "env_plugin_refuse"))
	if err != nil {
		return err
	}
	defer refuseClient.Kill()

	var steps []map[string]any
	step := func(entry map[string]any) { steps = append(steps, entry) }
	ids := []string{"webonly", "reminv", "dead", "ok", "refuse"}
	observe := func(label string) {
		state := map[string]int{}
		hooks := map[string]string{}
		for _, id := range ids {
			state[id] = env.GetPluginState(id)
			hooks[id] = hooksErr(env, id)
		}
		active := []string{}
		for _, info := range env.Active() {
			active = append(active, info.Manifest.Id+" "+info.Path)
		}
		sort.Strings(active)
		st, err := env.Statuses()
		errs := map[string]string{}
		for _, s := range st {
			if s.PluginId == "ok" || s.PluginId == "refuse" {
				errs[s.PluginId] = s.Error
			}
		}
		step(map[string]any{"step": "observe", "label": label, "state": state, "hooks": hooks,
			"active": active, "status_errors": errs, "statuses_error": errString(err)})
	}

	for _, r := range []struct {
		name   string
		config *model.PluginReattachConfig
	}{
		{"webonly", dead}, {"minversion", dead}, {"dead", dead},
		{"ok", okConfig}, {"ok", okConfig}, {"refuse", refuseConfig},
	} {
		step(map[string]any{"step": "reattach", "name": r.name, "error": errString(env.Reattach(manifest(r.name), r.config))})
	}
	observe("after reattaching")

	deactivated := map[string]bool{}
	for _, id := range ids {
		deactivated[id] = env.Deactivate(id)
	}
	step(map[string]any{"step": "deactivate", "deactivated": deactivated})
	observe("after deactivating")

	for _, id := range ids {
		env.RemovePlugin(id)
	}
	observe("after removing")
	env.Shutdown()

	out, err := json.MarshalIndent(steps, "", "  ")
	if err != nil {
		return err
	}
	text := strings.ReplaceAll(string(out), root, "$ROOT")
	_, err = os.Stdout.WriteString(text + "\n")
	return err
}
