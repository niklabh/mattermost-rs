# Phase 0 spikes (docs/PLUGIN_PLAN.md §4)

Throwaway Go peers for the Rust spikes in `spikes/plugin-phase0/`. They answer three questions
that decide whether the plugin plan holds. They are not fixtures, and nothing in the workspace
tests depends on them. Run everything with `spikes/plugin-phase0/run.sh`.

- `hello/`: a real Mattermost plugin built on `plugin.ClientMain`. The Rust launcher does the
  go-plugin handshake with it and calls hooks.
- `gobgen/`: gob-encodes real plugin RPC payloads and writes a reflection-derived expectation for
  each. The Rust dynamic decoder must reproduce it.
- `yamuxpeer/`: a hashicorp/yamux v0.1.2 peer, as server or client, for the interop stress test.
