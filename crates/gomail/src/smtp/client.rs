//! Port of `smtp.Client` (net/smtp/smtp.go) and `PlainAuth` (net/smtp/auth.go).

use std::collections::HashMap;

use super::conn::Conn;
use super::textproto::{self, DotWriter};
use super::tls::TlsConfig;
use super::{AuthError, Error};
use crate::base64;

/// Port of `smtp.ServerInfo` (auth.go:28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The name the client was created with (`NewClient`'s `host`).
    pub name: String,
    /// Whether the connection is TLS.
    pub tls: bool,
    /// The mechanisms the server advertised in its EHLO `AUTH` line.
    pub auth: Vec<String>,
}

/// Port of `smtp.Auth` (auth.go:14).
pub trait Auth {
    /// `Start`: the mechanism name and the initial response (empty for none).
    fn start(&mut self, server: &ServerInfo) -> Result<(String, Vec<u8>), Error>;
    /// `Next`: the response to a challenge; `None` is Go's nil — nothing more to send.
    fn next(&mut self, from_server: &[u8], more: bool) -> Result<Option<Vec<u8>>, Error>;
}

/// Port of `smtp.PlainAuth` (auth.go:48).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainAuth {
    pub identity: String,
    pub username: String,
    pub password: String,
    pub host: String,
}

/// `isLocalhost` (auth.go:57).
fn is_localhost(name: &str) -> bool {
    name == "localhost" || name == "127.0.0.1" || name == "::1"
}

impl Auth for PlainAuth {
    /// Refuses an unencrypted connection unless the **server name** (not the address dialled)
    /// is literally a localhost name, then refuses a server name other than `host`.
    fn start(&mut self, server: &ServerInfo) -> Result<(String, Vec<u8>), Error> {
        if !server.tls && !is_localhost(&server.name) {
            return Err(AuthError::UnencryptedConnection.into());
        }
        if server.name != self.host {
            return Err(AuthError::WrongHostName.into());
        }
        let resp = format!("{}\0{}\0{}", self.identity, self.username, self.password);
        Ok(("PLAIN".to_owned(), resp.into_bytes()))
    }

    fn next(&mut self, _from_server: &[u8], more: bool) -> Result<Option<Vec<u8>>, Error> {
        if more {
            // We've already sent everything.
            return Err(AuthError::UnexpectedServerChallenge.into());
        }
        Ok(None)
    }
}

/// Port of `smtp.Client` (smtp.go:26).
pub struct Client {
    conn: Conn,
    tls: bool,
    server_name: String,
    /// The EHLO extensions; `None` until an EHLO succeeds, and reset by HELO.
    ext: Option<HashMap<String, String>>,
    auth: Vec<String>,
    local_name: String,
    did_hello: bool,
    hello_error: Option<Error>,
}

/// `validateLine` (smtp.go:437).
fn validate_line(line: &str) -> Result<(), Error> {
    if line.contains(['\n', '\r']) {
        return Err(Error::LineContainsCrLf);
    }
    Ok(())
}

impl Client {
    /// `NewClient` (smtp.go:56): read the 220 greeting; on failure the connection is closed.
    pub async fn new(mut conn: Conn, host: &str) -> Result<Self, Error> {
        match textproto::read_response(&mut conn, 220).await {
            Ok(r) => {
                if let Some(err) = r.err {
                    conn.close();
                    return Err(err.into());
                }
            }
            Err(e) => {
                conn.close();
                return Err(e);
            }
        }
        let tls = conn.is_tls();
        Ok(Self {
            conn,
            tls,
            server_name: host.to_owned(),
            ext: None,
            auth: Vec::new(),
            local_name: "localhost".to_owned(),
            did_hello: false,
            hello_error: None,
        })
    }

    /// `Close` (smtp.go:74).
    pub fn close(&mut self) {
        self.conn.close();
    }

    /// `hello` (smtp.go:80): EHLO, falling back to HELO; the outcome is remembered.
    async fn hello_once(&mut self) -> Result<(), Error> {
        if !self.did_hello {
            self.did_hello = true;
            if self.ehlo().await.is_err() {
                self.hello_error = self.helo().await.err();
            }
        }
        match &self.hello_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// `Hello` (smtp.go:98).
    pub async fn hello(&mut self, local_name: &str) -> Result<(), Error> {
        validate_line(local_name)?;
        if self.did_hello {
            return Err(Error::HelloAfterOtherMethods);
        }
        self.local_name = local_name.to_owned();
        self.hello_once().await
    }

    /// `cmd` (smtp.go:111): send one line, read one reply.
    async fn cmd(&mut self, expect_code: i32, line: &str) -> Result<(i32, String), Error> {
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        self.conn.write_all(&bytes).await?;
        let r = textproto::read_response(&mut self.conn, expect_code).await?;
        match r.err {
            Some(e) => Err(e.into()),
            None => Ok((r.code, r.message)),
        }
    }

    /// `helo` (smtp.go:125).
    async fn helo(&mut self) -> Result<(), Error> {
        self.ext = None;
        let line = format!("HELO {}", self.local_name);
        self.cmd(250, &line).await.map(|_| ())
    }

    /// `ehlo` (smtp.go:133): the reply's lines after the first are `KEYWORD params`.
    async fn ehlo(&mut self) -> Result<(), Error> {
        let line = format!("EHLO {}", self.local_name);
        let (_, msg) = self.cmd(250, &line).await?;
        let mut ext = HashMap::new();
        let lines: Vec<&str> = msg.split('\n').collect();
        if lines.len() > 1 {
            for line in &lines[1..] {
                let (k, v) = line.split_once(' ').unwrap_or((line, ""));
                ext.insert(k.to_owned(), v.to_owned());
            }
        }
        if let Some(mechs) = ext.get("AUTH") {
            self.auth = mechs.split(' ').map(str::to_owned).collect();
        }
        self.ext = Some(ext);
        Ok(())
    }

    /// `StartTLS` (smtp.go:158): after a 220 the connection is TLS (or failed — see
    /// [`Conn::start_tls`]) and EHLO is sent again.
    pub async fn start_tls(&mut self, config: &TlsConfig) -> Result<(), Error> {
        self.hello_once().await?;
        self.cmd(220, "STARTTLS").await?;
        self.conn.start_tls(config).await;
        self.tls = true;
        self.ehlo().await
    }

    /// `Auth` (smtp.go:197).
    pub async fn auth(&mut self, a: &mut dyn Auth) -> Result<(), Error> {
        self.hello_once().await?;
        let info = ServerInfo {
            name: self.server_name.clone(),
            tls: self.tls,
            auth: self.auth.clone(),
        };
        let (mech, resp) = match a.start(&info) {
            Ok(started) => started,
            Err(e) => {
                let _ = self.quit().await;
                return Err(e);
            }
        };
        let first = format!("AUTH {mech} {}", base64::encode(&resp));
        let mut reply = self.cmd(0, first.trim()).await;
        loop {
            let (code, msg64) = reply?;
            let challenge = match code {
                334 => base64::decode(&msg64).map_err(Error::from),
                235 => Ok(msg64.clone().into_bytes()),
                _ => Err(textproto::Error { code, msg: msg64 }.into()),
            };
            let resp = challenge.and_then(|msg| a.next(&msg, code == 334));
            match resp {
                Err(e) => {
                    // abort the AUTH
                    let _ = self.cmd(501, "*").await;
                    let _ = self.quit().await;
                    return Err(e);
                }
                Ok(None) => return Ok(()),
                Ok(Some(resp)) => {
                    reply = self.cmd(0, &base64::encode(&resp)).await;
                }
            }
        }
    }

    /// `Mail` (smtp.go:245): `BODY=8BITMIME` and `SMTPUTF8` when the server advertised them
    /// (keywords matched case-sensitively, as Go's map lookup is).
    pub async fn mail(&mut self, from: &str) -> Result<(), Error> {
        validate_line(from)?;
        self.hello_once().await?;
        let mut line = format!("MAIL FROM:<{from}>");
        if let Some(ext) = &self.ext {
            if ext.contains_key("8BITMIME") {
                line.push_str(" BODY=8BITMIME");
            }
            if ext.contains_key("SMTPUTF8") {
                line.push_str(" SMTPUTF8");
            }
        }
        self.cmd(250, &line).await.map(|_| ())
    }

    /// `Rcpt` (smtp.go:268): any 25x reply is success.
    pub async fn rcpt(&mut self, to: &str) -> Result<(), Error> {
        validate_line(to)?;
        self.cmd(25, &format!("RCPT TO:<{to}>")).await.map(|_| ())
    }

    /// `Data` (smtp.go:292).
    pub async fn data(&mut self) -> Result<DataWriter<'_>, Error> {
        self.cmd(354, "DATA").await?;
        Ok(DataWriter {
            client: self,
            dot: DotWriter::new(),
            pending: Vec::new(),
            err: None,
        })
    }

    /// `Extension` (smtp.go:374).
    pub async fn extension(&mut self, ext: &str) -> Option<String> {
        self.hello_once().await.ok()?;
        self.ext.as_ref()?.get(&ext.to_uppercase()).cloned()
    }

    /// `Quit` (smtp.go:410).
    pub async fn quit(&mut self) -> Result<(), Error> {
        let _ = self.hello_once().await; // ignore error; we're quitting anyhow
        self.cmd(221, "QUIT").await?;
        self.conn.close();
        Ok(())
    }
}

/// The `bufio.Writer` size under `textproto.Writer`.
const BUFIO_SIZE: usize = 4096;

/// Port of `dataCloser` over `textproto.Writer.DotWriter`: dot-stuffed bytes are buffered and
/// sent 4096 at a time, as the `bufio.Writer` underneath Go's does, so a transport failure is
/// reported by the same call (`write` or `close`) that would report it in Go.
pub struct DataWriter<'a> {
    client: &'a mut Client,
    dot: DotWriter,
    pending: Vec<u8>,
    err: Option<Error>,
}

impl DataWriter<'_> {
    /// `Write`.
    pub async fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        self.dot.write(data, &mut self.pending);
        // bufio flushes a full buffer when the next byte arrives.
        while self.pending.len() > BUFIO_SIZE {
            let chunk: Vec<u8> = self.pending.drain(..BUFIO_SIZE).collect();
            if let Err(e) = self.client.conn.write_all(&chunk).await {
                self.err = Some(e.clone());
                return Err(e);
            }
        }
        Ok(())
    }

    /// `Close`: terminate the data (a write failure here is ignored, as Go's `dataCloser`
    /// ignores the dot-writer's `Close` error) and read the final reply, which must be 250.
    pub async fn close(self) -> Result<(), Error> {
        let DataWriter {
            client,
            dot,
            mut pending,
            err,
        } = self;
        if err.is_none() {
            dot.close(&mut pending);
            let _ = client.conn.write_all(&pending).await;
        }
        let r = textproto::read_response(&mut client.conn, 250).await?;
        match r.err {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{SinkScript, normalise_transcript, server_tls_config, spawn_sink};

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../../fixtures/behaviour_mail.json")).unwrap()
    }

    pub(crate) fn script(v: &serde_json::Value) -> SinkScript {
        SinkScript {
            tls: v["tls"].as_bool().unwrap_or(false),
            greeting: v["greeting"].as_str().unwrap_or("").to_owned(),
            replies: v["replies"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            auth_replies: v["auth_replies"]
                .as_array()
                .map(|a| a.iter().map(|r| r.as_str().unwrap().to_owned()).collect())
                .unwrap_or_default(),
            close_on: v["close_on"].as_str().unwrap_or("").to_owned(),
        }
    }

    /// The body Go sent, recovered from its transcript: between the `DATA` line and the
    /// terminating dot, un-stuffed. Writing it back through this client must reproduce the
    /// transcript byte for byte.
    fn go_body(transcript: &str) -> Vec<u8> {
        let start = transcript.find("DATA\r\n").unwrap() + "DATA\r\n".len();
        let end = transcript.rfind("\r\n.\r\n").unwrap() + 2;
        transcript[start..end]
            .split_inclusive("\r\n")
            .map(|l| l.strip_prefix('.').unwrap_or(l))
            .collect::<String>()
            .into_bytes()
    }

    /// Every successful plaintext, unauthenticated send in the oracle, replayed through this
    /// client with the SMTP calls `sendMail` makes: the transcript — EHLO or the HELO fallback,
    /// the MAIL parameters the extensions earn, RCPT, DATA and the dot-stuffed body — is Go's.
    #[tokio::test]
    async fn client_replays_go_transcripts() {
        let oracle = oracle();
        let tls = server_tls_config(
            oracle["sink_cert_pem"].as_str().unwrap(),
            oracle["sink_key_pem"].as_str().unwrap(),
        );
        let mut replayed = 0;
        for row in oracle["smtp"].as_array().unwrap() {
            let config = &row["config"];
            if row["call"] != "send"
                || !row["error"].is_null()
                || row["script"]["tls"] == true
                || config["connection_security"] != ""
                || config["enable_smtp_auth"] == true
                || row["transcript"].as_str().unwrap().is_empty()
            {
                continue;
            }
            let name = row["name"].as_str().unwrap();
            let want = row["transcript"].as_str().unwrap();
            let (port, sink) = spawn_sink(script(&row["script"]), tls.clone()).await;
            let conn = crate::smtp::dial(&format!("127.0.0.1:{port}"), None)
                .await
                .unwrap();
            let mut c = Client::new(conn, "127.0.0.1").await.unwrap();
            let hostname = config["hostname"].as_str().unwrap();
            if !hostname.is_empty() {
                c.hello(hostname).await.unwrap();
            }
            c.mail(config["feedback_email"].as_str().unwrap())
                .await
                .unwrap();
            c.rcpt(row["args"]["to"].as_str().unwrap()).await.unwrap();
            let mut w = c.data().await.unwrap();
            w.write(&go_body(want)).await.unwrap();
            w.close().await.unwrap();
            c.close();
            let got = sink.await.unwrap();
            assert_eq!(normalise_transcript(&got), want, "{name}");
            replayed += 1;
        }
        assert!(replayed >= 10, "{replayed}");
    }

    #[tokio::test]
    async fn hello_after_other_methods_and_validate_line() {
        let oracle = oracle();
        let tls = server_tls_config(
            oracle["sink_cert_pem"].as_str().unwrap(),
            oracle["sink_key_pem"].as_str().unwrap(),
        );
        let s = SinkScript {
            greeting: "220 hi\r\n".into(),
            replies: [("EHLO".to_owned(), "250 ok\r\n".to_owned())].into(),
            ..Default::default()
        };
        let (port, _sink) = spawn_sink(s, tls).await;
        let conn = crate::smtp::dial(&format!("127.0.0.1:{port}"), None)
            .await
            .unwrap();
        let mut c = Client::new(conn, "h").await.unwrap();
        assert_eq!(
            c.rcpt("a\r\nb").await.unwrap_err().to_string(),
            "smtp: A line must not contain CR or LF"
        );
        assert!(c.extension("8bitmime").await.is_none());
        assert_eq!(
            c.hello("x").await.unwrap_err().to_string(),
            "smtp: Hello called after other methods"
        );
    }

    #[test]
    fn plain_auth_refusals() {
        let mut a = PlainAuth {
            identity: String::new(),
            username: "u".into(),
            password: "p".into(),
            host: "h:25".into(),
        };
        let info = |name: &str, tls| ServerInfo {
            name: name.into(),
            tls,
            auth: vec![],
        };
        let e = |r: Result<(String, Vec<u8>), Error>| r.unwrap_err().to_string();
        assert_eq!(e(a.start(&info("h:25", false))), "unencrypted connection");
        assert_eq!(e(a.start(&info("localhost", false))), "wrong host name");
        assert_eq!(e(a.start(&info("x:25", true))), "wrong host name");
        let (mech, resp) = a.start(&info("h:25", true)).unwrap();
        assert_eq!((mech.as_str(), resp.as_slice()), ("PLAIN", &b"\0u\0p"[..]));
        let mut local = PlainAuth {
            host: "127.0.0.1".into(),
            ..a.clone()
        };
        assert!(local.start(&info("127.0.0.1", false)).is_ok());
        assert!(
            PlainAuth {
                host: "::1".into(),
                ..a.clone()
            }
            .start(&info("::1", false))
            .is_ok()
        );
        assert_eq!(
            a.next(b"", true).unwrap_err().to_string(),
            "unexpected server challenge"
        );
        assert_eq!(a.next(b"", false).unwrap(), None);
    }
}
