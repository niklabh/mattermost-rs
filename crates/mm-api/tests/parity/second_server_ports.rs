//! Every `SecondServer` in the parity binary gets a port nobody else in the binary uses.
//!
//! `SecondServer::start` frees its port before binding — deliberately, so a stale server cannot
//! answer for a fresh binary (see the comment inside `start`). The cost of that is that two call
//! sites naming one literal **kill each other**, and the one killed first reports its peer
//! unreachable, in a suite that did nothing wrong.
//!
//! Measured 2026-09-15. The uploads merge started its attachments-off servers on :8090 and :8091,
//! which are `LICENSED_RUST_PORT` and `LICENSED_GUEST_RUST_PORT`. The licensed pair is a
//! once-per-binary static, so whichever upload test ran after it killed it for the rest of the
//! run, and `user_convert` and `users_list` failed with ":8090 unreachable" on every full run
//! while passing alone. `custom_status_writes` and `channel_read_all` shared :8074 the same way.
//!
//! It also refuses the stack's own ports, which a second server would free just the same.
//!
//! This test needs no stack: it reads the test sources. Pick an unused port when it fails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The leading decimal literal of `rest`, after whitespace, when there is one.
fn leading_port(rest: &str) -> Option<u16> {
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[test]
fn no_two_second_servers_share_a_port() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files = Vec::new();
    rust_files(&tests, &mut files);
    assert!(
        files.len() > 10,
        "found the test sources under {}",
        tests.display()
    );

    let mut claims: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let name = file.strip_prefix(&tests).unwrap().display().to_string();
        // The start sites. `start(` followed by a literal; a call passing a named constant is
        // covered by the constant's own declaration below.
        for call in ["SecondServer::start(", "SecondServer::start_in("] {
            for (at, _) in text.match_indices(call) {
                if let Some(port) = leading_port(&text[at + call.len()..]) {
                    claims.entry(port).or_default().push(name.clone());
                }
            }
        }
        // The licensed pair's constants, whose starts pass the name rather than a literal.
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with("const ") && line.contains("_PORT: u16 =") {
                if let Some((_, value)) = line.split_once('=') {
                    if let Some(port) = leading_port(value) {
                        claims.entry(port).or_default().push(name.clone());
                    }
                }
            }
        }
    }

    assert!(
        claims.contains_key(&8090),
        "the licensed pair's port was not found — the scan is not reading what it thinks"
    );
    let shared: Vec<_> = claims.iter().filter(|(_, sites)| sites.len() > 1).collect();
    assert!(
        shared.is_empty(),
        "ports claimed more than once: {shared:?}"
    );

    // The stack's own servers, which a second server would free and so kill: Go and mm-api, the
    // oracles `scripts/go-*.sh` start at Go's port + 30 to + 38 (boards, discoverable, the licensed
    // three, edit limit, plugins, the managed-categories two), and the mock Marketplace
    // `parity::marketplace` serves for the plugins oracle at + 39, and the link-preview oracle
    // `scripts/go-links.sh` at + 50. Measured 2026-09-18: a plugin suite on :8095 took the
    // boards oracle down, and twenty tests in six other suites failed for it.
    // `parity::plugin_startup` starts its own Go server at + 73, and `parity::plugin_hooks` at
    // + 74.
    // `parity::plugin_hooks`' support-packet tranche starts its Go server at + 87, and its
    // plugin API tranches at + 88, + 89, + 90, + 92, + 93, + 94, + 95 and + 97, its slash-command tranche
    // at + 91, and its client plugin-HTTP tranche at + 98. Its auth tranche's is at + 61: an
    // offset of 100 or more is the next stack's Go server (stack k's Go is 8065 + 100k), so a
    // tranche's Go offset stays below 100. `parity::ratelimit` starts its Go servers at + 75, + 48 and + 49 (+ 82 and + 83 are
    // `parity::plugin_hooks`'s download and upload tranches). `parity::plugin_driver` starts its
    // Go server at + 59, and `parity::plugin_hooks`' notification tranche at + 62.
    // The `EnableTesting` oracle (`MMRS_EDITLIMIT_VARIANT=testing`) is at + 70: until 2026-09-25
    // `plugin_hooks`' onboarding host started on it and killed the oracle, which is why
    // `parity::manualtest` so often found no oracle to ask.
    let reserved: Vec<u16> = [
        8065, 8066, 8113, 8114, 8115, 8124, 8126, 8127, 8135, 8138, 8139, 8140, 8152, 8153, 8154,
        8155, 8156, 8157, 8159, 8162, 8163,
    ]
    .into_iter()
    .chain(8095..=8104)
    .collect();
    let taken: Vec<_> = claims
        .iter()
        .filter(|(port, _)| reserved.contains(port))
        .collect();
    assert!(
        taken.is_empty(),
        "second servers on a port the stack itself uses: {taken:?}"
    );

    // Stack k's servers start at 8065 + 100k, so a port at or past 8165, or a Go offset of 100
    // or more, is the next stack's Go server or mm-api — which the second server then frees, and
    // so kills. Measured 2026-09-25: `parity::plugin_driver` on stack 2 at :8165 and + 101 took
    // stack 3's Go server and mm-api down on every run.
    let beyond: Vec<_> = claims.keys().filter(|port| **port >= 8165).collect();
    assert!(
        beyond.is_empty(),
        "second servers on the next stack's ports: {beyond:?}"
    );
    let mut offsets = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with("const ") && line.contains("_OFFSET: u16 =") {
                if let Some(offset) = line.split_once('=').and_then(|(_, v)| leading_port(v)) {
                    offsets.push((offset, file.display().to_string()));
                }
            }
        }
    }
    assert!(
        offsets.iter().any(|(o, _)| *o == 74),
        "plugin_hooks' Go offset was not found — the scan is not reading what it thinks"
    );
    let far: Vec<_> = offsets.iter().filter(|(o, _)| *o >= 100).collect();
    assert!(far.is_empty(), "Go offsets into the next stack: {far:?}");
}
