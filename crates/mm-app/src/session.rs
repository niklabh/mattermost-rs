//! Port of `app.GetSession` (channels/app/session.go:86).

use std::collections::HashMap;

use mm_model::session::{SESSION_ACTIVITY_TIMEOUT, Session};
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_store::SessionStore;

use crate::App;
use crate::config::Config;

/// Go's error id for every failure on this path, whatever the cause. 401 in all cases.
const INVALID_TOKEN: &str = "api.context.invalid_token.error";

impl App {
    /// Port of `app.App.GetSession` (session.go:86).
    ///
    /// # The token-vs-id check is load-bearing
    ///
    /// `SessionStore.Get` matches `Token = $1 OR Id = $1`, so it will happily return a session
    /// when the caller passed a session **id**. Go catches that one line later — `if session.Token
    /// != token` — and rejects it (session.go:95). Dropping that check would turn session ids into
    /// bearer credentials, and session ids are far less protected than tokens: they appear in
    /// admin APIs and logs. It is reproduced here for that reason, not for tidiness.
    ///
    /// # The idle timeout, and why its four exemptions are load-bearing
    ///
    /// Go revokes a session whose `LastActivityAt` is older than
    /// `ServiceSettings.SessionIdleTimeoutInMinutes` — but only when that setting is positive,
    /// the session is not OAuth, not a mobile app, not minted from a user access token, and
    /// `ExtendSessionLengthWithActivity` is **off** (session.go:118-137). Each exemption widens
    /// what authenticates, so dropping one logs out a class of client the Go server keeps: an
    /// OAuth integration, a phone, a bot's personal access token, or every session on a server
    /// configured for sliding expiry. They are asserted one at a time in the tests below rather
    /// than as a single conjunction.
    ///
    /// The revoke itself is **synchronous here and a goroutine in Go**. Go's own comment says the
    /// goroutine exists to break a re-entrancy cycle in the web hub — a component we do not have —
    /// and that it does not wait for the result. Neither ordering is observable in the response,
    /// which is the 401 either way; doing it inline removes a race from the tests and guarantees
    /// the row is gone before the client can retry. A failed delete is logged and the 401 still
    /// returned, which is Go's behaviour by omission.
    ///
    /// # What this port still does not do
    ///
    /// Go consults a session cache and mints a session from a *user access token* when the lookup
    /// misses. Neither is ported: the cache is an optimisation we deliberately do without
    /// ([D-087]), and the access-token path is its own route's worth of work.
    #[tracing::instrument(skip_all, fields(session_id, user_id))]
    pub async fn get_session(&self, token: &str) -> AppResult<Session> {
        let session = match self.store().session().get(token).await {
            Ok(session) => session,
            Err(err) => {
                // Go skips the error check entirely here and only tests whether a session came
                // back, because a miss is a legitimate route into the access-token path. The
                // distinction still matters for us: a broken query must not be reported to the
                // client as a bad token.
                if !err.is_not_found() {
                    tracing::error!(error = %err, "session lookup failed");
                    return Err(AppError::boxed(
                        "GetSession",
                        "app.session.get.app_error",
                        None,
                        String::new(),
                        500,
                    ));
                }
                return Err(invalid_token("session not found"));
            }
        };

        if session.token != token {
            return Err(invalid_token(
                "session token is different from the one in DB",
            ));
        }

        if session.id.is_empty() || session.is_expired() {
            return Err(invalid_token("session is either nil or expired"));
        }

        if session_is_idle_past_timeout(self.config(), &session, get_millis()) {
            // Go: `a.Srv().Go(func() { RevokeSessionById(session.Id) })`, whose result it never
            // reads. `RevokeSessionById` re-fetches the row by id before deleting it; we already
            // hold the row, and the extra read exists in Go only because its goroutine captures
            // an id rather than a session.
            //
            // The two branches `platform.RevokeSession` has beyond the delete are both
            // unreachable from *this* caller by construction: the OAuth branch needs
            // `session.IsOAuth`, and `sendMobileWipeSignal` needs a device id — and the guard
            // above has already excluded both. So a plain `Remove` is the whole of it here, and
            // a general `RevokeSession` is deliberately not invented for a caller that cannot
            // reach its other halves.
            if let Err(err) = self.store().session().remove(&session.id).await {
                tracing::warn!(error = %err, session_id = %session.id, "error while revoking session");
            }
            return Err(invalid_token("idle timeout"));
        }

        tracing::Span::current().record("session_id", &session.id);
        tracing::Span::current().record("user_id", &session.user_id);
        Ok(session)
    }

    /// Port of `PlatformService.UpdateLastActivityAtIfNeeded` (platform/status.go:278).
    ///
    /// A **write on the read path**, and the reason it is not optional: Go's idle-timeout check
    /// above reads exactly the column this writes. A request served here without it moves a live
    /// user closer to being logged out by the Go server next door, which is what [D-084] recorded
    /// and this closes. The two are one change for that reason.
    ///
    /// # "If needed" is a five-minute throttle, not a cache lookup
    ///
    /// `model.SessionActivityTimeout` is five minutes, and Go's guard is a strict `<` on the
    /// *skip* side — so a session refreshed exactly five minutes ago **is** written. See
    /// [`activity_write_is_due`], which states it the other way round and is asserted at the
    /// millisecond. The throttle keeps the ratio of writes to reads sane on a busy server; it is
    /// not correctness, and getting the comparison backwards would write on every request rather
    /// than fail visibly.
    ///
    /// # It cannot fail
    ///
    /// Go logs a warning and returns; the caller is a response handler with the body already
    /// built, and Go has no way to report this to the client. Same here — the signature has no
    /// error, deliberately, so no caller can be tempted to turn a failed activity write into a
    /// failed request.
    ///
    /// Go's `UpdateWebConnUserActivity` call, first in the function, is skipped: it touches the
    /// websocket hub, which is phase 5. `PHASE5: update WebConn activity`. Go's own
    /// `session.LastActivityAt = now` afterwards mutates a **by-value copy** on its way into the
    /// session cache, so with no cache there is nothing for it to do here.
    #[tracing::instrument(skip_all, fields(session_id = %session.id, wrote))]
    pub async fn update_last_activity_at_if_needed(&self, session: &Session) {
        let now = get_millis();

        // PHASE5: update WebConn activity (`ps.UpdateWebConnUserActivity(session, now)`).

        if !activity_write_is_due(now, session.last_activity_at) {
            tracing::Span::current().record("wrote", false);
            return;
        }
        tracing::Span::current().record("wrote", true);

        if let Err(err) = self
            .store()
            .session()
            .update_last_activity_at(&session.id, now)
            .await
        {
            tracing::warn!(
                error = %err,
                user_id = %session.user_id,
                session_id = %session.id,
                "Failed to update LastActivityAt"
            );
        }
    }
}

/// Go passes `map[string]any{"Token": token, "Error": ""}` as the params. The token is a live
/// credential and `AppError`'s params are not serialised (`json:"-"`), but they do reach the i18n
/// layer and any logger that formats the struct — so the token is omitted rather than carried.
/// See D-079.
fn invalid_token(details: &str) -> Box<AppError> {
    let mut params: HashMap<String, serde_json::Value> = HashMap::new();
    params.insert("Error".to_owned(), serde_json::Value::String(String::new()));
    AppError::boxed("GetSession", INVALID_TOKEN, Some(params), details, 401)
}

/// The throttle in `UpdateLastActivityAtIfNeeded` (platform/status.go:282), extracted for the same
/// reason as [`session_is_idle_past_timeout`]'s `now`: with the clock inlined, "exactly five
/// minutes" cannot be constructed and the `<` boundary is untestable.
///
/// Go writes when `now - LastActivityAt` is **not less than** `SessionActivityTimeout`, so a
/// session refreshed exactly five minutes ago is written again.
fn activity_write_is_due(now: i64, last_activity_at: i64) -> bool {
    (now - last_activity_at) >= SESSION_ACTIVITY_TIMEOUT
}

/// The predicate of Go's idle-timeout branch (session.go:118-124).
///
/// A free function over [`Config`] rather than a method on [`App`], so that every exemption can be
/// asserted without a database — an `App` owns a `SqlStore`, and a check that consults no store
/// should not need one to be tested. `now` is a parameter for the same reason: Go reads
/// `model.GetMillis()` inline, and with the clock inlined here the strict `>` at the boundary is
/// untestable — every "exactly the timeout" fixture is already a few milliseconds past it by the
/// time the comparison runs, so `>` and `>=` are indistinguishable and a mutation of one into the
/// other survives.
///
/// Reads as "the timeout is armed **and** the session has been idle longer than it". The four
/// exemptions are `||`-ed into the disarming half deliberately: Go's condition is one `&&` chain
/// and inverting it wholesale is where a reader drops a clause.
fn session_is_idle_past_timeout(config: &Config, session: &Session, now: i64) -> bool {
    let minutes = config.session_idle_timeout_in_minutes;
    if minutes <= 0
        || session.is_oauth
        || session.is_mobile_app()
        // Go spells this `session.Props[SessionPropType] != SessionTypeUserAccessToken` inline
        // rather than calling its own `IsUserAccessToken`; the two are the same predicate, an
        // absent prop included.
        || session.is_user_access_token()
        || config.extend_session_length_with_activity
    {
        return false;
    }

    // `int64(*minutes) * 1000 * 60`. Go's `*int` is 64-bit on every platform this runs on and the
    // multiplication happens in `int64`, so a configured value large enough to overflow would wrap
    // there too — `saturating_mul` is used instead rather than reproducing a wrap that no operator
    // can reach and that would turn a huge timeout into an instant logout.
    let timeout = minutes.saturating_mul(1000).saturating_mul(60);

    // Strict `>`: a session idle by exactly the timeout survives on both servers.
    (now - session.last_activity_at) > timeout
}

impl App {
    /// Port of `app.App.GetSessions` (channels/app/session.go:144).
    ///
    /// Go's mapping here is blunt on purpose: *any* store failure becomes
    /// `app.session.get_sessions.app_error` with a 500. There is no not-found branch, because a
    /// user with no sessions is an empty list rather than a miss.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn get_sessions(&self, user_id: &str) -> AppResult<Vec<Session>> {
        self.store()
            .session()
            .get_sessions(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "sessions lookup failed");
                AppError::boxed(
                    "GetSessions",
                    "app.session.get_sessions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::session::external::USER_AUTH_SERVICE_IS_MOBILE;

    /// Thirty days, which is Go's default for `SessionIdleTimeoutInMinutes` and therefore the
    /// number every test here computes against.
    const DEFAULT_TIMEOUT_MINUTES: i64 = 43_200;
    const DEFAULT_TIMEOUT_MILLIS: i64 = DEFAULT_TIMEOUT_MINUTES * 60 * 1000;

    /// A fixed "now", so that `now - last_activity_at` is exactly what each test says it is.
    /// With a live clock the strict `>` at the boundary is unobservable — see the doc on
    /// [`session_is_idle_past_timeout`].
    const NOW: i64 = 1_800_000_000_000;

    /// A session idle for `idle_millis` as of [`NOW`].
    fn idle_session(idle_millis: i64) -> Session {
        Session {
            id: "sessionidsessionidsession1".to_owned(),
            token: "tokentokentokentokentoken1".to_owned(),
            last_activity_at: NOW - idle_millis,
            ..Session::default()
        }
    }

    /// The configuration the **live Go server writes**: the timeout armed at its 43200-minute
    /// default and sliding expiry off, because the persisted document is an update
    /// (`SiteURL` present) and `ExtendSessionLengthWithActivity` is `!isUpdate`.
    fn armed() -> Config {
        Config {
            session_idle_timeout_in_minutes: DEFAULT_TIMEOUT_MINUTES,
            extend_session_length_with_activity: false,
            ..Config::default()
        }
    }

    #[test]
    fn an_idle_session_is_past_the_timeout() {
        let session = idle_session(DEFAULT_TIMEOUT_MILLIS + 60_000);
        assert!(session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    #[test]
    fn a_recently_active_session_is_not() {
        let session = idle_session(60_000);
        assert!(!session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    /// Go's comparison is `>`, so idle by *exactly* the timeout is still valid. Asserted at the
    /// millisecond, which is only possible because `now` is a parameter: an off-by-one here logs
    /// a user out one server-request early, and against a live clock `>` and `>=` are the same
    /// function.
    #[test]
    fn the_boundary_is_a_strict_greater_than() {
        assert!(
            !session_is_idle_past_timeout(&armed(), &idle_session(DEFAULT_TIMEOUT_MILLIS), NOW),
            "idle by exactly the timeout is still valid"
        );
        assert!(
            session_is_idle_past_timeout(&armed(), &idle_session(DEFAULT_TIMEOUT_MILLIS + 1), NOW),
            "one millisecond more is not"
        );
    }

    /// The throttle boundary, likewise at the millisecond. Go's guard is
    /// `now - LastActivityAt < SessionActivityTimeout`, so five minutes to the millisecond is
    /// **due**, and one millisecond short is not.
    #[test]
    fn the_activity_write_is_due_at_exactly_five_minutes() {
        let five_minutes = 1000 * 60 * 5;
        assert!(!activity_write_is_due(NOW, NOW - (five_minutes - 1)));
        assert!(activity_write_is_due(NOW, NOW - five_minutes));
        assert!(activity_write_is_due(NOW, NOW - (five_minutes + 1)));
    }

    /// A session whose `LastActivityAt` is in the future — a clock skew between two servers
    /// writing the same row — is not due, and is not idle either. Both predicates go negative
    /// there rather than wrapping.
    #[test]
    fn a_future_last_activity_is_neither_due_nor_idle() {
        assert!(!activity_write_is_due(NOW, NOW + 60_000));
        assert!(!session_is_idle_past_timeout(
            &armed(),
            &idle_session(-60_000),
            NOW
        ));
    }

    /// Zero disarms the check outright — Go's `> 0` guard. This is the one that would bite a port
    /// that treated the setting as "always on with a default": an operator who sets it to `0` has
    /// switched idle revocation **off**, not set an instant timeout.
    #[test]
    fn a_zero_timeout_disarms_the_check() {
        let session = idle_session(DEFAULT_TIMEOUT_MILLIS * 100);
        let config = Config {
            session_idle_timeout_in_minutes: 0,
            ..armed()
        };
        assert!(!session_is_idle_past_timeout(&config, &session, NOW));
    }

    /// A negative value is `> 0`-false too. Go's setting is a plain `*int` with no validation on
    /// this field, so a hand-edited config really can hold one.
    #[test]
    fn a_negative_timeout_disarms_the_check() {
        let config = Config {
            session_idle_timeout_in_minutes: -1,
            ..armed()
        };
        assert!(!session_is_idle_past_timeout(
            &config,
            &idle_session(DEFAULT_TIMEOUT_MILLIS * 100),
            NOW
        ));
    }

    /// Sliding expiry and idle revocation are alternatives in Go, never both. With this on, an
    /// arbitrarily idle session authenticates.
    #[test]
    fn extend_session_length_with_activity_disarms_the_check() {
        let config = Config {
            extend_session_length_with_activity: true,
            ..armed()
        };
        assert!(!session_is_idle_past_timeout(
            &config,
            &idle_session(DEFAULT_TIMEOUT_MILLIS * 100),
            NOW
        ));
    }

    /// The four per-session exemptions, one assertion each. A conjunction would pass with three
    /// of the four dropped.
    #[test]
    fn an_oauth_session_is_exempt() {
        let session = Session {
            is_oauth: true,
            ..idle_session(DEFAULT_TIMEOUT_MILLIS * 100)
        };
        assert!(!session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    /// `IsMobileApp` is `DeviceId != "" || IsMobile()` — the device id half.
    #[test]
    fn a_session_with_a_device_id_is_exempt() {
        let session = Session {
            device_id: "apple:abcd".to_owned(),
            ..idle_session(DEFAULT_TIMEOUT_MILLIS * 100)
        };
        assert!(!session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    /// …and the `isMobile` prop half, which a mobile client sets without a device id until it
    /// registers for push.
    #[test]
    fn a_session_flagged_is_mobile_is_exempt() {
        let mut session = idle_session(DEFAULT_TIMEOUT_MILLIS * 100);
        session.add_prop(USER_AUTH_SERVICE_IS_MOBILE, "true");
        assert!(!session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    /// A personal access token's session never idles out — it is how bots and scripts
    /// authenticate, and they are idle by definition between runs.
    #[test]
    fn a_user_access_token_session_is_exempt() {
        let mut session = idle_session(DEFAULT_TIMEOUT_MILLIS * 100);
        session.add_prop(
            mm_model::session::SESSION_PROP_TYPE,
            mm_model::session::SESSION_TYPE_USER_ACCESS_TOKEN,
        );
        assert!(!session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    /// An unrelated `type` prop is **not** an exemption — the comparison is against one literal,
    /// not "has a type".
    #[test]
    fn another_session_type_is_not_exempt() {
        let mut session = idle_session(DEFAULT_TIMEOUT_MILLIS * 100);
        session.add_prop(
            mm_model::session::SESSION_PROP_TYPE,
            mm_model::session::SESSION_TYPE_CLOUD_KEY,
        );
        assert!(session_is_idle_past_timeout(&armed(), &session, NOW));
    }

    /// The error the idle branch returns is the same 401 and the same id as every other refusal
    /// on this path; only `detailed_error` distinguishes it, which is Go's choice and a
    /// deliberate one — the client must not be able to tell a revoked session from a wrong token.
    #[test]
    fn the_idle_refusal_is_indistinguishable_from_a_bad_token() {
        let idle = invalid_token("idle timeout");
        let unknown = invalid_token("session not found");
        assert_eq!(idle.id, unknown.id);
        assert_eq!(idle.status_code, unknown.status_code);
        assert_eq!(idle.message, unknown.message);
        assert_ne!(idle.detailed_error, unknown.detailed_error);
    }

    #[test]
    fn invalid_token_is_401_with_gos_error_id() {
        let err = invalid_token("session is either nil or expired");
        assert_eq!(err.id, INVALID_TOKEN);
        assert_eq!(err.status_code, 401);
        assert_eq!(err.where_, "GetSession");
        assert_eq!(err.detailed_error, "session is either nil or expired");
    }

    /// The params map reaches loggers. Go puts the token in it; we must not.
    #[test]
    fn invalid_token_params_omit_the_token() {
        let err = invalid_token("whatever");
        let params = err.params.expect("params are set");
        assert!(!params.contains_key("Token"));
        assert_eq!(
            params.get("Error"),
            Some(&serde_json::Value::String(String::new()))
        );
    }

    /// `AppError::new` starts `message` at `id`, which is what an untranslated Go error renders
    /// as. The client sees this string, so it is part of the wire format.
    #[test]
    fn message_defaults_to_the_error_id() {
        let err = invalid_token("x");
        assert_eq!(err.message, INVALID_TOKEN);
    }
}
