//! Port of `app.GetSession` (channels/app/session.go:86).

use std::collections::HashMap;

use mm_model::session::{SESSION_ACTIVITY_TIMEOUT, Session};
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_store::{OAuthStore, SessionStore};

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

    /// Port of `App.RevokeAllSessions` (session.go:253) over
    /// `PlatformService.RevokeAllSessions` (platform/session.go:324).
    ///
    /// Moved here from `mm_app::bot`, where it was written as `revoke_all_sessions_for_bot`
    /// because the session store belonged to another agent that round — the move [D-283] asked
    /// for, now that session revocation has routes of its own.
    ///
    /// # Two error ids, and which one you get says where it broke
    ///
    /// Go reads the session list first and maps a failure there to
    /// `app.session.get_sessions.app_error`; a failure in the delete loop is
    /// `app.session.remove.app_error`. Both are 500 and both come out of the same function, so a
    /// port that used one id for the whole body would pass every status-code assertion and still
    /// be wrong on the wire.
    ///
    /// # It is not atomic, and Go is not either
    ///
    /// The loop deletes one row at a time and returns on the first failure, so a partial
    /// revocation is a reachable outcome — some sessions gone, some alive, and a 500. Wrapping it
    /// in a transaction would be *better* and would not be Go: a client that retries after the
    /// 500 must find the already-deleted rows missing, not restored.
    ///
    /// # The OAuth arm is still not ported
    ///
    /// `session.IsOAuth` sends Go through `RevokeAccessToken`, which deletes the
    /// `OAuthAccessData` row as well. [`App::revoke_session`] refuses such a session outright;
    /// this one cannot, because the sessions come from a list the caller never chose. Each is
    /// logged and removed like any other, which revokes it but strands its access data — still
    /// [D-283], now reachable from `POST /users/{user_id}/sessions/revoke/all`.
    ///
    /// `sendMobileWipeSignal` is an `a.Srv().Go(...)` push behind
    /// `MobileEphemeralModeSettings.Enable`, off by default and off the response path.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, revoked))]
    pub async fn revoke_all_sessions(&self, user_id: &str) -> AppResult {
        let sessions = self
            .store()
            .session()
            .get_sessions(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the session list could not be read");
                AppError::boxed(
                    "RevokeAllSessions",
                    "app.session.get_sessions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("revoked", sessions.len());
        for session in &sessions {
            if session.is_oauth {
                tracing::warn!(
                    session_id = %session.id,
                    "an OAuth session is being removed without its access data (D-283)",
                );
            }
            // `Remove` takes an id **or** a token; Go passes the id here, and so does this.
            self.store()
                .session()
                .remove(&session.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "a session could not be removed");
                    AppError::boxed(
                        "RevokeAllSessions",
                        "app.session.remove.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;
        }

        Ok(())
    }

    /// Port of `App.RevokeSessionsFromAllUsers` (session.go:281) over
    /// `PlatformService.RevokeSessionsFromAllUsers` (platform/session.go:167).
    ///
    /// Behind `POST /api/v4/users/sessions/revoke/all`, and it does what it says: every session
    /// row on the server, **including the caller's own**, so the response to a successful call is
    /// a 200 delivered over a credential that no longer authenticates anything.
    ///
    /// # The order of the two deletes is the security property
    ///
    /// Access data first, sessions second. Go's comment is "revoke tokens before sessions so they
    /// can't be used to relogin" (platform/session.go:169): an OAuth access token outlives the
    /// session it minted, so deleting sessions first opens a window in which every client just
    /// logged out can trade its token for a fresh one. Swapping these two lines leaves every
    /// test green and reopens that window, which is why there is a test below asserting the
    /// order rather than only the outcome.
    ///
    /// # Error ids do not follow the order
    ///
    /// The access-data failure is `app.oauth.remove_access_data.app_error`; the session failure —
    /// and Go's `default` arm — is `app.session.remove_all_sessions_for_team.app_error`. That
    /// second id names a *team* operation and has nothing to do with teams; it is Go's, copied.
    ///
    /// `GetAllSessionsWithActiveDeviceIds` runs first in Go, but only when
    /// `MobileEphemeralModeSettings.Enable` is on, and its only use is the mobile wipe push. Both
    /// are unported, so the read is skipped rather than performed and discarded.
    #[tracing::instrument(skip_all)]
    pub async fn revoke_sessions_from_all_users(&self) -> AppResult {
        self.store()
            .oauth()
            .remove_all_access_data()
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "access data could not be removed");
                AppError::boxed(
                    "RevokeSessionsFromAllUsers",
                    "app.oauth.remove_access_data.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.store()
            .session()
            .remove_all_sessions()
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "sessions could not be removed");
                AppError::boxed(
                    "RevokeSessionsFromAllUsers",
                    "app.session.remove_all_sessions_for_team.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(())
    }

    /// Port of `App.RevokeOtherSessionsForDeviceId` (session.go:330) over
    /// `PlatformService.RevokeOtherSessionsForDeviceId` (platform/session.go:187), and of its
    /// VoIP twin (session.go:341 / platform/session.go:207) — one function, because the two
    /// differ only in which column they compare and in two strings.
    ///
    /// # A failed revoke inside the loop is *not* an error
    ///
    /// Go logs `Could not revoke session for device` at warn and **carries on to the next
    /// session** (platform/session.go:199). Only the initial `GetSessions` can fail the call.
    /// Propagating a per-session failure would turn a device re-registration into a 500 and leave
    /// the remaining stale sessions alive, which is the opposite of what the loop is for.
    ///
    /// # The empty-id guard has a different status from everything else here
    ///
    /// An empty id is **400** `app.session.revoke_other_sessions.empty_device_id.app_error`
    /// (`…empty_voip_device_id…` for the VoIP arm), where a store failure is 500
    /// `app.session.get_sessions.app_error`. It is unreachable from `handleDeviceProps`, which
    /// only calls in when at least one id is non-empty — reproduced anyway because it is the
    /// caller's contract and the next caller may not check.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, voip = voip, revoked))]
    pub async fn revoke_other_sessions_for_device_id(
        &self,
        user_id: &str,
        device_id: &str,
        current_session_id: &str,
        voip: bool,
    ) -> AppResult {
        let (caller, empty_id) = if voip {
            (
                "RevokeOtherSessionsForVoIPDeviceId",
                "app.session.revoke_other_sessions.empty_voip_device_id.app_error",
            )
        } else {
            (
                "RevokeOtherSessionsForDeviceId",
                "app.session.revoke_other_sessions.empty_device_id.app_error",
            )
        };

        if device_id.is_empty() {
            return Err(AppError::boxed(caller, empty_id, None, String::new(), 400));
        }

        let sessions = self
            .store()
            .session()
            .get_sessions(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the session list could not be read");
                AppError::boxed(
                    caller,
                    "app.session.get_sessions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut revoked = 0_usize;
        for session in &sessions {
            let matches = if voip {
                session.voip_device_id == device_id
            } else {
                session.device_id == device_id
            };
            // `session.Id != currentSessionId` — the caller's own session is the *one* it must
            // keep, and it is the session that just registered this device. Dropping this half of
            // the predicate logs the caller out of the request they are in the middle of.
            if !matches || session.id == current_session_id {
                continue;
            }

            if let Err(err) = self.revoke_session(session).await {
                // Warn and continue; see the note above.
                tracing::warn!(
                    session_id = %session.id,
                    error = %err,
                    "could not revoke session for device",
                );
                continue;
            }
            revoked += 1;
        }

        tracing::Span::current().record("revoked", revoked);
        Ok(())
    }

    /// Port of `App.SetExtraSessionProps` (session.go:395).
    ///
    /// # The `changed` flag is not an optimisation
    ///
    /// Go compares every incoming value against the one already on the session and **returns
    /// without writing** when none differs. Removing that short-circuit would turn a mobile
    /// client's periodic no-op device call into a write on every request. The comparison is
    /// against `session.Props[k]`, so a key that is *absent* differs from `""` only when the
    /// incoming value is non-empty — and `handleDeviceProps` never sends an empty one, having
    /// filtered those out before it calls in.
    ///
    /// The session is mutated in place whether or not the write succeeds, matching Go: `AddProp`
    /// runs inside the loop, before `UpdateProps`.
    #[tracing::instrument(skip_all, fields(session_id = %session.id, changed))]
    pub async fn set_extra_session_props(
        &self,
        session: &mut Session,
        new_props: &[(&str, &str)],
    ) -> AppResult {
        let mut changed = false;
        for (key, value) in new_props {
            if session.props.as_ref().and_then(|props| props.get(*key))
                == Some(&(*value).to_owned())
            {
                continue;
            }
            session.add_prop(*key, *value);
            changed = true;
        }

        tracing::Span::current().record("changed", changed);
        if !changed {
            return Ok(());
        }

        self.store()
            .session()
            .update_props(session)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "session props could not be written");
                AppError::boxed(
                    "SetExtraSessionProps",
                    "app.session.set_extra_session_prop.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.AttachDeviceId` (session.go:387).
    ///
    /// A thin wrapper over [`mm_store::SessionStore::update_device_id`] whose only content is the
    /// error mapping — one arm, 500 `app.session.update_device_id.app_error`.
    #[tracing::instrument(skip_all, fields(session_id = %session_id))]
    pub async fn attach_device_id(
        &self,
        session_id: &str,
        device_id: &str,
        voip_device_id: &str,
        expires_at: i64,
    ) -> AppResult {
        self.store()
            .session()
            .update_device_id(session_id, device_id, voip_device_id, expires_at)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the device id could not be written");
                AppError::boxed(
                    "AttachDeviceId",
                    "app.session.update_device_id.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `PlatformService.SetSessionExpireInHours` (platform/session.go:275).
    ///
    /// Mutates the session **in memory only** — the write is the caller's, through
    /// [`App::attach_device_id`], which takes the new `ExpiresAt` as an argument.
    ///
    /// The base the hours are added to is *not* always now. Go uses `CreateAt + hours` unless the
    /// session has no `CreateAt` or `ExtendSessionLengthWithActivity` is on, in which case it is
    /// `now + hours`. So on a stock **upgraded** server (where that setting defaults off) a
    /// mobile client attaching a device id gets an expiry measured from when it logged in, not
    /// from when it called — a session created 100 days ago with a 180-day mobile length has 80
    /// days left, not 180. Collapsing this to `now + hours` is the plausible wrong reading.
    pub fn set_session_expire_in_hours(&self, session: &mut Session, hours: i64) {
        let base = if session.create_at == 0 || self.config().extend_session_length_with_activity {
            get_millis()
        } else {
            session.create_at
        };
        session.expires_at = base + (1000 * 60 * 60 * hours);
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

    // ---------------------------------------------------------------------------------------
    // The session write family.
    // ---------------------------------------------------------------------------------------

    /// An [`App`] with no reachable database, for the branches that never touch the store.
    fn offline_app(config: Config) -> crate::App {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            // sqlx's default `acquire_timeout` is 30 seconds and the connection to :1 is refused
            // instantly but retried until it expires. Capped for the same reason
            // `authorization.rs` caps it.
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nonexistent")
            .expect("a lazy pool never connects");
        crate::App::with_config(mm_store::SqlStore::from_pool(pool), config)
    }

    /// `SetSessionExpireInHours` measures from `CreateAt`, **not** from now, unless sliding
    /// expiry is on or the session has no `CreateAt`.
    ///
    /// This is the branch that decides how long a phone stays logged in, and the wrong reading —
    /// "now + hours" — is both the obvious one and invisible in any test written against a
    /// freshly created session, where the two agree to within a millisecond. So the session here
    /// is deliberately old.
    #[tokio::test]
    async fn the_mobile_expiry_is_measured_from_create_at_on_a_stock_server() {
        // A session created 100 days ago, on a server where `ExtendSessionLengthWithActivity` is
        // off — which is every upgraded install, because the default is `!isUpdate`.
        let created = get_millis() - 100 * 24 * 60 * 60 * 1000;
        let app = offline_app(Config {
            extend_session_length_with_activity: false,
            ..Config::default()
        });

        let mut session = Session {
            create_at: created,
            ..Session::default()
        };
        app.set_session_expire_in_hours(&mut session, 4320);

        assert_eq!(
            session.expires_at,
            created + 4320 * 60 * 60 * 1000,
            "the base is CreateAt, so a 180-day length on a 100-day-old session leaves 80 days"
        );
        // And it is emphatically not `now + hours`, which is 100 days further out.
        let from_now = get_millis() + 4320 * 60 * 60 * 1000;
        assert!(
            session.expires_at < from_now - 99 * 24 * 60 * 60 * 1000,
            "measuring from now would extend the session by the session's whole age"
        );
    }

    /// The two branches that *do* measure from now: sliding expiry on, or no `CreateAt` at all.
    #[tokio::test]
    async fn the_mobile_expiry_is_measured_from_now_when_sliding_or_uncreated() {
        let sliding = offline_app(Config {
            extend_session_length_with_activity: true,
            ..Config::default()
        });
        let created = get_millis() - 100 * 24 * 60 * 60 * 1000;
        let mut session = Session {
            create_at: created,
            ..Session::default()
        };
        let before = get_millis();
        sliding.set_session_expire_in_hours(&mut session, 1);
        assert!(
            session.expires_at >= before + 60 * 60 * 1000,
            "sliding expiry measures from now even on an old session"
        );

        // `CreateAt == 0` takes the same arm regardless of the setting — otherwise an unsaved
        // session would be handed an expiry in 1970 and be born already expired.
        let stock = offline_app(Config {
            extend_session_length_with_activity: false,
            ..Config::default()
        });
        let mut unsaved = Session::default();
        let before = get_millis();
        stock.set_session_expire_in_hours(&mut unsaved, 1);
        assert!(unsaved.expires_at >= before + 60 * 60 * 1000);
    }

    /// The hours-to-millis arithmetic, at a size where a `* 1000` in the wrong place shows.
    #[tokio::test]
    async fn the_expiry_arithmetic_is_hours_times_3_600_000() {
        let app = offline_app(Config {
            extend_session_length_with_activity: false,
            ..Config::default()
        });
        let mut session = Session {
            create_at: 1_000_000_000_000,
            ..Session::default()
        };
        app.set_session_expire_in_hours(&mut session, 4320);
        assert_eq!(session.expires_at, 1_000_000_000_000 + 15_552_000_000);

        // Zero hours is a session that expires the instant it was created — reachable from a
        // `SessionLengthMobileInHours` of 0, and it must not become "no expiry".
        let mut zero = Session {
            create_at: 1_000_000_000_000,
            ..Session::default()
        };
        app.set_session_expire_in_hours(&mut zero, 0);
        assert_eq!(zero.expires_at, 1_000_000_000_000);
    }

    /// `SetExtraSessionProps` writes **only** when something actually differs. The store here is
    /// unreachable, so any attempt to write is an `Err` — which makes "did not write" directly
    /// observable rather than something to take on trust.
    #[tokio::test]
    async fn set_extra_session_props_does_not_write_when_nothing_changed() {
        let app = offline_app(Config::default());

        // No props at all and nothing to set: the loop never runs, `changed` stays false.
        let mut session = Session::default();
        app.set_extra_session_props(&mut session, &[])
            .await
            .expect("an empty prop list must not reach the store");

        // A value that already matches: the `continue` arm.
        let mut session = Session::default();
        session.add_prop("mobile_version", "2.34.0");
        app.set_extra_session_props(&mut session, &[("mobile_version", "2.34.0")])
            .await
            .expect("an unchanged value must not reach the store");

        // Two keys, both already matching — so a port that broke out of the loop on the first
        // match rather than continuing would still pass, but one that ORed instead of ANDed
        // would not.
        let mut session = Session::default();
        session.add_prop("mobile_version", "2.34.0");
        session.add_prop("device_notification_disabled", "true");
        app.set_extra_session_props(
            &mut session,
            &[
                ("mobile_version", "2.34.0"),
                ("device_notification_disabled", "true"),
            ],
        )
        .await
        .expect("two unchanged values must not reach the store");
    }

    /// The mirror: a genuine change must reach the store, and the session is mutated either way.
    #[tokio::test]
    async fn set_extra_session_props_writes_when_a_value_differs() {
        let app = offline_app(Config::default());

        let mut session = Session::default();
        session.add_prop("mobile_version", "2.33.0");
        let err = app
            .set_extra_session_props(&mut session, &[("mobile_version", "2.34.0")])
            .await
            .expect_err("a changed value must reach the unreachable store and fail");
        assert_eq!(err.id, "app.session.set_extra_session_prop.app_error");
        assert_eq!(err.status_code, 500);

        // `AddProp` runs inside the loop, before `UpdateProps` — so the in-memory session carries
        // the new value even though the write failed. Go's ordering, and the reason a caller
        // must not treat the session it holds as persisted.
        assert_eq!(
            session.props.as_ref().and_then(|p| p.get("mobile_version")),
            Some(&"2.34.0".to_owned())
        );

        // A key that is absent differs from any non-empty value, which is how the *first*
        // device call on a session ever writes anything.
        let mut fresh = Session::default();
        app.set_extra_session_props(&mut fresh, &[("mobile_version", "2.34.0")])
            .await
            .expect_err("an absent key is a change");
    }

    /// The empty-id guard on the device revocation, which is 400 and not the 500 every other
    /// failure on that path is — and which names a different parameter for the VoIP arm.
    #[tokio::test]
    async fn an_empty_device_id_is_a_400_naming_its_own_parameter() {
        let app = offline_app(Config::default());

        let err = app
            .revoke_other_sessions_for_device_id("user", "", "session", false)
            .await
            .expect_err("an empty device id is refused");
        assert_eq!(
            err.id,
            "app.session.revoke_other_sessions.empty_device_id.app_error"
        );
        assert_eq!(err.status_code, 400);
        assert_eq!(err.where_, "RevokeOtherSessionsForDeviceId");

        let err = app
            .revoke_other_sessions_for_device_id("user", "", "session", true)
            .await
            .expect_err("an empty VoIP device id is refused");
        assert_eq!(
            err.id,
            "app.session.revoke_other_sessions.empty_voip_device_id.app_error"
        );
        assert_eq!(err.status_code, 400);
        assert_eq!(err.where_, "RevokeOtherSessionsForVoIPDeviceId");

        // The guard runs **before** the store, so a non-empty id on the same unreachable store is
        // a different error entirely. That is what proves the 400 above came from the guard
        // rather than from a store failure that happened to be mapped.
        let err = app
            .revoke_other_sessions_for_device_id("user", "apple_rn:tok", "session", false)
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.id, "app.session.get_sessions.app_error");
        assert_eq!(err.status_code, 500);
    }

    /// `RevokeAllSessions` maps a list failure and a delete failure to **different** error ids.
    /// Only the first is reachable without a database; the second is asserted at the API edge.
    #[tokio::test]
    async fn revoke_all_sessions_reports_a_list_failure_as_its_own_id() {
        let app = offline_app(Config::default());
        let err = app
            .revoke_all_sessions("user")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.id, "app.session.get_sessions.app_error");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.where_, "RevokeAllSessions");
    }

    /// `RevokeSessionsFromAllUsers` deletes **access data before sessions**, and the error id
    /// says which half failed. With an unreachable store the first statement is the one that
    /// fails, so this pins the order: if the two were swapped, the id would be the session one.
    #[tokio::test]
    async fn the_all_users_revoke_removes_access_data_first() {
        let app = offline_app(Config::default());
        let err = app
            .revoke_sessions_from_all_users()
            .await
            .expect_err("the store is unreachable");
        assert_eq!(
            err.id, "app.oauth.remove_access_data.app_error",
            "access data must be deleted before sessions — a token outlives the session it \
             minted, so the other order leaves a relogin window"
        );
        assert_eq!(err.status_code, 500);
        assert_eq!(err.where_, "RevokeSessionsFromAllUsers");
    }

    /// `AttachDeviceId`'s one error arm.
    #[tokio::test]
    async fn attach_device_id_has_a_single_error_arm() {
        let app = offline_app(Config::default());
        let err = app
            .attach_device_id("session", "apple_rn:tok", "", 1_800_000_000_000)
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.id, "app.session.update_device_id.app_error");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.where_, "AttachDeviceId");
    }
}
