//! The part of connector.go a connection is opened from: a DSN, in URL or `key=value` form.

use std::collections::BTreeMap;

use crate::error::Error;

/// What [`crate::Conn::connect`] needs from a DSN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub options: String,
    pub application_name: String,
    /// `connect_timeout`, in seconds; 0 waits indefinitely.
    pub connect_timeout: u64,
    /// `binary_parameters=yes`.
    pub binary_parameters: bool,
    /// Every key lib/pq does not know, sent as a run-time parameter at startup.
    pub runtime: BTreeMap<String, String>,
}

/// Keys lib/pq recognises and this port ignores, because they only change the transport
/// (TLS material, Kerberos, service files, multi-host balancing) or are already fixed.
const IGNORED: &[&str] = &[
    "sslcert",
    "sslkey",
    "sslrootcert",
    "sslsni",
    "sslpassword",
    "sslinline",
    "sslnegotiation",
    "krbsrvname",
    "krbspn",
    "passfile",
    "service",
    "servicefile",
    "hostaddr",
    "load_balance_hosts",
    "target_session_attrs",
    "min_protocol_version",
    "max_protocol_version",
    "ssl_min_protocol_version",
    "ssl_max_protocol_version",
    "disable_prepared_binary_result",
];

impl Config {
    /// `NewConfig(dsn)` for the keys this port reads. `PG*` environment variables are not
    /// consulted.
    pub fn parse(dsn: &str) -> Result<Self, Error> {
        let opts = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
            parse_url(dsn)?
        } else {
            parse_kv(dsn)?
        };
        let mut cfg = Config {
            host: "localhost".into(),
            port: 5432,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            options: String::new(),
            application_name: String::new(),
            connect_timeout: 0,
            binary_parameters: false,
            runtime: BTreeMap::new(),
        };
        let mut fallback_application_name = None;
        let mut user_set = false;
        for (k, v) in opts {
            match k.as_str() {
                "host" => cfg.host = v,
                "port" => {
                    cfg.port = v
                        .parse()
                        .map_err(|_| Error::msg(format!("pq: wrong value for \"port\": {v:?}")))?;
                }
                "user" => {
                    cfg.user = v;
                    user_set = true;
                }
                "password" => cfg.password = v,
                "dbname" => cfg.database = v,
                "options" => cfg.options = v,
                "application_name" => cfg.application_name = v,
                "fallback_application_name" => fallback_application_name = Some(v),
                "connect_timeout" => {
                    cfg.connect_timeout = v.parse().map_err(|_| {
                        Error::msg(format!("pq: wrong value for \"connect_timeout\": {v:?}"))
                    })?;
                }
                "binary_parameters" => cfg.binary_parameters = v == "yes" || v == "true",
                "sslmode" => match v.as_str() {
                    "disable" | "allow" | "prefer" => {}
                    other => {
                        return Err(Error::msg(format!(
                            "gopq: sslmode {other:?} is not supported: this port has no TLS"
                        )));
                    }
                },
                "client_encoding" => {
                    if !matches!(
                        v.to_ascii_uppercase().as_str(),
                        "UTF8" | "UTF-8" | "UNICODE"
                    ) {
                        return Err(Error::msg(format!(
                            "pq: unsupported client_encoding {v:?}: must be absent or \"UTF8\""
                        )));
                    }
                }
                "datestyle" => {
                    if v != "ISO, MDY" {
                        return Err(Error::msg(format!(
                            "pq: unsupported datestyle {v:?}: must be absent or \"ISO, MDY\""
                        )));
                    }
                }
                k if IGNORED.contains(&k) => {}
                _ => {
                    cfg.runtime.insert(k, v);
                }
            }
        }
        if let Some(fallback) = fallback_application_name
            && cfg.application_name.is_empty()
        {
            cfg.application_name = fallback;
        }
        if !user_set {
            cfg.user = std::env::var("USER").unwrap_or_default();
        }
        Ok(cfg)
    }
}

fn percent_decode(s: &str) -> String {
    form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// `convertURL`: user, password, host, port, the path as the database, then the query.
fn parse_url(dsn: &str) -> Result<Vec<(String, String)>, Error> {
    let rest = dsn
        .split_once("://")
        .map(|(_, r)| r)
        .ok_or_else(|| Error::msg("invalid connection protocol"))?;
    let (authority_path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = match authority_path.find('/') {
        Some(i) => (&authority_path[..i], &authority_path[i..]),
        None => (authority_path, ""),
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let mut out = Vec::new();
    let mut accrue = |k: &str, v: String| {
        if !v.is_empty() {
            out.push((k.to_owned(), v));
        }
    };
    if let Some(userinfo) = userinfo {
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        accrue("user", percent_decode(user));
        accrue("password", percent_decode(password));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || hostport.starts_with('[') => {
            accrue(
                "host",
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned(),
            );
            accrue("port", port.to_owned());
        }
        _ => accrue("host", hostport.to_owned()),
    }
    if !path.is_empty() {
        accrue("dbname", percent_decode(&path[1..]));
    }
    // `u.Query()` then `q.Get(k)`: the first value of each key.
    let mut seen = std::collections::BTreeSet::new();
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        if seen.insert(k.to_string()) {
            accrue(&k, v.into_owned());
        }
    }
    Ok(out)
}

/// `fromDSN` for `key=value` strings: whitespace-separated, single-quoted values with
/// backslash escapes.
fn parse_kv(dsn: &str) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    let mut chars = dsn.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        loop {
            match chars.next() {
                None => {
                    return Err(Error::msg(format!(
                        "missing \"=\" after {key:?} in connection info string\""
                    )));
                }
                Some(c) if c.is_whitespace() || c == '=' => {
                    if c != '=' {
                        while chars.peek().is_some_and(|c| c.is_whitespace()) {
                            chars.next();
                        }
                        if chars.next() != Some('=') {
                            return Err(Error::msg(format!(
                                "missing \"=\" after {key:?} in connection info string\""
                            )));
                        }
                    }
                    break;
                }
                Some(c) => key.push(c),
            }
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next() {
                    None => {
                        return Err(Error::msg(
                            "unterminated quoted string literal in connection string",
                        ));
                    }
                    Some('\'') => break,
                    Some('\\') => {
                        if let Some(c) = chars.next() {
                            value.push(c);
                        }
                    }
                    Some(c) => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
                if c == '\\' {
                    match chars.next() {
                        Some(e) => value.push(e),
                        None => return Err(Error::msg("missing character after backslash")),
                    }
                } else {
                    value.push(c);
                }
            }
        }
        out.push((key, value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_dsn_and_its_key_value_twin() {
        let cfg = Config::parse(
            "postgres://mmuser:mm%40pw@localhost:5434/mattermost?sslmode=disable&connect_timeout=10&search_path=x",
        )
        .unwrap();
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.port, 5434);
        assert_eq!(cfg.user, "mmuser");
        assert_eq!(cfg.password, "mm@pw");
        assert_eq!(cfg.database, "mattermost");
        assert_eq!(cfg.connect_timeout, 10);
        assert_eq!(
            cfg.runtime.get("search_path").map(String::as_str),
            Some("x")
        );
        let kv = Config::parse(
            "host=localhost port=5434 user=mmuser password='mm@pw' dbname=mattermost sslmode=disable connect_timeout=10 search_path=x",
        )
        .unwrap();
        assert_eq!(kv, cfg);
    }

    #[test]
    fn tls_and_other_encodings_are_refused() {
        assert!(Config::parse("host=x sslmode=require user=u").is_err());
        assert!(Config::parse("host=x client_encoding=LATIN1 user=u").is_err());
        assert!(Config::parse("host=x datestyle='ISO, DMY' user=u").is_err());
        assert!(
            Config::parse("host=x binary_parameters=yes user=u")
                .unwrap()
                .binary_parameters
        );
    }
}
