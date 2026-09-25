//! Port of conn.go, conn_go18.go, stmt.go and rows.go: a connection as lib/pq drives it.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::config::Config;
use crate::encode::{self, Format};
use crate::error::{Error, IN_FAILED_TRANSACTION, PqError, QUERY_IN_PROGRESS, UNEXPECTED_READY};
use crate::value::{NamedValue, Value};

// Backend message codes (internal/proto).
const PARSE_COMPLETE: u8 = b'1';
const BIND_COMPLETE: u8 = b'2';
const CLOSE_COMPLETE: u8 = b'3';
const NOTIFICATION_RESPONSE: u8 = b'A';
const COMMAND_COMPLETE: u8 = b'C';
const DATA_ROW: u8 = b'D';
const ERROR_RESPONSE: u8 = b'E';
const EMPTY_QUERY_RESPONSE: u8 = b'I';
const BACKEND_KEY_DATA: u8 = b'K';
const NOTICE_RESPONSE: u8 = b'N';
const AUTHENTICATION_REQUEST: u8 = b'R';
const PARAMETER_STATUS: u8 = b'S';
const ROW_DESCRIPTION: u8 = b'T';
const READY_FOR_QUERY: u8 = b'Z';
const NO_DATA: u8 = b'n';
const PARAMETER_DESCRIPTION: u8 = b't';
const NEGOTIATE_PROTOCOL_VERSION: u8 = b'v';

// Frontend message codes.
const BIND: u8 = b'B';
const CLOSE: u8 = b'C';
const DESCRIBE: u8 = b'D';
const EXECUTE: u8 = b'E';
const PARSE: u8 = b'P';
const QUERY: u8 = b'Q';
const SYNC: u8 = b'S';
const TERMINATE: u8 = b'X';
const PASSWORD_MESSAGE: u8 = b'p';

const PROTOCOL_VERSION_30: i32 = 3 << 16;
const MAX_ERRLEN: usize = 30_000;

/// `colFmtDataAllBinary`: one result-format code, 1.
const COL_FMT_ALL_BINARY: [u8; 4] = [0, 1, 0, 1];
/// `colFmtDataAllText`: no result-format codes.
const COL_FMT_ALL_TEXT: [u8; 2] = [0, 0];

const TXN_IDLE: u8 = b'I';
const TXN_IN_TRANSACTION: u8 = b'T';
const TXN_FAILED: u8 = b'E';

/// `transactionStatus.String`.
fn txn_status_string(s: u8) -> String {
    match s {
        TXN_IDLE => "idle".into(),
        TXN_IN_TRANSACTION => "idle in transaction".into(),
        TXN_FAILED => "in a failed transaction".into(),
        other => format!("pq: unknown transactionStatus {other}"),
    }
}

/// `%q` of a `proto.ResponseCode`: its `String()`, quoted.
fn response_code_q(r: u8) -> String {
    let name = match r {
        PARSE_COMPLETE => "ParseComplete",
        BIND_COMPLETE => "BindComplete",
        CLOSE_COMPLETE => "CloseComplete",
        NOTIFICATION_RESPONSE => "NotificationResponse",
        COMMAND_COMPLETE => "CommandComplete",
        DATA_ROW => "DataRow",
        ERROR_RESPONSE => "ErrorResponse",
        b'G' => "CopyInResponse",
        b'H' => "CopyOutResponse",
        EMPTY_QUERY_RESPONSE => "EmptyQueryResponse",
        BACKEND_KEY_DATA => "BackendKeyData",
        NOTICE_RESPONSE => "NoticeResponse",
        AUTHENTICATION_REQUEST => "AuthRequest",
        PARAMETER_STATUS => "ParamStatus",
        ROW_DESCRIPTION => "RowDescription",
        b'V' => "FunctionCallResponse",
        b'W' => "CopyBothResponse",
        READY_FOR_QUERY => "ReadyForQuery",
        NO_DATA => "NoData",
        b's' => "PortalSuspended",
        PARAMETER_DESCRIPTION => "ParamDescription",
        NEGOTIATE_PROTOCOL_VERSION => "NegotiateProtocolVersion",
        b'c' => "CopyDone",
        b'd' => "CopyData",
        _ => "<unknown>",
    };
    let c = if r <= 0x1f || r == 0x7f {
        format!("0x{r:x}")
    } else {
        char::from(r).to_string()
    };
    format!("{:?}", format!("({c}) {name}"))
}

/// `writeBuf`: messages built back to back, each length filled in when the next starts.
struct WriteBuf {
    buf: Vec<u8>,
    pos: usize,
}

impl WriteBuf {
    fn new(code: u8) -> Self {
        WriteBuf {
            buf: vec![code, 0, 0, 0, 0],
            pos: 1,
        }
    }
    fn int32(&mut self, n: i32) {
        self.buf.extend_from_slice(&n.to_be_bytes());
    }
    fn int16(&mut self, n: i32) {
        self.buf.extend_from_slice(&(n as u16).to_be_bytes());
    }
    fn string(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    fn byte(&mut self, c: u8) {
        self.buf.push(c);
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn fill_length(&mut self) {
        let len = (self.buf.len() - self.pos) as u32;
        self.buf[self.pos..self.pos + 4].copy_from_slice(&len.to_be_bytes());
    }
    fn next(&mut self, code: u8) {
        self.fill_length();
        self.pos = self.buf.len() + 1;
        self.buf.extend_from_slice(&[code, 0, 0, 0, 0]);
    }
    fn wrap(mut self) -> Vec<u8> {
        self.fill_length();
        self.buf
    }
}

/// `readBuf`: a cursor over one message's body. Go panics on a short message; this errors.
struct ReadBuf<'a>(&'a [u8]);

fn short() -> Error {
    Error::msg("pq: invalid message format; expected string terminator")
}

impl<'a> ReadBuf<'a> {
    fn int32(&mut self) -> Result<i32, Error> {
        let b = self.next(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn oid(&mut self) -> Result<u32, Error> {
        let b = self.next(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn int16(&mut self) -> Result<usize, Error> {
        let b = self.next(2)?;
        Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }
    fn string(&mut self) -> Result<String, Error> {
        let i = self.0.iter().position(|&b| b == 0).ok_or_else(short)?;
        let s = String::from_utf8_lossy(&self.0[..i]).into_owned();
        self.0 = &self.0[i + 1..];
        Ok(s)
    }
    fn next(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(short());
        }
        let (v, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(v)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.next(1)?[0])
    }
}

/// `fieldDesc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDesc {
    pub oid: u32,
    pub len: i32,
    pub modifier: i32,
}

impl FieldDesc {
    /// `fieldDesc.Name`.
    pub fn name(&self) -> &'static str {
        crate::oid::type_name(self.oid)
    }

    /// `fieldDesc.PrecisionScale`.
    pub fn precision_scale(&self) -> (i64, i64, bool) {
        match self.oid {
            encode::T_NUMERIC | encode::T__NUMERIC => {
                let m = self.modifier - 4;
                (i64::from((m >> 16) & 0xffff), i64::from(m & 0xffff), true)
            }
            _ => (0, 0, false),
        }
    }
}

/// `rowsHeader`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowsHeader {
    /// `nil` when no `RowDescription` arrived.
    pub col_names: Option<Vec<String>>,
    pub col_typs: Vec<FieldDesc>,
    col_fmts: Vec<Format>,
}

/// A command's result: `driver.RowsAffected(n)`, or lib/pq's `noRows` for an empty statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecResult {
    RowsAffected(i64),
    Empty,
}

impl ExecResult {
    /// `LastInsertId()`: never supported.
    pub fn last_insert_id(&self) -> Result<i64, Error> {
        match self {
            ExecResult::RowsAffected(_) => {
                Err(Error::msg("LastInsertId is not supported by this driver"))
            }
            ExecResult::Empty => Err(Error::msg(
                "no LastInsertId available after the empty statement",
            )),
        }
    }

    /// `RowsAffected()`.
    pub fn rows_affected(&self) -> Result<i64, Error> {
        match self {
            ExecResult::RowsAffected(n) => Ok(*n),
            ExecResult::Empty => Err(Error::msg(
                "no RowsAffected available after the empty statement",
            )),
        }
    }
}

/// `*pq.conn`.
pub struct Conn {
    stream: BufReader<TcpStream>,
    /// `syncErr`: the first error that made the connection unusable.
    err: Option<Error>,
    in_progress: bool,
    txn_status: u8,
    namei: i64,
    saved: Option<(u8, Vec<u8>)>,
    binary_parameters: bool,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("txn_status", &char::from(self.txn_status))
            .field("bad", &self.err.is_some())
            .finish_non_exhaustive()
    }
}

/// `parseError`.
fn parse_error(body: &[u8], q: &str) -> Error {
    let mut e = PqError {
        query: q.to_owned(),
        ..PqError::default()
    };
    let mut r = ReadBuf(body);
    while let Ok(t) = r.byte() {
        if t == 0 {
            break;
        }
        let Ok(msg) = r.string() else { break };
        match t {
            b'S' => e.severity = msg,
            b'C' => e.code = msg,
            b'M' => e.message = msg,
            b'D' => e.detail = msg,
            b'H' => e.hint = msg,
            b'P' => e.position = msg,
            b'p' => e.internal_position = msg,
            b'q' => e.internal_query = msg,
            b'W' => e.r#where = msg,
            b's' => e.schema = msg,
            b't' => e.table = msg,
            b'c' => e.column = msg,
            b'd' => e.data_type_name = msg,
            b'n' => e.constraint = msg,
            b'F' => e.file = msg,
            b'L' => e.line = msg,
            b'R' => e.routine = msg,
            _ => {}
        }
    }
    Error::Pq(Box::new(e))
}

fn parse_statement_row_describe(
    r: &mut ReadBuf<'_>,
) -> Result<(Vec<String>, Vec<FieldDesc>), Error> {
    let n = r.int16()?;
    let mut names = Vec::with_capacity(n);
    let mut typs = Vec::with_capacity(n);
    for _ in 0..n {
        names.push(r.string()?);
        r.next(6)?;
        let oid = r.oid()?;
        let len = i32::from(r.int16()? as u16 as i16);
        let modifier = r.int32()?;
        r.next(2)?;
        typs.push(FieldDesc { oid, len, modifier });
    }
    Ok((names, typs))
}

fn parse_portal_row_describe(r: &mut ReadBuf<'_>) -> Result<RowsHeader, Error> {
    let n = r.int16()?;
    let mut names = Vec::with_capacity(n);
    let mut typs = Vec::with_capacity(n);
    let mut fmts = Vec::with_capacity(n);
    for _ in 0..n {
        names.push(r.string()?);
        r.next(6)?;
        let oid = r.oid()?;
        let len = i32::from(r.int16()? as u16 as i16);
        let modifier = r.int32()?;
        let fmt = r.int16()?;
        typs.push(FieldDesc { oid, len, modifier });
        fmts.push(if fmt == 1 {
            Format::Binary
        } else {
            Format::Text
        });
    }
    Ok(RowsHeader {
        col_names: Some(names),
        col_typs: typs,
        col_fmts: fmts,
    })
}

/// `decideColumnFormats`: binary for the five types lib/pq decodes in binary, text otherwise,
/// and the Bind message's format list to match.
fn decide_column_formats(typs: &[FieldDesc]) -> (Vec<Format>, Vec<u8>) {
    if typs.is_empty() {
        return (Vec::new(), COL_FMT_ALL_TEXT.to_vec());
    }
    let mut fmts = Vec::with_capacity(typs.len());
    let mut all_binary = true;
    let mut all_text = true;
    for t in typs {
        match t.oid {
            encode::T_BYTEA | encode::T_INT8 | encode::T_INT4 | encode::T_INT2 | encode::T_UUID => {
                fmts.push(Format::Binary);
                all_text = false;
            }
            _ => {
                fmts.push(Format::Text);
                all_binary = false;
            }
        }
    }
    if all_binary {
        (fmts, COL_FMT_ALL_BINARY.to_vec())
    } else if all_text {
        (fmts, COL_FMT_ALL_TEXT.to_vec())
    } else {
        let mut data = Vec::with_capacity(2 + fmts.len() * 2);
        data.extend_from_slice(&(fmts.len() as u16).to_be_bytes());
        for f in &fmts {
            data.extend_from_slice(&(if *f == Format::Binary { 1u16 } else { 0 }).to_be_bytes());
        }
        (fmts, data)
    }
}

/// The command tag of a `CommandComplete`: the rows it affected, and the command. `INSERT`'s
/// tag carries an oid before the count; a tag without a count affected no rows.
fn parse_command_tag(tag: &str) -> Result<(ExecResult, String), Error> {
    let mut affected: Option<&str> = None;
    let mut command = tag;
    for prefix in ["SELECT ", "UPDATE ", "DELETE ", "FETCH ", "MOVE ", "COPY "] {
        if let Some(rest) = tag.strip_prefix(prefix) {
            affected = Some(rest);
            command = &prefix[..prefix.len() - 1];
            break;
        }
    }
    if affected.is_none() && tag.starts_with("INSERT ") {
        let parts: Vec<&str> = tag.split(' ').collect();
        if parts.len() != 3 {
            return Err(Error::msg(format!(
                "pq: unexpected INSERT command tag {tag}"
            )));
        }
        affected = Some(parts[2]);
        command = "INSERT";
    }
    let Some(affected) = affected else {
        return Ok((ExecResult::RowsAffected(0), command.to_owned()));
    };
    match affected.parse::<i64>() {
        Ok(n) => Ok((ExecResult::RowsAffected(n), command.to_owned())),
        Err(_) => Err(Error::msg(format!(
            "pq: could not parse commandTag: strconv.ParseInt: parsing {affected:?}: invalid syntax"
        ))),
    }
}

/// `*pq.stmt`: a prepared statement, named or unnamed.
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    name: String,
    header: RowsHeader,
    col_fmt_data: Vec<u8>,
    param_typs: Vec<u32>,
    closed: bool,
}

/// `*pq.rows`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rows {
    header: RowsHeader,
    done: bool,
    result: Option<ExecResult>,
    tag: String,
    next: Option<RowsHeader>,
}

impl Conn {
    /// `Open(dsn)`: dial, negotiate, authenticate, and wait for the first `ReadyForQuery`.
    pub async fn connect(dsn: &str) -> Result<Conn, Error> {
        let cfg = Config::parse(dsn)?;
        let open = Self::open(&cfg);
        if cfg.connect_timeout > 0 {
            match tokio::time::timeout(Duration::from_secs(cfg.connect_timeout), open).await {
                Ok(result) => result,
                Err(_) => Err(Error::Io {
                    message: format!("dial tcp {}:{}: i/o timeout", cfg.host, cfg.port),
                    eof: false,
                }),
            }
        } else {
            open.await
        }
    }

    async fn open(cfg: &Config) -> Result<Conn, Error> {
        let stream = TcpStream::connect((cfg.host.as_str(), cfg.port))
            .await
            .map_err(|e| Error::Io {
                message: format!("dial tcp {}:{}: {e}", cfg.host, cfg.port),
                eof: false,
            })?;
        let _ = stream.set_nodelay(true);
        let mut conn = Conn {
            stream: BufReader::new(stream),
            err: None,
            in_progress: false,
            txn_status: TXN_IDLE,
            namei: 0,
            saved: None,
            binary_parameters: cfg.binary_parameters,
        };
        conn.startup(cfg).await?;
        Ok(conn)
    }

    /// `startup`: the startup packet, then authentication, parameters and the backend key.
    async fn startup(&mut self, cfg: &Config) -> Result<(), Error> {
        let mut w = WriteBuf::new(0);
        w.int32(PROTOCOL_VERSION_30);
        if !cfg.user.is_empty() {
            w.string("user");
            w.string(&cfg.user);
        }
        if !cfg.database.is_empty() {
            w.string("database");
            w.string(&cfg.database);
        }
        if !cfg.options.is_empty() {
            w.string("options");
            w.string(&cfg.options);
        }
        if !cfg.application_name.is_empty() {
            w.string("application_name");
            w.string(&cfg.application_name);
        }
        w.string("client_encoding");
        w.string("UTF8");
        w.string("datestyle");
        w.string("ISO, MDY");
        for (k, v) in &cfg.runtime {
            w.string(k);
            w.string(v);
        }
        w.string("");
        let packet = w.wrap();
        self.write(&packet[1..]).await?;

        loop {
            let (t, body) = self.recv().await?;
            let mut r = ReadBuf(&body);
            match t {
                BACKEND_KEY_DATA | PARAMETER_STATUS | NEGOTIATE_PROTOCOL_VERSION => {}
                AUTHENTICATION_REQUEST => self.auth(&mut r, cfg).await?,
                READY_FOR_QUERY => {
                    self.txn_status = r.byte()?;
                    return Ok(());
                }
                other => {
                    return Err(Error::msg(format!(
                        "pq: unknown response for startup: {}",
                        response_code_q(other)
                    )));
                }
            }
        }
    }

    /// `auth`: cleartext, MD5 and SCRAM-SHA-256.
    async fn auth(&mut self, r: &mut ReadBuf<'_>, cfg: &Config) -> Result<(), Error> {
        match r.int32()? {
            0 => Ok(()),
            3 => {
                let mut w = WriteBuf::new(PASSWORD_MESSAGE);
                w.string(&cfg.password);
                self.send(w).await
            }
            5 => {
                use md5::Digest as _;
                let salt = r.next(4)?.to_vec();
                let hex = |b: &[u8]| -> String {
                    md5::Md5::digest(b)
                        .iter()
                        .map(|x| format!("{x:02x}"))
                        .collect()
                };
                let inner = hex(format!("{}{}", cfg.password, cfg.user).as_bytes());
                let mut outer_in = inner.into_bytes();
                outer_in.extend_from_slice(&salt);
                let mut w = WriteBuf::new(PASSWORD_MESSAGE);
                w.string(&format!("md5{}", hex(&outer_in)));
                self.send(w).await
            }
            10 => {
                let mut mechanisms = Vec::new();
                while let Ok(m) = r.string() {
                    if m.is_empty() {
                        break;
                    }
                    mechanisms.push(m);
                }
                if !mechanisms.iter().any(|m| m == "SCRAM-SHA-256") {
                    return Err(Error::msg(format!(
                        "pq: unsupported SASL mechanism(s): {}",
                        mechanisms.join(", ")
                    )));
                }
                let mut sc = crate::scram::Scram::new(&cfg.password);
                let first = sc.first();
                let mut w = WriteBuf::new(PASSWORD_MESSAGE);
                w.string("SCRAM-SHA-256");
                w.int32(first.len() as i32);
                w.bytes(&first);
                self.send(w).await?;

                let (t, body) = self.recv().await?;
                let mut r = ReadBuf(&body);
                if t != AUTHENTICATION_REQUEST {
                    return Err(Error::msg(format!(
                        "pq: unexpected password response: {}",
                        response_code_q(t)
                    )));
                }
                if r.int32()? != 11 {
                    return Err(Error::msg(format!(
                        "pq: unexpected authentication response: {}",
                        response_code_q(t)
                    )));
                }
                let out = sc.step(r.0)?;
                let mut w = WriteBuf::new(PASSWORD_MESSAGE);
                w.bytes(&out);
                self.send(w).await?;

                let (t, body) = self.recv().await?;
                let mut r = ReadBuf(&body);
                if t != AUTHENTICATION_REQUEST {
                    return Err(Error::msg(format!(
                        "pq: unexpected password response: {}",
                        response_code_q(t)
                    )));
                }
                if r.int32()? != 12 {
                    return Err(Error::msg(format!(
                        "pq: unexpected authentication response: {}",
                        response_code_q(t)
                    )));
                }
                sc.finish(r.0)
            }
            code => Err(Error::msg(format!(
                "pq: unknown authentication response: {code}"
            ))),
        }
    }

    // ---- transport -------------------------------------------------------------------------

    /// `send`: a write that sent nothing is a `safeRetryError`.
    async fn send(&mut self, w: WriteBuf) -> Result<(), Error> {
        let bytes = w.wrap();
        self.write(&bytes).await
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let mut written = 0;
        while written < bytes.len() {
            match self.stream.get_mut().write(&bytes[written..]).await {
                Ok(0) | Err(_) if written == 0 => {
                    return Err(Error::SafeRetry("write: broken pipe".into()));
                }
                Ok(0) => {
                    return Err(Error::Io {
                        message: "write: broken pipe".into(),
                        eof: false,
                    });
                }
                Ok(n) => written += n,
                Err(e) => return Err(Error::io(&e)),
            }
        }
        Ok(())
    }

    /// `saveMessage`.
    fn save_message(&mut self, t: u8, body: Vec<u8>) -> Result<(), Error> {
        if self.saved.is_some() {
            self.set_err(Error::BadConn);
            return Err(Error::msg(format!(
                "unexpected saveMessageType {}",
                self.saved.as_ref().map_or(0, |(t, _)| *t)
            )));
        }
        self.saved = Some((t, body));
        Ok(())
    }

    /// `recvMessage`.
    async fn recv_message(&mut self) -> Result<(u8, Vec<u8>), Error> {
        if let Some(saved) = self.saved.take() {
            return Ok(saved);
        }
        let mut head = [0u8; 5];
        self.stream
            .read_exact(&mut head)
            .await
            .map_err(|e| Error::io(&e))?;
        let t = head[0];
        let n =
            (u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize).saturating_sub(4);
        if t == READY_FOR_QUERY {
            self.in_progress = false;
        }
        if t == ERROR_RESPONSE && !(4..=MAX_ERRLEN).contains(&n) {
            let mut msg = Vec::new();
            let _ = self.stream.read_until(0, &mut msg).await;
            let msg = String::from_utf8_lossy(&msg);
            return Err(Error::msg(format!(
                "pq: server error: {}{}",
                String::from_utf8_lossy(&head[1..]),
                msg.trim_end_matches('\0')
            )));
        }
        let mut body = vec![0u8; n];
        self.stream
            .read_exact(&mut body)
            .await
            .map_err(|e| Error::io(&e))?;
        Ok((t, body))
    }

    /// `recv`: for the startup sequence; an `ErrorResponse` is the error.
    async fn recv(&mut self) -> Result<(u8, Vec<u8>), Error> {
        loop {
            let (t, body) = self.recv_message().await?;
            match t {
                ERROR_RESPONSE => return Err(parse_error(&body, "")),
                NOTICE_RESPONSE | NOTIFICATION_RESPONSE => {}
                _ => return Ok((t, body)),
            }
        }
    }

    /// `recv1Buf`: every asynchronous message skipped.
    async fn recv1(&mut self) -> Result<(u8, Vec<u8>), Error> {
        loop {
            let (t, body) = self.recv_message().await?;
            match t {
                NOTIFICATION_RESPONSE | NOTICE_RESPONSE | PARAMETER_STATUS => {}
                _ => return Ok((t, body)),
            }
        }
    }

    // ---- the error state -------------------------------------------------------------------

    /// `syncErr.set`: only the first error sticks.
    fn set_err(&mut self, err: Error) {
        if self.err.is_none() {
            self.err = Some(err);
        }
    }

    /// `syncErr.get`: `ErrBadConn` once anything was set.
    fn err_get(&self) -> Result<(), Error> {
        if self.err.is_some() {
            return Err(Error::BadConn);
        }
        Ok(())
    }

    /// Whether the connection has been marked bad.
    pub fn is_bad(&self) -> bool {
        self.err.is_some()
    }

    /// `handleError`.
    fn handle_error(&mut self, reported: Error, query: Option<&str>) -> Error {
        let reported = match reported {
            Error::Io { eof: true, .. } => Error::BadConn,
            Error::Io {
                message,
                eof: false,
            } => {
                self.set_err(Error::BadConn);
                Error::Io {
                    message,
                    eof: false,
                }
            }
            Error::SafeRetry(_) => {
                self.set_err(Error::BadConn);
                Error::BadConn
            }
            Error::Pq(mut e) => {
                if let Some(q) = query.filter(|q| !q.is_empty()) {
                    e.query = q.to_owned();
                }
                if e.fatal() {
                    Error::BadConn
                } else {
                    Error::Pq(e)
                }
            }
            Error::Eof => Error::BadConn,
            other => other,
        };
        if reported == Error::BadConn {
            self.set_err(Error::BadConn);
        }
        reported
    }

    fn handle(&mut self, result: Result<(), Error>, query: Option<&str>) -> Result<(), Error> {
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(self.handle_error(e, query)),
        }
    }

    // ---- small protocol steps --------------------------------------------------------------

    /// `parseComplete`: [`parse_command_tag`], and a tag it cannot read marks the connection bad.
    fn parse_complete(&mut self, tag: &str) -> Result<(ExecResult, String), Error> {
        let parsed = parse_command_tag(tag);
        if parsed.is_err() {
            self.set_err(Error::BadConn);
        }
        parsed
    }

    fn process_ready_for_query(&mut self, body: &[u8]) {
        self.txn_status = body.first().copied().unwrap_or(TXN_IDLE);
    }

    /// `readReadyForQuery`.
    async fn read_ready_for_query(&mut self) -> Result<(), Error> {
        let (t, body) = self.recv1().await?;
        match t {
            READY_FOR_QUERY => {
                self.process_ready_for_query(&body);
                Ok(())
            }
            ERROR_RESPONSE => {
                let err = parse_error(&body, "");
                self.set_err(Error::BadConn);
                Err(err)
            }
            other => {
                self.set_err(Error::BadConn);
                Err(Error::msg(format!(
                    "pq: unexpected message {}; expected ReadyForQuery",
                    response_code_q(other)
                )))
            }
        }
    }

    /// `readParseResponse`.
    async fn read_parse_response(&mut self) -> Result<(), Error> {
        let (t, body) = self.recv1().await?;
        match t {
            PARSE_COMPLETE => Ok(()),
            ERROR_RESPONSE => {
                let err = parse_error(&body, "");
                let _ = self.read_ready_for_query().await;
                Err(err)
            }
            other => {
                self.set_err(Error::BadConn);
                Err(Error::msg(format!(
                    "pq: unexpected Parse response {}",
                    response_code_q(other)
                )))
            }
        }
    }

    /// `readStatementDescribeResponse`.
    async fn read_statement_describe_response(
        &mut self,
    ) -> Result<(Vec<u32>, Option<Vec<String>>, Vec<FieldDesc>), Error> {
        let mut param_typs = Vec::new();
        loop {
            let (t, body) = self.recv1().await?;
            let mut r = ReadBuf(&body);
            match t {
                PARAMETER_DESCRIPTION => {
                    let n = r.int16()?;
                    param_typs = (0..n).map(|_| r.oid()).collect::<Result<_, _>>()?;
                }
                NO_DATA => return Ok((param_typs, None, Vec::new())),
                ROW_DESCRIPTION => {
                    let (names, typs) = parse_statement_row_describe(&mut r)?;
                    return Ok((param_typs, Some(names), typs));
                }
                ERROR_RESPONSE => {
                    let err = parse_error(&body, "");
                    let _ = self.read_ready_for_query().await;
                    return Err(err);
                }
                other => {
                    self.set_err(Error::BadConn);
                    return Err(Error::msg(format!(
                        "pq: unexpected Describe statement response {}",
                        response_code_q(other)
                    )));
                }
            }
        }
    }

    /// `readPortalDescribeResponse`.
    async fn read_portal_describe_response(&mut self) -> Result<RowsHeader, Error> {
        let (t, body) = self.recv1().await?;
        let mut r = ReadBuf(&body);
        match t {
            ROW_DESCRIPTION => parse_portal_row_describe(&mut r),
            NO_DATA => Ok(RowsHeader::default()),
            ERROR_RESPONSE => {
                let err = parse_error(&body, "");
                let _ = self.read_ready_for_query().await;
                Err(err)
            }
            other => {
                self.set_err(Error::BadConn);
                Err(Error::msg(format!(
                    "pq: unexpected Describe response {}",
                    response_code_q(other)
                )))
            }
        }
    }

    /// `readBindResponse`.
    async fn read_bind_response(&mut self) -> Result<(), Error> {
        let (t, body) = self.recv1().await?;
        match t {
            BIND_COMPLETE => Ok(()),
            ERROR_RESPONSE => {
                let err = parse_error(&body, "");
                let _ = self.read_ready_for_query().await;
                Err(err)
            }
            other => {
                self.set_err(Error::BadConn);
                Err(Error::msg(format!(
                    "pq: unexpected Bind response {}",
                    response_code_q(other)
                )))
            }
        }
    }

    /// `postExecuteWorkaround`: look at one message; an error ends the query now, anything else
    /// is saved for `Rows.Next`.
    async fn post_execute_workaround(&mut self) -> Result<(), Error> {
        let (t, body) = self.recv1().await?;
        match t {
            ERROR_RESPONSE => {
                let err = parse_error(&body, "");
                let _ = self.read_ready_for_query().await;
                Err(err)
            }
            COMMAND_COMPLETE | DATA_ROW | EMPTY_QUERY_RESPONSE => self.save_message(t, body),
            other => {
                self.set_err(Error::BadConn);
                Err(Error::msg(format!(
                    "pq: unexpected message during extended query execution: {}",
                    response_code_q(other)
                )))
            }
        }
    }

    /// `readExecuteResponse`, for `Exec`, which ignores any rows.
    async fn read_execute_response(&mut self, protocol_state: &str) -> Result<ExecResult, Error> {
        let mut res: Option<ExecResult> = None;
        let mut res_err: Option<Error> = None;
        loop {
            let (t, body) = self.recv1().await?;
            match t {
                COMMAND_COMPLETE => {
                    if let Some(e) = &res_err {
                        self.set_err(Error::BadConn);
                        return Err(Error::msg(format!(
                            "pq: unexpected CommandComplete after error {e}"
                        )));
                    }
                    let tag = ReadBuf(&body).string()?;
                    res = Some(self.parse_complete(&tag)?.0);
                }
                READY_FOR_QUERY => {
                    self.process_ready_for_query(&body);
                    if res.is_none() && res_err.is_none() {
                        res_err = Some(Error::msg(UNEXPECTED_READY));
                    }
                    return match res_err {
                        Some(e) => Err(e),
                        None => res.ok_or_else(|| Error::msg(UNEXPECTED_READY)),
                    };
                }
                ERROR_RESPONSE => res_err = Some(parse_error(&body, "")),
                ROW_DESCRIPTION | DATA_ROW | EMPTY_QUERY_RESPONSE => {
                    if let Some(e) = &res_err {
                        self.set_err(Error::BadConn);
                        return Err(Error::msg(format!(
                            "pq: unexpected {} after error {e}",
                            response_code_q(t)
                        )));
                    }
                    if t == EMPTY_QUERY_RESPONSE {
                        res = Some(ExecResult::Empty);
                    }
                }
                other => {
                    self.set_err(Error::BadConn);
                    return Err(Error::msg(format!(
                        "pq: unknown {protocol_state} response: {}",
                        response_code_q(other)
                    )));
                }
            }
        }
    }

    // ---- simple protocol -------------------------------------------------------------------

    /// `simpleExec`: the command tag too.
    async fn simple_exec(&mut self, q: &str) -> Result<(ExecResult, String), Error> {
        let mut w = WriteBuf::new(QUERY);
        w.string(q);
        self.send(w).await?;
        let mut res: Option<ExecResult> = None;
        let mut tag = String::new();
        let mut res_err: Option<Error> = None;
        loop {
            let (t, body) = self.recv1().await?;
            match t {
                COMMAND_COMPLETE => {
                    let text = ReadBuf(&body).string()?;
                    let (r, c) = self.parse_complete(&text)?;
                    res = Some(r);
                    tag = c;
                }
                READY_FOR_QUERY => {
                    self.process_ready_for_query(&body);
                    if res.is_none() && res_err.is_none() {
                        res_err = Some(Error::msg(UNEXPECTED_READY));
                    }
                    return match res_err {
                        Some(e) => Err(e),
                        None => Ok((res.unwrap_or(ExecResult::Empty), tag)),
                    };
                }
                ERROR_RESPONSE => res_err = Some(parse_error(&body, q)),
                EMPTY_QUERY_RESPONSE => res = Some(ExecResult::Empty),
                ROW_DESCRIPTION | DATA_ROW => {}
                other => {
                    self.set_err(Error::BadConn);
                    return Err(Error::msg(format!(
                        "pq: unknown response for simple query: {}",
                        response_code_q(other)
                    )));
                }
            }
        }
    }

    /// `simpleQuery`.
    async fn simple_query(&mut self, q: &str) -> Result<Rows, Error> {
        let mut w = WriteBuf::new(QUERY);
        w.string(q);
        if let Err(e) = self.send(w).await {
            return Err(self.handle_error(e, Some(q)));
        }
        let mut res: Option<Rows> = None;
        let mut res_err: Option<Error> = None;
        loop {
            let (t, body) = match self.recv1().await {
                Ok(m) => m,
                Err(e) => return Err(self.handle_error(e, Some(q))),
            };
            match t {
                COMMAND_COMPLETE | EMPTY_QUERY_RESPONSE => {
                    if res_err.is_some() {
                        self.set_err(Error::BadConn);
                        return Err(Error::msg(format!(
                            "pq: unexpected message {} in simple query execution",
                            response_code_q(t)
                        )));
                    }
                    let rows = res.get_or_insert_with(Rows::default);
                    if t == COMMAND_COMPLETE {
                        let text = ReadBuf(&body).string()?;
                        let (r, tag) = match self.parse_complete(&text) {
                            Ok(v) => v,
                            Err(e) => return Err(self.handle_error(e, Some(q))),
                        };
                        let rows = res.get_or_insert_with(Rows::default);
                        rows.result = Some(r);
                        rows.tag = tag;
                        if rows.header.col_names.is_some() {
                            let rows = res.take().unwrap_or_default();
                            return match res_err {
                                None => Ok(rows),
                                Some(e) => Err(self.handle_error(e, Some(q))),
                            };
                        }
                        res.get_or_insert_with(Rows::default).done = true;
                    } else {
                        rows.done = true;
                    }
                }
                READY_FOR_QUERY => {
                    self.process_ready_for_query(&body);
                    return match res_err {
                        Some(e) => Err(self.handle_error(e, Some(q))),
                        None => Ok(res.unwrap_or(Rows {
                            done: true,
                            ..Rows::default()
                        })),
                    };
                }
                ERROR_RESPONSE => {
                    res = None;
                    res_err = Some(parse_error(&body, q));
                }
                DATA_ROW => {
                    let Some(rows) = res else {
                        self.set_err(Error::BadConn);
                        return Err(Error::msg(
                            "pq: unexpected DataRow in simple query execution",
                        ));
                    };
                    self.save_message(t, body)?;
                    return Ok(rows);
                }
                ROW_DESCRIPTION => {
                    let mut r = ReadBuf(&body);
                    res = Some(Rows {
                        header: parse_portal_row_describe(&mut r)?,
                        ..Rows::default()
                    });
                }
                other => {
                    self.set_err(Error::BadConn);
                    return Err(Error::msg(format!(
                        "pq: unknown response for simple query: {}",
                        response_code_q(other)
                    )));
                }
            }
        }
    }

    // ---- extended protocol -----------------------------------------------------------------

    /// `prepareTo`: Parse, Describe the statement, Sync.
    async fn prepare_to(&mut self, q: &str, name: &str) -> Result<Stmt, Error> {
        let mut w = WriteBuf::new(PARSE);
        w.string(name);
        w.string(q);
        w.int16(0);
        w.next(DESCRIBE);
        w.byte(SYNC);
        w.string(name);
        w.next(SYNC);
        self.send(w).await?;
        self.read_parse_response().await?;
        let (param_typs, col_names, col_typs) = self.read_statement_describe_response().await?;
        let (col_fmts, col_fmt_data) = decide_column_formats(&col_typs);
        self.read_ready_for_query().await?;
        Ok(Stmt {
            name: name.to_owned(),
            header: RowsHeader {
                col_names,
                col_typs,
                col_fmts,
            },
            col_fmt_data,
            param_typs,
            closed: false,
        })
    }

    /// `sendBinaryParameters`.
    fn send_binary_parameters(w: &mut WriteBuf, args: &[NamedValue]) -> Result<(), Error> {
        let any_bytes = args.iter().any(|a| matches!(a.value, Value::Bytes(_)));
        if any_bytes {
            w.int16(args.len() as i32);
            for a in args {
                w.int16(i32::from(matches!(a.value, Value::Bytes(_))));
            }
        } else {
            w.int16(0);
        }
        w.int16(args.len() as i32);
        for a in args {
            match &a.value {
                Value::Null => w.int32(-1),
                Value::Bytes(b) if b.is_empty() => w.int32(-1),
                v => match encode::binary_encode(v)? {
                    Some(datum) => {
                        w.int32(datum.len() as i32);
                        w.bytes(&datum);
                    }
                    None => w.int32(-1),
                },
            }
        }
        Ok(())
    }

    /// `sendBinaryModeQuery`: Parse, Bind, Describe the portal, Execute, Sync in one write.
    async fn send_binary_mode_query(&mut self, q: &str, args: &[NamedValue]) -> Result<(), Error> {
        if args.len() >= 65536 {
            return Err(Error::msg(format!(
                "pq: got {} parameters but PostgreSQL only supports 65535 parameters",
                args.len()
            )));
        }
        let mut w = WriteBuf::new(PARSE);
        w.byte(0);
        w.string(q);
        w.int16(0);
        w.next(BIND);
        w.int16(0);
        Self::send_binary_parameters(&mut w, args)?;
        w.bytes(&COL_FMT_ALL_TEXT);
        w.next(DESCRIBE);
        w.byte(b'P');
        w.byte(0);
        w.next(EXECUTE);
        w.byte(0);
        w.int32(0);
        w.next(SYNC);
        self.send(w).await
    }

    /// `(*conn).query`, reached through `QueryContext`.
    pub async fn query(&mut self, q: &str, args: &[NamedValue]) -> Result<Rows, Error> {
        self.err_get()?;
        if self.in_progress {
            return Err(Error::msg(QUERY_IN_PROGRESS));
        }
        self.in_progress = true;
        if args.is_empty() {
            return self.simple_query(q).await;
        }
        if self.binary_parameters {
            let sent = self.send_binary_mode_query(q, args).await;
            self.handle(sent, Some(q))?;
            let parsed = self.read_parse_response().await;
            self.handle(parsed, Some(q))?;
            let bound = self.read_bind_response().await;
            self.handle(bound, Some(q))?;
            let header = match self.read_portal_describe_response().await {
                Ok(h) => h,
                Err(e) => return Err(self.handle_error(e, Some(q))),
            };
            let worked = self.post_execute_workaround().await;
            self.handle(worked, Some(q))?;
            return Ok(Rows {
                header,
                ..Rows::default()
            });
        }
        let st = match self.prepare_to(q, "").await {
            Ok(st) => st,
            Err(e) => return Err(self.handle_error(e, Some(q))),
        };
        let executed = st.exec_raw(self, args).await;
        self.handle(executed, Some(q))?;
        Ok(Rows {
            header: st.header,
            ..Rows::default()
        })
    }

    /// `(*conn).Exec`, reached through `ExecContext`.
    pub async fn exec(&mut self, q: &str, args: &[Value]) -> Result<ExecResult, Error> {
        self.err_get()?;
        if self.in_progress {
            return Err(Error::msg(QUERY_IN_PROGRESS));
        }
        self.in_progress = true;
        if args.is_empty() {
            return match self.simple_exec(q).await {
                Ok((r, _)) => Ok(r),
                Err(e) => Err(self.handle_error(e, Some(q))),
            };
        }
        let named: Vec<NamedValue> = args
            .iter()
            .enumerate()
            .map(|(i, v)| NamedValue::positional(i, v.clone()))
            .collect();
        if self.binary_parameters {
            let sent = self.send_binary_mode_query(q, &named).await;
            self.handle(sent, Some(q))?;
            let parsed = self.read_parse_response().await;
            self.handle(parsed, Some(q))?;
            let bound = self.read_bind_response().await;
            self.handle(bound, Some(q))?;
            if let Err(e) = self.read_portal_describe_response().await {
                return Err(self.handle_error(e, Some(q)));
            }
            let worked = self.post_execute_workaround().await;
            self.handle(worked, Some(q))?;
            return match self.read_execute_response("Execute").await {
                Ok(r) => Ok(r),
                Err(e) => Err(self.handle_error(e, Some(q))),
            };
        }
        let st = match self.prepare_to(q, "").await {
            Ok(st) => st,
            Err(e) => return Err(self.handle_error(e, Some(q))),
        };
        st.exec(self, &named).await
    }

    /// `(*conn).Prepare`: a named statement, `"1"`, `"2"`, … per connection.
    pub async fn prepare(&mut self, q: &str) -> Result<Stmt, Error> {
        self.err_get()?;
        self.namei += 1;
        let name = self.namei.to_string();
        match self.prepare_to(q, &name).await {
            Ok(st) => Ok(st),
            Err(e) => Err(self.handle_error(e, Some(q))),
        }
    }

    /// `Ping`: `;` through the simple protocol; any failure is `ErrBadConn`.
    pub async fn ping(&mut self) -> Result<(), Error> {
        match self.simple_query(";").await {
            Err(_) => Err(Error::BadConn),
            Ok(mut rows) => {
                let _ = rows.close(self).await;
                Ok(())
            }
        }
    }

    fn is_in_transaction(&self) -> bool {
        self.txn_status == TXN_IN_TRANSACTION || self.txn_status == TXN_FAILED
    }

    /// `checkIsInTransaction`.
    fn check_is_in_transaction(&mut self, intxn: bool) -> Result<(), Error> {
        if self.is_in_transaction() != intxn {
            self.set_err(Error::BadConn);
            return Err(Error::msg(format!(
                "pq: unexpected transaction status {}",
                txn_status_string(self.txn_status)
            )));
        }
        Ok(())
    }

    /// `BeginTx`: `sql.IsolationLevel` 0, 1, 2, 4 and 6, and `READ ONLY` / `READ WRITE`.
    pub async fn begin_tx(&mut self, isolation: i64, read_only: bool) -> Result<(), Error> {
        let mut mode = match isolation {
            0 => String::new(),
            1 => " ISOLATION LEVEL READ UNCOMMITTED".into(),
            2 => " ISOLATION LEVEL READ COMMITTED".into(),
            4 => " ISOLATION LEVEL REPEATABLE READ".into(),
            6 => " ISOLATION LEVEL SERIALIZABLE".into(),
            other => {
                return Err(Error::msg(format!(
                    "pq: isolation level not supported: {other}"
                )));
            }
        };
        mode.push_str(if read_only {
            " READ ONLY"
        } else {
            " READ WRITE"
        });
        self.begin(&mode).await
    }

    /// `begin`.
    async fn begin(&mut self, mode: &str) -> Result<(), Error> {
        self.err_get()?;
        self.check_is_in_transaction(false)?;
        let (_, tag) = match self.simple_exec(&format!("BEGIN{mode}")).await {
            Ok(v) => v,
            Err(e) => return Err(self.handle_error(e, None)),
        };
        if tag != "BEGIN" {
            self.set_err(Error::BadConn);
            return Err(Error::msg(format!("unexpected command tag {tag}")));
        }
        if self.txn_status != TXN_IN_TRANSACTION {
            self.set_err(Error::BadConn);
            return Err(Error::msg(format!(
                "unexpected transaction status {}",
                txn_status_string(self.txn_status)
            )));
        }
        Ok(())
    }

    /// `Commit`: a failed transaction is rolled back and reported as `ErrInFailedTransaction`.
    pub async fn commit(&mut self) -> Result<(), Error> {
        self.err_get()?;
        self.check_is_in_transaction(true)?;
        if self.txn_status == TXN_FAILED {
            self.rollback_inner().await?;
            return Err(Error::msg(IN_FAILED_TRANSACTION));
        }
        let (_, tag) = match self.simple_exec("COMMIT").await {
            Ok(v) => v,
            Err(e) => {
                if self.is_in_transaction() {
                    self.set_err(Error::BadConn);
                }
                return Err(self.handle_error(e, None));
            }
        };
        if tag != "COMMIT" {
            self.set_err(Error::BadConn);
            return Err(Error::msg(format!("unexpected command tag {tag}")));
        }
        self.check_is_in_transaction(false)
    }

    /// `Rollback`.
    pub async fn rollback(&mut self) -> Result<(), Error> {
        self.err_get()?;
        match self.rollback_inner().await {
            Ok(()) => Ok(()),
            Err(e) => Err(self.handle_error(e, None)),
        }
    }

    /// `rollback`.
    async fn rollback_inner(&mut self) -> Result<(), Error> {
        self.check_is_in_transaction(true)?;
        let (_, tag) = match self.simple_exec("ROLLBACK").await {
            Ok(v) => v,
            Err(e) => {
                if self.is_in_transaction() {
                    self.set_err(Error::BadConn);
                }
                return Err(e);
            }
        };
        if tag != "ROLLBACK" {
            return Err(Error::msg(format!("unexpected command tag {tag}")));
        }
        self.check_is_in_transaction(false)
    }

    /// `Close`: `Terminate`, then the socket.
    pub async fn close(mut self) -> Result<(), Error> {
        let result = self.write(&[TERMINATE, 0, 0, 0, 4]).await;
        let _ = self.stream.get_mut().shutdown().await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(self.handle_error(e, None)),
        }
    }
}

impl Stmt {
    /// `NumInput`.
    pub fn num_input(&self) -> i64 {
        self.param_typs.len() as i64
    }

    /// The columns a query through this statement will have.
    pub fn header(&self) -> &RowsHeader {
        &self.header
    }

    /// `stmt.exec`: Bind (text parameters, or binary under `binary_parameters`), Execute, Sync.
    async fn exec_raw(&self, cn: &mut Conn, v: &[NamedValue]) -> Result<(), Error> {
        if v.len() >= 65536 {
            return Err(Error::msg(format!(
                "pq: got {} parameters but PostgreSQL only supports 65535 parameters",
                v.len()
            )));
        }
        if v.len() != self.param_typs.len() {
            return Err(Error::msg(format!(
                "pq: got {} parameters but the statement requires {}",
                v.len(),
                self.param_typs.len()
            )));
        }
        let mut w = WriteBuf::new(BIND);
        w.byte(0);
        w.string(&self.name);
        if cn.binary_parameters {
            Conn::send_binary_parameters(&mut w, v)?;
        } else {
            w.int16(0);
            w.int16(v.len() as i32);
            for (i, x) in v.iter().enumerate() {
                if x.value == Value::Null {
                    w.int32(-1);
                    continue;
                }
                match encode::encode(&x.value, self.param_typs[i])? {
                    None => w.int32(-1),
                    Some(b) => {
                        w.int32(b.len() as i32);
                        w.bytes(&b);
                    }
                }
            }
        }
        w.bytes(&self.col_fmt_data);
        w.next(EXECUTE);
        w.byte(0);
        w.int32(0);
        w.next(SYNC);
        cn.send(w).await?;
        cn.read_bind_response().await?;
        cn.post_execute_workaround().await
    }

    /// `stmt.query`.
    pub async fn query(&self, cn: &mut Conn, v: &[NamedValue]) -> Result<Rows, Error> {
        cn.err_get()?;
        match self.exec_raw(cn, v).await {
            Ok(()) => Ok(Rows {
                header: self.header.clone(),
                ..Rows::default()
            }),
            Err(e) => Err(cn.handle_error(e, None)),
        }
    }

    /// `stmt.ExecContext`.
    pub async fn exec(&self, cn: &mut Conn, v: &[NamedValue]) -> Result<ExecResult, Error> {
        cn.err_get()?;
        if let Err(e) = self.exec_raw(cn, v).await {
            return Err(cn.handle_error(e, None));
        }
        match cn.read_execute_response("simple query").await {
            Ok(r) => Ok(r),
            Err(e) => Err(cn.handle_error(e, None)),
        }
    }

    /// `stmt.Close`: Close the statement, Sync, and expect exactly CloseComplete, ReadyForQuery.
    pub async fn close(&mut self, cn: &mut Conn) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        cn.err_get()?;
        let mut w = WriteBuf::new(CLOSE);
        w.byte(SYNC);
        w.string(&self.name);
        if let Err(e) = cn.send(w).await {
            return Err(cn.handle_error(e, None));
        }
        if let Err(e) = cn.send(WriteBuf::new(SYNC)).await {
            return Err(cn.handle_error(e, None));
        }
        let (t, _) = match cn.recv1().await {
            Ok(m) => m,
            Err(e) => return Err(cn.handle_error(e, None)),
        };
        if t != CLOSE_COMPLETE {
            cn.set_err(Error::BadConn);
            return Err(Error::msg(format!(
                "pq: unexpected close response: {}",
                response_code_q(t)
            )));
        }
        self.closed = true;
        let (t, body) = match cn.recv1().await {
            Ok(m) => m,
            Err(e) => return Err(cn.handle_error(e, None)),
        };
        if t != READY_FOR_QUERY {
            cn.set_err(Error::BadConn);
            return Err(Error::msg(format!(
                "pq: expected ready for query, but got: {}",
                response_code_q(t)
            )));
        }
        cn.process_ready_for_query(&body);
        Ok(())
    }
}

impl Rows {
    /// `Columns`: `nil` for a statement that returns none.
    pub fn columns(&self) -> Option<&[String]> {
        self.header.col_names.as_deref()
    }

    /// The column descriptions, for `ColumnTypeDatabaseTypeName` and `ColumnTypePrecisionScale`.
    pub fn column_types(&self) -> &[FieldDesc] {
        &self.header.col_typs
    }

    /// `HasNextResultSet`.
    pub fn has_next_result_set(&self) -> bool {
        self.next.is_some() && !self.done
    }

    /// `NextResultSet`.
    pub fn next_result_set(&mut self) -> Result<(), Error> {
        match self.next.take() {
            None => Err(Error::Eof),
            Some(next) => {
                self.header = next;
                Ok(())
            }
        }
    }

    /// `Next`: fill `dest` from the next `DataRow`, or `io.EOF` at the end of the result set.
    /// Only the first `min(len(dest), columns)` values are written.
    pub async fn next(&mut self, cn: &mut Conn, dest: &mut [Value]) -> Result<(), Error> {
        if self.done {
            return Err(Error::Eof);
        }
        if let Some(e) = cn.err.clone() {
            return Err(e);
        }
        let mut res_err: Option<Error> = None;
        loop {
            let (t, body) = match cn.recv1().await {
                Ok(m) => m,
                Err(e) => return Err(cn.handle_error(e, None)),
            };
            let mut r = ReadBuf(&body);
            match t {
                ERROR_RESPONSE => res_err = Some(parse_error(&body, "")),
                COMMAND_COMPLETE | EMPTY_QUERY_RESPONSE => {
                    if t == COMMAND_COMPLETE {
                        let tag = r.string()?;
                        match cn.parse_complete(&tag) {
                            Ok((res, tag)) => {
                                self.result = Some(res);
                                self.tag = tag;
                            }
                            Err(e) => return Err(cn.handle_error(e, None)),
                        }
                    }
                }
                READY_FOR_QUERY => {
                    cn.process_ready_for_query(&body);
                    self.done = true;
                    return match res_err {
                        Some(e) => Err(cn.handle_error(e, None)),
                        None => Err(Error::Eof),
                    };
                }
                DATA_ROW => {
                    let n = r.int16()?;
                    if let Some(e) = res_err {
                        cn.set_err(Error::BadConn);
                        return Err(Error::msg(format!(
                            "pq: unexpected DataRow after error {e}"
                        )));
                    }
                    let count = dest.len().min(n);
                    for (i, slot) in dest.iter_mut().take(count).enumerate() {
                        let l = r.int32()?;
                        if l == -1 {
                            *slot = Value::Null;
                            continue;
                        }
                        let bytes = r.next(l as usize)?;
                        let typ = self.header.col_typs.get(i).map_or(0, |t| t.oid);
                        let fmt = self.header.col_fmts.get(i).copied().unwrap_or(Format::Text);
                        match encode::decode(bytes, typ, fmt) {
                            Ok(v) => *slot = v,
                            Err(e) => return Err(cn.handle_error(e, None)),
                        }
                    }
                    return Ok(());
                }
                ROW_DESCRIPTION => {
                    self.next = Some(parse_portal_row_describe(&mut r)?);
                    return Err(Error::Eof);
                }
                other => {
                    return Err(Error::msg(format!(
                        "pq: unexpected message after execute: {}",
                        response_code_q(other)
                    )));
                }
            }
        }
    }

    /// `Close`: read to the end, through every remaining result set.
    pub async fn close(&mut self, cn: &mut Conn) -> Result<(), Error> {
        loop {
            match self.next(cn, &mut []).await {
                Ok(()) => {}
                Err(Error::Eof) => {
                    if self.done {
                        return Ok(());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_framed_back_to_back() {
        let mut w = WriteBuf::new(b'P');
        w.string("");
        w.string("SELECT 1");
        w.int16(0);
        w.next(b'S');
        let bytes = w.wrap();
        assert_eq!(
            bytes,
            b"P\x00\x00\x00\x10\x00SELECT 1\x00\x00\x00S\x00\x00\x00\x04".to_vec()
        );
    }

    #[test]
    fn the_column_formats_follow_the_types() {
        let t = |oid| FieldDesc {
            oid,
            len: 0,
            modifier: 0,
        };
        assert_eq!(decide_column_formats(&[]).1, COL_FMT_ALL_TEXT.to_vec());
        assert_eq!(
            decide_column_formats(&[t(encode::T_INT4), t(encode::T_BYTEA)]).1,
            COL_FMT_ALL_BINARY.to_vec()
        );
        assert_eq!(
            decide_column_formats(&[t(encode::T_TEXT)]).1,
            COL_FMT_ALL_TEXT.to_vec()
        );
        assert_eq!(
            decide_column_formats(&[t(encode::T_INT8), t(encode::T_TEXT)]).1,
            vec![0, 2, 0, 1, 0, 0]
        );
    }

    #[test]
    fn numeric_precision_and_scale_come_from_the_type_modifier() {
        let numeric = FieldDesc {
            oid: encode::T_NUMERIC,
            len: -1,
            modifier: ((10 << 16) | 2) + 4,
        };
        assert_eq!(numeric.precision_scale(), (10, 2, true));
        assert_eq!(numeric.name(), "NUMERIC");
        let text = FieldDesc {
            oid: encode::T_TEXT,
            len: -1,
            modifier: -1,
        };
        assert_eq!(text.precision_scale(), (0, 0, false));
    }

    #[test]
    fn command_tags_parse_as_lib_pq_parses_them() {
        let ok = |tag: &str| parse_command_tag(tag).unwrap();
        assert_eq!(
            ok("INSERT 0 3"),
            (ExecResult::RowsAffected(3), "INSERT".into())
        );
        assert_eq!(
            ok("SELECT 2"),
            (ExecResult::RowsAffected(2), "SELECT".into())
        );
        assert_eq!(
            ok("UPDATE 0"),
            (ExecResult::RowsAffected(0), "UPDATE".into())
        );
        assert_eq!(ok("COPY 7"), (ExecResult::RowsAffected(7), "COPY".into()));
        assert_eq!(
            ok("CREATE TABLE"),
            (ExecResult::RowsAffected(0), "CREATE TABLE".into())
        );
        assert_eq!(ok("BEGIN"), (ExecResult::RowsAffected(0), "BEGIN".into()));
        assert_eq!(
            parse_command_tag("INSERT 5").unwrap_err().to_string(),
            "pq: unexpected INSERT command tag INSERT 5"
        );
        assert_eq!(
            parse_command_tag("DELETE x").unwrap_err().to_string(),
            "pq: could not parse commandTag: strconv.ParseInt: parsing \"x\": invalid syntax"
        );
    }
}
