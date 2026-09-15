//! Port of the log-reading half of `platform/log.go` (`GetLogsSkipSend`, `GetLogFile`, the two
//! filters), the path helpers of `config/logger.go` it calls, and the `Server.GetLogs` /
//! `QueryLogs` pair of `app/admin.go` — everything behind `GET /api/v4/logs`,
//! `POST /api/v4/logs/query` and `GET /api/v4/logs/download`.
//!
//! # The file is found the way Go finds it, from the working directory
//!
//! `LogSettings.FileLocation` is `""` on a stock server, and then the file is
//! `<FindDir("logs")>/mattermost.log`: `fileutils.FindDir` looks for a `logs` directory in the
//! working directory and four of its ancestors, then in the same five places relative to the
//! executable, and answers `./` when none exists. The logging *root* the path is validated
//! against is `MM_LOG_PATH` or that same search — so the two servers agree exactly when they
//! run from the same place, which is why `scripts/mm-api-env.sh` launches this process from the
//! Go server's run directory. There is no second source of truth: this process writes no log
//! file of its own, and what it serves is the file the configuration names.
//!
//! # `ValidateLogFilePath` resolves the file's symlinks and not the root's
//!
//! `filepath.EvalSymlinks` on the file, `filepath.Abs` alone on the root, then a string-prefix
//! comparison. On a checkout where `reference/.build` is a symlink the resolved file lives
//! under the target and the unresolved root under the link, and every read is the 403
//! `api.admin.file_read_error` — on both servers, which is the point of copying the asymmetry
//! rather than fixing it. Measured against Go on 2026-09-15.
//!
//! # Every line but the file's first carries its leading newline
//!
//! `GetLogsSkipSend` scans backwards a byte at a time and, on a `\n`, slices from **that byte**
//! to the end of the line — so the string appended is `"\n{…}"` for every line except the
//! one at offset zero. `json.Unmarshal` skips the whitespace, so the filters never notice; the
//! client sees it in every element of the array. Reproduced, since it is the wire.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use mm_model::system::{LogEntry, LogFilter};
use mm_model::utils::{AppError, AppResult};

use crate::App;

/// `config.LogFilename` (config/logger.go:26).
pub const LOG_FILENAME: &str = "mattermost.log";

/// The environment variable `config.GetLogRootPath` reads first.
const LOG_ROOT_ENV: &str = "MM_LOG_PATH";

/// `web.LogsPerPageDefault` and `LogsPerPageMaximum` (web/params.go:21-22) — the same number,
/// so a missing, negative or over-large `logs_per_page` all read 10,000.
pub const LOGS_PER_PAGE_DEFAULT: i64 = 10_000;
pub const LOGS_PER_PAGE_MAXIMUM: i64 = 10_000;

/// `fileutils.CommonBaseSearchPaths` (fileutils.go:14) without `server.GetPackagePath()`, which
/// is the Go *source* tree's package directory baked in at build time — a place that holds no
/// `logs` directory on any deployment and is not a location this binary has.
const COMMON_BASE_SEARCH_PATHS: [&str; 5] = [".", "..", "../..", "../../..", "../../../.."];

/// Port of `filepath.Clean` for Unix paths (path/filepath/path.go): `.` components dropped,
/// `..` folded lexically, repeated separators collapsed, a rooted path kept rooted, and `.` for
/// an empty result.
fn go_clean(path: &Path) -> PathBuf {
    let rooted = path.is_absolute();
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(last) if last != ".." => {
                    parts.pop();
                }
                _ if rooted => {}
                _ => parts.push("..".into()),
            },
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::Prefix(_) => {}
        }
    }
    let mut out = PathBuf::new();
    if rooted {
        out.push("/");
    }
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Port of `os.Getwd` (os/getwd.go:26) — **`$PWD` when it names the current directory**, and
/// the kernel's answer only otherwise.
///
/// Not `std::env::current_dir`, which is `getcwd(3)` and returns the *physical* path with every
/// symlink resolved. Go's "clumsy but widespread kludge" prefers the logical path the shell
/// left in `PWD` when a stat of it and of `.` agree, so a server started inside a symlinked
/// directory sees the link in its working directory — and the log-root check below, which
/// resolves the file's symlinks and not the root's, then refuses a file that lives under the
/// link's target. That refusal is the Go server's on this project's worktrees, and reproducing
/// it needs the same working directory, spelt the same way.
fn go_getwd() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    if let Some(pwd) = std::env::var_os("PWD") {
        let pwd = PathBuf::from(pwd);
        if pwd.is_absolute() {
            let dot = std::fs::metadata(".")?;
            if let Ok(named) = std::fs::metadata(&pwd) {
                if named.dev() == dot.dev() && named.ino() == dot.ino() {
                    return Ok(pwd);
                }
            }
        }
    }
    std::env::current_dir()
}

/// Port of `filepath.Abs`: the working directory ([`go_getwd`]) joined ahead of a relative
/// path, then [`go_clean`] — lexical only, no symlink is followed.
fn go_abs(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(go_clean(path));
    }
    let cwd = go_getwd()?;
    Ok(go_clean(&cwd.join(path)))
}

/// Port of `fileutils.findPath` with `workingDirFirst = true` and the `IsDir` filter
/// (fileutils.go:36): an absolute `path` is answered by its own existence, filter or no filter;
/// a relative one is tried under each base path from the working directory, then under each
/// base path from the (symlink-resolved) executable's directory.
fn find_dir_path(path: &str) -> Option<PathBuf> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return std::fs::metadata(candidate)
            .ok()
            .map(|_| candidate.to_path_buf());
    }
    let mut search: Vec<PathBuf> = COMMON_BASE_SEARCH_PATHS.iter().map(PathBuf::from).collect();
    if let Some(binary_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| std::fs::canonicalize(exe).ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        search.extend(
            COMMON_BASE_SEARCH_PATHS
                .iter()
                .map(|base| binary_dir.join(base)),
        );
    }
    for parent in search {
        let Ok(found) = go_abs(&parent.join(candidate)) else {
            continue;
        };
        if std::fs::metadata(&found).is_ok_and(|info| info.is_dir()) {
            return Some(found);
        }
    }
    None
}

/// Port of `fileutils.FindDir` (fileutils.go:98): the directory, or `./` and `false`.
pub fn find_dir(dir: &str) -> (PathBuf, bool) {
    match find_dir_path(dir) {
        Some(found) => (found, true),
        None => (PathBuf::from("./"), false),
    }
}

/// Port of `config.GetLogFileLocation` (config/logger.go:143): `FileLocation`, or the found
/// `logs` directory when it is empty, joined with `mattermost.log`. `filepath.Join` cleans, so
/// the not-found `./` yields a bare `mattermost.log` in the working directory.
pub fn get_log_file_location(file_location: &str) -> PathBuf {
    let dir = if file_location.is_empty() {
        find_dir("logs").0
    } else {
        PathBuf::from(file_location)
    };
    go_clean(&dir.join(LOG_FILENAME))
}

/// Port of `config.GetLogRootPath` (config/logger.go:156): `MM_LOG_PATH` made absolute, else
/// the found `logs` directory made absolute.
pub fn get_log_root_path() -> PathBuf {
    if let Some(env_path) = std::env::var_os(LOG_ROOT_ENV).filter(|v| !v.is_empty()) {
        let env_path = PathBuf::from(env_path);
        if let Ok(abs) = go_abs(&env_path) {
            return abs;
        }
    }
    let (logs_dir, _) = find_dir("logs");
    go_abs(&logs_dir).unwrap_or(logs_dir)
}

/// Port of `config.ValidateLogFilePath` (config/logger.go:177). See the module docs for the
/// asymmetry between the two sides of the comparison; the string form of each error is Go's.
pub fn validate_log_file_path(file_path: &Path, logging_root: &Path) -> Result<(), String> {
    let mut abs_path = go_abs(file_path)
        .map_err(|err| format!("cannot resolve path {}: {err}", file_path.display()))?;
    match std::fs::canonicalize(&abs_path) {
        Ok(real) => abs_path = real,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "cannot resolve symlinks for {}: {err}",
                abs_path.display()
            ));
        }
    }
    let abs_root = go_abs(logging_root).map_err(|err| {
        format!(
            "cannot resolve logging root {}: {err}",
            logging_root.display()
        )
    })?;

    let abs_path = abs_path.to_string_lossy().into_owned();
    let abs_root = abs_root.to_string_lossy().into_owned();
    let root_with_sep = if abs_root.ends_with('/') {
        abs_root.clone()
    } else {
        format!("{abs_root}/")
    };
    if abs_path != abs_root && !abs_path.starts_with(&root_with_sep) {
        return Err(format!(
            "path {} is outside logging root {abs_root}",
            file_path.display()
        ));
    }
    Ok(())
}

/// The `api.admin.file_read_error` every failure of `GetLogsSkipSend` is, at the status Go
/// gives that failure: 403 for the root check, 500 for everything the filesystem refuses.
fn file_read_error(status: i32) -> Box<AppError> {
    AppError::boxed(
        "getLogs",
        "api.admin.file_read_error",
        None,
        String::new(),
        status,
    )
}

/// Port of Go's `time.Parse` for the one layout the filters use
/// (`2006-01-02 15:04:05.999 -07:00`), as Go parses it: four-digit year, two-digit month and
/// day, **one- or two-digit hour** (`15` is `stdHour`, which `getnum` reads unpadded), two-digit
/// minute and second, an optional fraction of any length after `.` or `,` (the first nine digits
/// count), a space, and a signed `hh:mm` offset. Anything left over is an error, as are
/// out-of-range fields.
pub fn parse_go_log_time(value: &str) -> Option<DateTime<FixedOffset>> {
    let bytes = value.as_bytes();
    let mut i = 0usize;
    let digits = |i: &mut usize, min: usize, max: usize| -> Option<u32> {
        let start = *i;
        while *i < bytes.len() && *i - start < max && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i - start < min {
            return None;
        }
        value[start..*i].parse().ok()
    };
    let expect = |i: &mut usize, c: u8| -> Option<()> {
        if bytes.get(*i) == Some(&c) {
            *i += 1;
            Some(())
        } else {
            None
        }
    };

    let year = digits(&mut i, 4, 4)?;
    expect(&mut i, b'-')?;
    let month = digits(&mut i, 2, 2)?;
    expect(&mut i, b'-')?;
    let day = digits(&mut i, 2, 2)?;
    expect(&mut i, b' ')?;
    let hour = digits(&mut i, 1, 2)?;
    expect(&mut i, b':')?;
    let minute = digits(&mut i, 2, 2)?;
    expect(&mut i, b':')?;
    let second = digits(&mut i, 2, 2)?;
    // `.999`: present only when a separator is followed by a digit; then every digit is
    // consumed and the first nine are the nanoseconds.
    let mut nanos: u32 = 0;
    if matches!(bytes.get(i), Some(b'.' | b',')) && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
    {
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let fraction = &value[start..end.min(start + 9)];
        let scale = 10u32.pow(9 - fraction.len() as u32);
        nanos = fraction.parse::<u32>().ok()? * scale;
        i = end;
    }
    expect(&mut i, b' ')?;
    let sign = match bytes.get(i) {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    i += 1;
    let zone_hours = digits(&mut i, 2, 2)?;
    expect(&mut i, b':')?;
    let zone_minutes = digits(&mut i, 2, 2)?;
    if i != bytes.len() {
        return None;
    }
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in(year, month)
        || hour > 23
        || minute > 59
        || second > 59
        || zone_hours > 23
        || zone_minutes > 59
    {
        return None;
    }
    let offset =
        FixedOffset::east_opt(sign * (zone_hours as i32 * 3600 + zone_minutes as i32 * 60))?;
    offset
        .with_ymd_and_hms(year as i32, month, day, hour, minute, second)
        .single()?
        .with_nanosecond(nanos)
}

/// `daysIn(month, year)` — February leaps on the Gregorian rule.
fn days_in(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
    }
}

use chrono::Timelike;

/// Port of `isLogFilteredByLevel` (platform/log.go:311): no levels means nothing is filtered;
/// otherwise a line is filtered unless its level is one of them.
pub fn is_log_filtered_by_level(filter: &LogFilter, entry: &LogEntry) -> bool {
    match filter.log_levels.as_deref() {
        None | Some([]) => false,
        Some(levels) => !levels.iter().any(|level| level == &entry.level),
    }
}

/// Port of `isLogFilteredByDate` (platform/log.go:320) with `now` injected.
///
/// An unparsable `date_from` is the zero time, an unparsable `date_to` is now, and an
/// unparsable line timestamp is **not filtered**. A timestamp equal to either bound passes;
/// strictly between them passes; anything else is filtered.
pub fn is_log_filtered_by_date(filter: &LogFilter, entry: &LogEntry, now: DateTime<Utc>) -> bool {
    if filter.date_from.is_empty() && filter.date_to.is_empty() {
        return false;
    }
    let date_from = parse_go_log_time(&filter.date_from).unwrap_or_else(go_zero_time);
    let date_to = parse_go_log_time(&filter.date_to).unwrap_or_else(|| now.fixed_offset());
    let Some(timestamp) = parse_go_log_time(&entry.timestamp) else {
        tracing::debug!("Cannot parse timestamp, skipping");
        return false;
    };
    if timestamp == date_from || timestamp == date_to {
        return false;
    }
    if timestamp > date_from && timestamp < date_to {
        return false;
    }
    true
}

/// `time.Time{}`: 0001-01-01T00:00:00Z.
fn go_zero_time() -> DateTime<FixedOffset> {
    FixedOffset::east_opt(0)
        .and_then(|utc| utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0).single())
        .unwrap_or_else(|| Utc::now().fixed_offset())
}

/// `json.Unmarshal(line, &entry)` for `model.LogEntry`, whose two untagged fields match their
/// keys case-insensitively. `None` where Go's unmarshal errors — a line that is not a JSON
/// object, or a `timestamp`/`level` that is not a string — which the caller treats as "not
/// filtered". A missing key is the empty string.
pub fn parse_log_entry(line: &[u8]) -> Option<LogEntry> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let object = value.as_object()?;
    let field = |name: &str| -> Option<String> {
        let found = object.get(name).or_else(|| {
            object
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        });
        match found {
            None | Some(serde_json::Value::Null) => Some(String::new()),
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(_) => None,
        }
    };
    Some(LogEntry {
        timestamp: field("timestamp")?,
        level: field("level")?,
    })
}

/// The page of lines `GetLogsSkipSend` (platform/log.go:105) reads out of a non-empty file.
///
/// A backwards scan over the bytes, newest line first: every `\n` (and offset zero) ends a
/// line; lines are counted from the end and the page is the ones past `page × per_page`; a
/// line the filters reject inside the page gives its count back, so it consumes no slot; the
/// scan stops when `per_page` lines are held or the file's start is reached; the result is
/// reversed into file order. `per_page == 0` stops before the first line — Go's `nil`, which
/// the handler writes as `null`.
///
/// `None` is the `Seek(-1, SeekCurrent)` from offset zero that a file holding only `\n` (or
/// nothing) makes Go attempt: a 500 `file_read_error`, which is not what a reader would expect
/// an empty log to answer, and is what it answers.
pub fn lines_from_log(
    contents: &[u8],
    page: i64,
    per_page: i64,
    filter: &LogFilter,
    now: DateTime<Utc>,
) -> Option<Vec<String>> {
    let size = contents.len();
    let mut line_end_pos = match contents.last() {
        Some(b'\n') => size - 1,
        Some(_) => size,
        None => 0,
    };
    let mut cursor = line_end_pos;
    if cursor == 0 {
        return None;
    }
    let window = page.saturating_mul(per_page);
    let mut line_count: i64 = 0;
    let mut lines: Vec<String> = Vec::new();
    loop {
        let pos = cursor - 1;
        cursor = pos;
        let byte = contents[pos];
        if byte == b'\n' || pos == 0 {
            line_count += 1;
            if line_count > window {
                let line = &contents[pos..line_end_pos];
                let filtered = match parse_log_entry(line) {
                    Some(entry) => {
                        is_log_filtered_by_level(filter, &entry)
                            || is_log_filtered_by_date(filter, &entry, now)
                    }
                    None => {
                        tracing::debug!("Failed to parse line, skipping");
                        false
                    }
                };
                if filtered {
                    line_count -= 1;
                } else {
                    lines.push(String::from_utf8_lossy(line).into_owned());
                }
            }
            if pos == 0 {
                break;
            }
            line_end_pos = pos;
        }
        if lines.len() as i64 == per_page {
            break;
        }
    }
    lines.reverse();
    Some(lines)
}

/// Why `GetLogFile` (platform/log.go:202) refused. Each is Go's `error`, and the handler turns
/// all three into the same 500 `api.system.logs.download_bytes_buffer.app_error`.
#[derive(Debug, thiserror::Error)]
pub enum LogFileError {
    #[error("Unable to retrieve mattermost logs because LogSettings.EnableFile is set to false")]
    FileLoggingDisabled,
    #[error("log file path {path} is outside allowed logging directory: {reason}")]
    OutsideRoot { path: String, reason: String },
    #[error("failed read mattermost log file at path {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl App {
    /// Port of `PlatformService.GetLogsSkipSend` (platform/log.go:105) — see the module docs
    /// for the four things about it a reader would get wrong. `LogSettings.EnableFile` off is a
    /// single empty line and no file access at all.
    #[tracing::instrument(skip(self, filter), fields(lines))]
    pub async fn get_logs_skip_send(
        &self,
        page: i64,
        per_page: i64,
        filter: &LogFilter,
    ) -> AppResult<Vec<String>> {
        if !self.config().log_enable_file {
            return Ok(vec![String::new()]);
        }
        let log_file = get_log_file_location(&self.config().log_file_location);
        if let Err(err) = validate_log_file_path(&log_file, &get_log_root_path()) {
            tracing::error!(
                path = %log_file.display(),
                config_section = "LogSettings.FileLocation",
                error = %err,
                "Blocked attempt to read log file outside allowed root"
            );
            return Err(file_read_error(403));
        }
        let contents = tokio::fs::read(&log_file).await.map_err(|err| {
            tracing::warn!(path = %log_file.display(), error = %err, "log file read failed");
            file_read_error(500)
        })?;
        let lines = lines_from_log(&contents, page, per_page, filter, Utc::now())
            .ok_or_else(|| file_read_error(500))?;
        tracing::Span::current().record("lines", lines.len());
        Ok(lines)
    }

    /// Port of `Server.GetLogs` (app/admin.go:24) for a server with no cluster interface: the
    /// local page under an empty filter, and neither the hostname banner nor the peers' lines.
    pub async fn get_logs(&self, page: i64, per_page: i64) -> AppResult<Vec<String>> {
        self.get_logs_skip_send(page, per_page, &LogFilter::default())
            .await
    }

    /// Port of `Server.QueryLogs` (app/admin.go:60) with no cluster: the node is `"default"`,
    /// and it is read only when `server_names` is empty or names it — a filter naming other
    /// nodes alone answers an empty map, without touching the file.
    #[tracing::instrument(skip(self, filter))]
    pub async fn query_logs(
        &self,
        page: i64,
        per_page: i64,
        filter: &LogFilter,
    ) -> AppResult<BTreeMap<String, Vec<String>>> {
        const DEFAULT_NODE: &str = "default";
        let mut log_data = BTreeMap::new();
        match filter.server_names.as_deref() {
            Some(names) if !names.is_empty() => {
                for node_name in names {
                    if node_name == DEFAULT_NODE {
                        let lines = self.get_logs_skip_send(page, per_page, filter).await?;
                        log_data.insert(node_name.clone(), lines);
                    }
                }
            }
            _ => {
                let lines = self.get_logs_skip_send(page, per_page, filter).await?;
                log_data.insert(DEFAULT_NODE.to_owned(), lines);
            }
        }
        Ok(log_data)
    }

    /// Port of `PlatformService.GetLogFile` (platform/log.go:202): the validated path of the
    /// file `GET /api/v4/logs/download` streams, its bytes read to prove them readable — Go
    /// reads the whole file into the response; the handler opens it again to stream it.
    #[tracing::instrument(skip(self))]
    pub async fn get_log_file(&self) -> Result<PathBuf, LogFileError> {
        if !self.config().log_enable_file {
            return Err(LogFileError::FileLoggingDisabled);
        }
        let log_file = get_log_file_location(&self.config().log_file_location);
        if let Err(reason) = validate_log_file_path(&log_file, &get_log_root_path()) {
            tracing::error!(
                path = %log_file.display(),
                config_section = "LogSettings.FileLocation",
                error = %reason,
                "Blocked attempt to read log file outside allowed root"
            );
            return Err(LogFileError::OutsideRoot {
                path: log_file.to_string_lossy().into_owned(),
                reason,
            });
        }
        tokio::fs::metadata(&log_file)
            .await
            .map_err(|source| LogFileError::Read {
                path: log_file.to_string_lossy().into_owned(),
                source,
            })?;
        Ok(log_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        parse_go_log_time(s).unwrap().to_utc()
    }

    /// `filepath.Clean`'s own table, the Unix half.
    #[test]
    fn go_clean_matches_filepath_clean() {
        for (input, want) in [
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("", "."),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("a/b/c/", "a/b/c"),
            ("./", "."),
            ("../", ".."),
            ("../../", "../.."),
            ("/abc/", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("///abc", "/abc"),
            ("//abc//", "/abc"),
            ("abc//", "abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            ("/../abc", "/abc"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ] {
            assert_eq!(go_clean(Path::new(input)), Path::new(want), "{input:?}");
        }
    }

    /// The resolved file against the unresolved root, as strings: the exact root passes, a
    /// child passes, a sibling whose name merely extends the root's does not.
    #[test]
    fn the_root_check_is_a_string_prefix_with_a_separator() {
        let dir = std::env::temp_dir().join(format!("mmrs-logs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::create_dir_all(dir.join("logs2")).unwrap();
        let root = dir.join("logs");
        assert_eq!(
            validate_log_file_path(&root.join("mattermost.log"), &root),
            Ok(())
        );
        assert_eq!(validate_log_file_path(&root, &root), Ok(()));
        let sibling = dir.join("logs2").join("mattermost.log");
        assert!(
            validate_log_file_path(&sibling, &root)
                .unwrap_err()
                .starts_with(&format!(
                    "path {} is outside logging root",
                    sibling.display()
                ))
        );
        // A symlink on the file's side is resolved; the root is not, so a link *into* the root
        // from outside fails the prefix even though the bytes live inside it.
        let link = dir.join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        std::fs::write(root.join("mattermost.log"), b"x\n").unwrap();
        assert_eq!(
            validate_log_file_path(&link.join("mattermost.log"), &root),
            Ok(())
        );
        assert!(validate_log_file_path(&root.join("mattermost.log"), &link).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `$PWD` wins when it names `.`, however it is spelt; a stale `$PWD` loses to `getcwd`.
    #[test]
    fn getwd_prefers_a_matching_pwd() {
        let physical = std::env::current_dir().unwrap();
        // The variable is process-global, so this test does not set it; it checks the rule
        // against whatever the harness left, which is either the same directory or unset.
        let answer = go_getwd().unwrap();
        match std::env::var_os("PWD") {
            Some(pwd)
                if std::fs::metadata(&pwd).ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.dev(), m.ino())
                }) == std::fs::metadata(&physical).ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.dev(), m.ino())
                }) =>
            {
                assert_eq!(answer, PathBuf::from(pwd))
            }
            _ => assert_eq!(answer, physical),
        }
    }

    /// The layout, field by field: the unpadded hour, the optional fraction, the offset.
    #[test]
    fn the_go_layout_parses_as_go_parses_it() {
        let t = parse_go_log_time("2026-09-15 12:36:22.095 +05:30").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-09-15T12:36:22.095+05:30");
        assert_eq!(
            parse_go_log_time("2026-09-15 9:06:02 -07:00")
                .unwrap()
                .to_rfc3339(),
            "2026-09-15T09:06:02-07:00"
        );
        assert_eq!(
            parse_go_log_time("2026-09-15 09:06:02,1234567891 +00:00")
                .unwrap()
                .nanosecond(),
            123_456_789
        );
        for bad in [
            "",
            "2026-09-15T12:36:22.095+05:30",
            "2026-9-15 12:36:22 +05:30",
            "2026-09-15 12:36:22 +0530",
            "2026-09-15 12:36:22 +05:30 ",
            "2026-13-15 12:36:22 +05:30",
            "2026-02-30 12:36:22 +05:30",
            "2026-09-15 24:36:22 +05:30",
            "2026-09-15 12:36:22. +05:30",
        ] {
            assert!(parse_go_log_time(bad).is_none(), "{bad:?}");
        }
    }

    /// The level filter is a membership test that an empty list switches off.
    #[test]
    fn the_level_filter_matches_go() {
        let entry = LogEntry {
            timestamp: String::new(),
            level: "error".to_owned(),
        };
        let mut filter = LogFilter::default();
        assert!(!is_log_filtered_by_level(&filter, &entry));
        filter.log_levels = Some(vec![]);
        assert!(!is_log_filtered_by_level(&filter, &entry));
        filter.log_levels = Some(vec!["info".to_owned()]);
        assert!(is_log_filtered_by_level(&filter, &entry));
        filter.log_levels = Some(vec!["info".to_owned(), "error".to_owned()]);
        assert!(!is_log_filtered_by_level(&filter, &entry));
    }

    /// The date filter: both bounds inclusive, a bad `from` is the zero time, a bad `to` is
    /// now, and a bad line timestamp passes.
    #[test]
    fn the_date_filter_matches_go() {
        let now = at("2026-09-15 12:00:00 +00:00");
        let line = |ts: &str| LogEntry {
            timestamp: ts.to_owned(),
            level: String::new(),
        };
        let filter = |from: &str, to: &str| LogFilter {
            date_from: from.to_owned(),
            date_to: to.to_owned(),
            ..LogFilter::default()
        };
        let f = filter("2026-09-15 10:00:00 +00:00", "2026-09-15 11:00:00 +00:00");
        assert!(!is_log_filtered_by_date(
            &filter("", ""),
            &line("garbage"),
            now
        ));
        assert!(!is_log_filtered_by_date(
            &f,
            &line("2026-09-15 10:00:00 +00:00"),
            now
        ));
        assert!(!is_log_filtered_by_date(
            &f,
            &line("2026-09-15 11:00:00 +00:00"),
            now
        ));
        assert!(!is_log_filtered_by_date(
            &f,
            &line("2026-09-15 10:30:00 +00:00"),
            now
        ));
        // The same instant in another zone is equal, not filtered.
        assert!(!is_log_filtered_by_date(
            &f,
            &line("2026-09-15 15:30:00 +05:30"),
            now
        ));
        assert!(is_log_filtered_by_date(
            &f,
            &line("2026-09-15 09:59:59.999 +00:00"),
            now
        ));
        assert!(is_log_filtered_by_date(
            &f,
            &line("2026-09-15 11:00:00.001 +00:00"),
            now
        ));
        assert!(!is_log_filtered_by_date(&f, &line("not a time"), now));
        // A bad `from` is year 1; a bad `to` is now.
        let open = filter("bad", "bad");
        assert!(!is_log_filtered_by_date(
            &open,
            &line("1999-01-01 00:00:00 +00:00"),
            now
        ));
        assert!(is_log_filtered_by_date(
            &open,
            &line("2026-09-15 12:00:01 +00:00"),
            now
        ));
        assert!(!is_log_filtered_by_date(
            &open,
            &line("2026-09-15 12:00:00 +00:00"),
            now
        ));
    }

    /// The reverse scan: leading newlines on every line but the first, paging from the end,
    /// filtered lines giving their slot back, and the two degenerate files.
    #[test]
    fn the_page_is_read_from_the_end_with_leading_newlines() {
        let now = Utc::now();
        let none = LogFilter::default();
        let file = b"{\"level\":\"info\",\"n\":1}\n{\"level\":\"error\",\"n\":2}\n{\"level\":\"info\",\"n\":3}\n";
        assert_eq!(
            lines_from_log(file, 0, 10, &none, now).unwrap(),
            vec![
                "{\"level\":\"info\",\"n\":1}".to_owned(),
                "\n{\"level\":\"error\",\"n\":2}".to_owned(),
                "\n{\"level\":\"info\",\"n\":3}".to_owned(),
            ]
        );
        assert_eq!(
            lines_from_log(file, 0, 2, &none, now).unwrap(),
            vec![
                "\n{\"level\":\"error\",\"n\":2}".to_owned(),
                "\n{\"level\":\"info\",\"n\":3}".to_owned(),
            ]
        );
        assert_eq!(
            lines_from_log(file, 1, 2, &none, now).unwrap(),
            vec!["{\"level\":\"info\",\"n\":1}".to_owned()]
        );
        assert_eq!(
            lines_from_log(file, 5, 2, &none, now).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            lines_from_log(file, 0, 0, &none, now).unwrap(),
            Vec::<String>::new()
        );
        // No trailing newline: the last line runs to the end of the file.
        assert_eq!(
            lines_from_log(b"a\nb", 0, 10, &none, now).unwrap(),
            vec!["a".to_owned(), "\nb".to_owned()]
        );
        // A filtered line inside the page gives its slot to the next older line.
        let errors_only = LogFilter {
            log_levels: Some(vec!["error".to_owned()]),
            ..LogFilter::default()
        };
        assert_eq!(
            lines_from_log(file, 0, 1, &errors_only, now).unwrap(),
            vec!["\n{\"level\":\"error\",\"n\":2}".to_owned()]
        );
        // Unparsable lines are never filtered.
        assert_eq!(
            lines_from_log(b"not json\n", 0, 5, &errors_only, now).unwrap(),
            vec!["not json".to_owned()]
        );
        assert!(lines_from_log(b"", 0, 5, &none, now).is_none());
        assert!(lines_from_log(b"\n", 0, 5, &none, now).is_none());
    }

    /// `LogEntry`'s untagged fields match case-insensitively, and a non-string is an error.
    #[test]
    fn the_entry_parse_is_gos_unmarshal() {
        let entry = parse_log_entry(br#"{"Timestamp":"t","LEVEL":"debug","msg":"x"}"#).unwrap();
        assert_eq!(
            (entry.timestamp.as_str(), entry.level.as_str()),
            ("t", "debug")
        );
        let entry = parse_log_entry(br#"{"msg":"x"}"#).unwrap();
        assert_eq!((entry.timestamp.as_str(), entry.level.as_str()), ("", ""));
        assert!(parse_log_entry(br#"{"level":5}"#).is_none());
        assert!(parse_log_entry(b"[1]").is_none());
        assert!(parse_log_entry(b"nope").is_none());
    }

    /// `FindDir` on a name no ancestor holds is `./`, and the file location is then a bare
    /// `mattermost.log`; a configured directory is joined and cleaned.
    #[test]
    fn the_file_location_follows_get_log_file_location() {
        assert_eq!(
            get_log_file_location("/var/log/mm/"),
            PathBuf::from("/var/log/mm/mattermost.log")
        );
        assert_eq!(
            find_dir("mmrs-no-such-directory-anywhere"),
            (PathBuf::from("./"), false)
        );
    }
}
