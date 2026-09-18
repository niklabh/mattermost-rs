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
        for (at, _) in text.match_indices("SecondServer::start(") {
            if let Some(port) = leading_port(&text[at + "SecondServer::start(".len()..]) {
                claims.entry(port).or_default().push(name.clone());
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

    // The stack's own servers, which a second server would free and so kill: Go and mm-api, and
    // the oracles `scripts/go-*.sh` start at Go's port + 30 to + 35 (boards, discoverable, the
    // licensed three, edit limit). Measured 2026-09-18: a plugin suite on :8095 took the boards
    // oracle down, and twenty tests in six other suites failed for it.
    let reserved: Vec<u16> = [8065, 8066].into_iter().chain(8095..=8100).collect();
    let taken: Vec<_> = claims
        .iter()
        .filter(|(port, _)| reserved.contains(port))
        .collect();
    assert!(
        taken.is_empty(),
        "second servers on a port the stack itself uses: {taken:?}"
    );
}
