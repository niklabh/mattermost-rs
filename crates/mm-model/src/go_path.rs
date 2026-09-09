//! Go's `path` package — the two functions the ported tree needs.
//!
//! **Not `std::path`**, which is filesystem-flavoured and platform-dependent: on Windows it would
//! join with a backslash, and it does not collapse `a/b/../c` lexically the way Go's `path.Clean`
//! does. A URL path is neither of those things, so the algorithm is reproduced rather than
//! delegated.
//!
//! Lived inside `command_autocomplete.rs` until a second caller appeared —
//! `GetSubpathFromConfig`, which cleans the path out of `SiteURL`. It sits beside [`crate::go_url`]
//! for the same reason that module exists: a Go stdlib port with its own oracle, used by whatever
//! needs it.

/// Port of `path.Join` — concatenate the non-empty elements with `/`, then `Clean`.
pub fn join(elems: &[&str]) -> String {
    let joined = elems
        .iter()
        .filter(|e| !e.is_empty())
        .copied()
        .collect::<Vec<&str>>()
        .join("/");
    if joined.is_empty() {
        return String::new();
    }
    clean(&joined)
}

/// Port of `path.Clean` — the lexical shortest equivalent path.
///
/// Byte-oriented, like Go's: a `/` inside a multi-byte character is impossible in UTF-8, so
/// working on bytes is safe and matches Go's indexing exactly.
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }

    let bytes = path.as_bytes();
    let rooted = bytes[0] == b'/';
    let n = bytes.len();

    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut r = 0usize;
    let mut dotdot = 0usize;

    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }

    while r < n {
        if bytes[r] == b'/' {
            // Empty path element.
            r += 1;
        } else if bytes[r] == b'.' && (r + 1 == n || bytes[r + 1] == b'/') {
            // `.` element.
            r += 1;
        } else if bytes[r] == b'.'
            && r + 1 < n
            && bytes[r + 1] == b'.'
            && (r + 2 == n || bytes[r + 2] == b'/')
        {
            // `..` element: remove the previous one.
            r += 2;
            if out.len() > dotdot {
                // Go decrements the write cursor and then tests `out.index(out.w)` — the byte
                // **just past** the new end, i.e. the byte it has only now excluded. So the
                // separator that ends the loop is itself dropped. Testing `out.last()` here
                // instead leaves it in place, which turns `a/b/..` into `a/` rather than `a`;
                // the oracle caught exactly that.
                let mut popped = out.pop();
                while out.len() > dotdot && popped != Some(b'/') {
                    popped = out.pop();
                }
            } else if !rooted {
                // Cannot backtrack, and the path is relative: keep the `..`.
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.push(b'.');
                out.push(b'.');
                dotdot = out.len();
            }
        } else {
            // A real path element. Add a separator if needed, then copy it.
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && bytes[r] != b'/' {
                out.push(bytes[r]);
                r += 1;
            }
        }
    }

    if out.is_empty() {
        return ".".to_string();
    }

    // Every byte came from `path`, which is `&str`, and the only bytes inserted are ASCII.
    String::from_utf8(out).unwrap_or_default()
}

/// Port of `path.Base` — the last element of `path`.
///
/// `filepath.Base` is the same function on Unix (`os.IsPathSeparator` is `== '/'` and
/// `VolumeName` is empty), so this serves both. The two answers that are not the obvious ones are
/// pinned by the oracle: an empty path is `"."`, and a path of nothing but separators is `"/"`.
pub fn base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let bytes = path.as_bytes();
    // Strip trailing separators.
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &bytes[..end];
    // Find the last element.
    let start = match trimmed.iter().rposition(|b| *b == b'/') {
        Some(i) => i + 1,
        None => 0,
    };
    if start == trimmed.len() {
        // Nothing but separators.
        return "/".to_string();
    }
    // Every byte came from `path`, and the slice boundaries are at ASCII `/` or the ends.
    String::from_utf8_lossy(&trimmed[start..]).into_owned()
}

/// Port of `path.Dir` — everything but the last element, cleaned.
///
/// Also `filepath.Dir` on Unix, for the reason given on [`base`]. Note that `Dir("a")` is `"."`
/// and not the empty string: `writeFileLocally`'s `os.MkdirAll(filepath.Dir(path))` therefore
/// creates nothing for a bare filename rather than failing.
pub fn dir(path: &str) -> String {
    let cut = match path.as_bytes().iter().rposition(|b| *b == b'/') {
        Some(i) => i + 1,
        None => 0,
    };
    clean(&path[..cut])
}

#[cfg(test)]
mod go_parity {
    use super::{base, clean, dir, join};

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
            .expect("behaviour_go_stdlib.json is generated by reference/dump")
    }

    /// `path.Clean` is reimplemented here because `std::path` is filesystem-flavoured and
    /// platform-dependent; this asserts the reimplementation against Go's own answers.
    #[test]
    fn path_clean_matches_go() {
        let oracle = oracle();
        let cases = oracle["path_clean"].as_array().unwrap();
        assert!(cases.len() >= 25, "corpus should cover Clean's branches");
        for case in cases {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                clean(input),
                case["out"].as_str().unwrap(),
                "path.Clean({input:?})"
            );
        }
    }

    /// `path.Join` drops empty elements **before** cleaning, so `Join("", "x")` is `x`, not `/x`.
    #[test]
    fn path_join_matches_go() {
        let oracle = oracle();
        let cases = oracle["path_join"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let a = case["a"].as_str().unwrap();
            let b = case["b"].as_str().unwrap();
            assert_eq!(
                join(&[a, b]),
                case["out"].as_str().unwrap(),
                "path.Join({a:?}, {b:?})"
            );
        }
    }

    fn filestore_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_filestore.json"))
            .expect("behaviour_filestore.json is generated by reference/dump")
    }

    /// `filepath.Base` reduces a listing entry to the name `listExports` and `listImports`
    /// return. Its two degenerate answers — `""` is `"."` and `"///"` is `"/"` — are the ones a
    /// hand-written version gets wrong.
    #[test]
    fn filepath_base_matches_go() {
        let oracle = filestore_oracle();
        let cases = oracle["filepath_base"].as_array().unwrap();
        assert!(cases.len() >= 20, "corpus should cover Base's branches");
        for case in cases {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                base(input),
                case["out"].as_str().unwrap(),
                "filepath.Base({input:?})"
            );
        }
    }

    /// `filepath.Dir` is what `writeFileLocally` hands to `MkdirAll`. `Dir("a/b/")` is `"a/b"`,
    /// not `"a"` — a trailing separator makes the last element empty and the *directory* the
    /// whole path.
    #[test]
    fn filepath_dir_matches_go() {
        let oracle = filestore_oracle();
        let cases = oracle["filepath_dir"].as_array().unwrap();
        assert!(cases.len() >= 20, "corpus should cover Dir's branches");
        for case in cases {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                dir(input),
                case["out"].as_str().unwrap(),
                "filepath.Dir({input:?})"
            );
        }
    }

    /// `filepath.Join` is `path.Join` on Unix, and the local file backend calls it on every
    /// operation. Asserted against the *filestore* corpus rather than the stdlib one because
    /// these are the argument shapes a backend sees — including the `..` rows, where the result
    /// names a file outside the configured directory.
    #[test]
    fn filepath_join_matches_go() {
        let oracle = filestore_oracle();
        for case in oracle["filepath_join2"].as_array().unwrap() {
            let a = case["a"].as_str().unwrap();
            let b = case["b"].as_str().unwrap();
            assert_eq!(
                join(&[a, b]),
                case["out"].as_str().unwrap(),
                "filepath.Join({a:?}, {b:?})"
            );
        }
        for case in oracle["filepath_join3"].as_array().unwrap() {
            let a = case["a"].as_str().unwrap();
            let b = case["b"].as_str().unwrap();
            let c = case["c"].as_str().unwrap();
            assert_eq!(
                join(&[a, b, c]),
                case["out"].as_str().unwrap(),
                "filepath.Join({a:?}, {b:?}, {c:?})"
            );
        }
    }
}
