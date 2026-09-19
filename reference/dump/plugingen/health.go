package main

// `plugingen health <root>`: Go's plugin health check (public/plugin/health_check.go and its two
// entry points in environment.go) over <root>/plugins, driven through the script below. It is the
// Go half of crates/mm-plugin's tests/environment.rs `health_check_matches_go_step_for_step`,
// which builds the same two bundles, runs the same script over mm_plugin's Environment, and
// compares the transcripts.
//
// `ok` stays up. `crashy` is examples/env_plugin installed under a name containing `crash`, which
// exits when `$ENV_PLUGIN_LOG.crash` appears (deleting it first). The job's own ticker runs every
// thirty seconds, so the script calls `CheckPlugin` itself, which is what each tick does for every
// running plugin.

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
	"github.com/mattermost/mattermost/server/public/plugin"
	"github.com/mattermost/mattermost/server/public/plugin/plugintest"
	"github.com/mattermost/mattermost/server/public/shared/mlog"
)

// HealthIDs are the plugins the script observes; `absent` has no bundle.
var HealthIDs = []string{"ok", "crashy", "absent"}

func runHealth(root string) error {
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
	request := os.Getenv("ENV_PLUGIN_LOG") + ".crash"

	var steps []map[string]any
	step := func(entry map[string]any) { steps = append(steps, entry) }
	health := func() map[string]string {
		out := map[string]string{}
		for _, id := range HealthIDs {
			out[id] = errString(env.PerformHealthCheck(id))
		}
		return out
	}
	observe := func(label string) {
		statuses := map[string]any{}
		st, stErr := env.Statuses()
		for _, s := range st {
			statuses[s.PluginId] = map[string]any{"state": s.State, "error": s.Error, "version": s.Version}
		}
		active := []string{}
		for _, info := range env.Active() {
			active = append(active, info.Manifest.Id)
		}
		sort.Strings(active)
		hooks := map[string]string{}
		for _, id := range HealthIDs {
			hooks[id] = hooksErr(env, id)
		}
		step(map[string]any{
			"step": "observe", "label": label, "statuses": statuses, "statuses_error": errString(stErr),
			"active": active, "hooks": hooks, "job": env.GetPluginHealthCheckJob() != nil,
		})
	}
	// crash asks `crashy` to exit and waits until its supervisor stops answering.
	crash := func(label string) error {
		if err := os.WriteFile(request, nil, 0o644); err != nil {
			return err
		}
		deadline := time.Now().Add(10 * time.Second)
		for {
			if _, err := os.Stat(request); errors.Is(err, os.ErrNotExist) {
				break
			}
			if time.Now().After(deadline) {
				return errors.New("crashy never took the crash request")
			}
			time.Sleep(10 * time.Millisecond)
		}
		for env.PerformHealthCheck("crashy") == nil {
			if time.Now().After(deadline) {
				return errors.New("crashy still answers after crashing")
			}
			time.Sleep(10 * time.Millisecond)
		}
		step(map[string]any{"step": "crash", "label": label, "health": health()})
		return nil
	}

	before := env.GetPluginHealthCheckJob() != nil
	env.TogglePluginHealthCheckJob(true)
	job := env.GetPluginHealthCheckJob()
	env.TogglePluginHealthCheckJob(true)
	step(map[string]any{"step": "toggle on", "before": before, "on": job != nil, "same": env.GetPluginHealthCheckJob() == job})

	for _, id := range HealthIDs {
		_, activated, err := env.Activate(id)
		step(map[string]any{"step": "activate", "id": id, "activated": activated, "error": errString(err)})
	}
	observe("activated")

	for _, id := range HealthIDs {
		job.CheckPlugin(id)
	}
	step(map[string]any{"step": "healthy", "health": health()})
	observe("after checking healthy plugins")

	for _, label := range []string{"first", "second", "third"} {
		if err := crash(label); err != nil {
			return err
		}
		job.CheckPlugin("crashy")
		job.CheckPlugin("ok")
		observe("after the " + label + " failure")
	}

	// Deactivate leaves the dead supervisor registered, so a check reaches it: a new failure.
	job.CheckPlugin("crashy")
	observe("a check after deactivation")

	// A restart that fails: the executable is gone when the job restarts it.
	if err := crash("before a failed restart"); err != nil {
		return err
	}
	exe := filepath.Join(pluginDir, "crashy", "server", "env_plugin_crashy")
	if err := os.Rename(exe, exe+".away"); err != nil {
		return err
	}
	job.CheckPlugin("crashy")
	observe("after a failed restart")
	// The failed activation replaced the registration, supervisor and all: nothing to ping.
	job.CheckPlugin("crashy")
	step(map[string]any{"step": "no supervisor", "health": health()})
	observe("a check with no supervisor")
	if err := os.Rename(exe+".away", exe); err != nil {
		return err
	}

	env.TogglePluginHealthCheckJob(false)
	step(map[string]any{"step": "toggle off", "job": env.GetPluginHealthCheckJob() != nil})
	env.TogglePluginHealthCheckJob(true)
	step(map[string]any{"step": "toggle on again", "job": env.GetPluginHealthCheckJob() != nil, "new": env.GetPluginHealthCheckJob() != job})

	env.Shutdown()
	observe("after shutdown")

	out, err := json.MarshalIndent(steps, "", "  ")
	if err != nil {
		return err
	}
	text := strings.ReplaceAll(string(out), root, "$ROOT")
	_, err = os.Stdout.WriteString(text + "\n")
	return err
}
