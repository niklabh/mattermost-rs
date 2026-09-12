//! Port of `server/channels/app/channel_join_request.go` — the whole file.
//!
//! Behind the seven routes of `api4/channel_join_request.go`: requesting to join a *discoverable*
//! private channel, withdrawing that request, and the admin review of it.
//!
//! # The ABAC fast path is dark on this deployment, and that is a licence fact, not a guess
//!
//! `RequestJoinChannel` asks `ChannelAccessControlled` (app/channel.go:4522) and, when a policy is
//! attached *and* the user qualifies, adds the member directly and answers
//! `{"status":"approved"}` with no request row at all. That gate's **first line** is
//! `!MinimumEnterpriseAdvancedLicense(a.License())`, and the image this stack runs is
//! `mattermost-team-edition` with zero rows in `Licenses` — so it returns `false` before reading
//! the config or the channel. [`App::channel_access_controlled`] is therefore a constant, pinned
//! by a test, exactly like [`App::team_membership_access_control_enabled`]. Every request
//! consequently takes the request-row path, which is what the parity suite measures.
//!
//! # A conflicting save is a **success**, not a 409
//!
//! The partial unique index refuses a second pending row per `(channel, user)`. Go catches the
//! conflict, re-reads the caller's existing pending request and returns **it** with a 201 — so
//! POSTing twice is idempotent and hands back the original `id`, `create_at` and `message`.
//! Measured against the discoverable-on Go oracle, not inferred. Only a conflict whose re-read
//! *also* fails becomes `…duplicate.app_error` at 409, which nothing reachable produces.
//!
//! # Two places drop the requester's free text, and both are deliberate
//!
//! Withdrawing sets `Message = ""`, and a review sets `Message = ""` as well (Go's comment: "it
//! served its purpose during review and keeping it would leak free-text into the audit trail").
//! So the body a client gets back from `DELETE` and `PATCH` has an **empty** message even though
//! the row it started from had one. Dropping either assignment is invisible on the happy path of a
//! request created without a message.
//!
//! # The websocket broadcasts carry a hook this server does not run
//!
//! `useOnlyChannelAdminsHook` narrows a channel-scoped broadcast to the channel's *admins*. The
//! hook fields are attached here and stripped before send — [D-183] — but the hook itself is not
//! executed, so on this server every channel member would receive a join-request event rather
//! than only the admins. That is a disclosure divergence, recorded as [D-340], and it is latent:
//! the routes that raise these events are dark.

use mm_model::channel::Channel;
use mm_model::channel_join_request::{
    CHANNEL_JOIN_REQUEST_STATUS_APPROVED, CHANNEL_JOIN_REQUEST_STATUS_DENIED,
    CHANNEL_JOIN_REQUEST_STATUS_PENDING, CHANNEL_JOIN_REQUEST_STATUS_WITHDRAWN, ChannelJoinRequest,
    ChannelJoinRequestList, ChannelJoinRequestPatch, GetChannelJoinRequestsOpts,
    is_valid_channel_join_request_status,
};
use mm_model::channel_member::ChannelMember;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_CREATED, WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_UPDATED,
    WebSocketEvent,
};
use mm_store::channel_join_request_store::ChannelJoinRequestStore;
use mm_store::channel_store::ChannelStore;

use crate::App;
use crate::channel_member::{ChannelMemberOpts, MemberWrite};

/// `channelJoinRequestPaginationDefaultPerPage` (channel_join_request.go:18) — the public
/// `/api/v4` default, and **not** the same constant the store falls back to even though both are
/// 60.
const PAGINATION_DEFAULT_PER_PAGE: i64 = 60;

/// `channelJoinRequestPaginationMaxPerPage` (channel_join_request.go:22).
const PAGINATION_MAX_PER_PAGE: i64 = 200;

/// `channelMembersPageSize` inside `channelAdminUserIDs` (channel_join_request.go:378).
const CHANNEL_MEMBERS_PAGE_SIZE: i64 = 200;

/// `broadcastOnlyChannelAdmins` (app/web_broadcast_hooks.go:28).
const BROADCAST_ONLY_CHANNEL_ADMINS: &str = "only_channel_admins";

/// Port of `sanitizeJoinRequestListOpts` (channel_join_request.go:270).
///
/// Four clamps, and the order of the first two matters: an **unrecognised** status is silently
/// rewritten to `pending` rather than refused, so `?status=bogus` lists pending requests instead
/// of answering 400. Measured.
fn sanitize_list_opts(mut opts: GetChannelJoinRequestsOpts) -> GetChannelJoinRequestsOpts {
    if opts.status.is_empty() || !is_valid_channel_join_request_status(&opts.status) {
        opts.status = CHANNEL_JOIN_REQUEST_STATUS_PENDING.to_owned();
    }
    if opts.page < 0 {
        opts.page = 0;
    }
    if opts.per_page <= 0 {
        opts.per_page = PAGINATION_DEFAULT_PER_PAGE;
    } else if opts.per_page > PAGINATION_MAX_PER_PAGE {
        opts.per_page = PAGINATION_MAX_PER_PAGE;
    }
    opts
}

/// Port of `requestJoinChannelGuard` (channel_join_request.go:29) — the seven refusals, in Go's
/// order.
///
/// The order is the whole content: an archived **public** channel answers `archived` and not
/// `not_private`, and a guest asking about a non-discoverable channel is told
/// `not_discoverable` — the channel's state is fully examined before the user's. Two of the seven
/// are 403 (`not_discoverable`, `guest`, `deleted_user`) and the rest 400, so a reordering shows
/// up as both a different id and a different status.
///
/// Go's `channel == nil` arm is unreachable through any caller: `GetChannel` has already answered
/// 404 with the same id. Rust makes it unrepresentable, which is why there is no branch for it.
fn request_join_channel_guard(user: &User, channel: &Channel) -> AppResult {
    if channel.delete_at != 0 {
        return Err(guard_error(
            "api.channel.discoverable_join_request.archived.app_error",
            format!("channel_id={}", channel.id),
            400,
        ));
    }

    if channel.channel_type != mm_model::channel::CHANNEL_TYPE_PRIVATE {
        return Err(guard_error(
            "api.channel.discoverable_join_request.not_private.app_error",
            format!("channel_id={}", channel.id),
            400,
        ));
    }

    if !channel.discoverable {
        return Err(guard_error(
            "api.channel.discoverable_join_request.not_discoverable.app_error",
            format!("channel_id={}", channel.id),
            403,
        ));
    }

    // Shared channels join through their own remote-cluster sync mechanism.
    if channel.is_shared() {
        return Err(guard_error(
            "api.channel.discoverable_join_request.shared.app_error",
            format!("channel_id={}", channel.id),
            400,
        ));
    }

    if user.is_guest() {
        return Err(guard_error(
            "api.channel.discoverable_join_request.guest.app_error",
            format!("user_id={}", user.id),
            403,
        ));
    }

    if user.delete_at != 0 {
        // Note the id: the guard borrows `AddChannelMember`'s, not one of its own.
        return Err(guard_error(
            "app.channel.add_member.deleted_user.app_error",
            String::new(),
            403,
        ));
    }

    Ok(())
}

fn guard_error(id: &'static str, details: String, status: i32) -> Box<AppError> {
    AppError::boxed("RequestJoinChannel", id, None, details, status)
}

/// What `RequestJoinChannel` decided, mirroring Go's `(joined bool, req *ChannelJoinRequest)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinRequestOutcome {
    /// The ABAC fast path added the user directly; Go returns `joined = true, req = nil`.
    Joined,
    /// A pending row, either freshly saved or the caller's existing one.
    Pending(Box<ChannelJoinRequest>),
}

impl App {
    /// Port of `app.App.ChannelAccessControlled` (app/channel.go:4522), pinned to this
    /// deployment.
    ///
    /// Go's gate is `MinimumEnterpriseAdvancedLicense(License())` **and**
    /// `AccessControlSettings.EnableAttributeBasedAccessControl` **and** the channel's hydrated
    /// membership policy action. The licence term is `false` here — `docker-compose.yml` pins
    /// `mattermost-team-edition` and `Licenses` holds no rows — and it short-circuits the other
    /// two, so the whole ABAC fast path in [`App::request_join_channel`] is dark.
    ///
    /// Point this at a real licence read before running against an Enterprise Advanced server.
    /// Until then the honest statement is that the ABAC arm is read from the Go source and not
    /// exercised by anything.
    pub fn channel_access_controlled(&self) -> bool {
        false
    }

    /// Port of `app.App.RequestJoinChannel` (channel_join_request.go:66).
    ///
    /// The order decides which error a bad request gets, and every step can refuse:
    ///
    /// 1. `GetUser` on the **session** user — a 404 here is not reachable through the API.
    /// 2. `GetChannel` — `app.channel.get.existing.app_error` at 404.
    /// 3. [`request_join_channel_guard`].
    /// 4. **The membership read**, which refuses an existing member with `already_member` at 400.
    ///    A store failure that is not a miss becomes `app.channel.get_member.app_error` at 500 —
    ///    a *different* id from the guard's, and the only 500 on this path.
    /// 5. The ABAC gate, dark here — see [`App::channel_access_controlled`].
    /// 6. `Save`, whose conflict is a success.
    #[tracing::instrument(skip(self, message), fields(user_id = %user_id, channel_id = %channel_id))]
    pub async fn request_join_channel(
        &self,
        user_id: &str,
        channel_id: &str,
        message: &str,
    ) -> AppResult<JoinRequestOutcome> {
        let user = self.get_user(user_id).await?;
        let channel = self.get_channel(channel_id).await?;

        request_join_channel_guard(&user, &channel)?;

        match self
            .store()
            .channel()
            .get_member(&channel.id, &user.id)
            .await
        {
            Ok(_) => {
                return Err(guard_error(
                    "api.channel.discoverable_join_request.already_member.app_error",
                    format!("channel_id={}", channel.id),
                    400,
                ));
            }
            Err(err) if err.is_not_found() => {}
            Err(err) => {
                tracing::error!(error = %err, "channel member lookup failed");
                return Err(AppError::boxed(
                    "RequestJoinChannel",
                    "app.channel.get_member.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        // `enforced` is a constant `false` on this deployment, so the ABAC arm below it — the
        // policy evaluation, its `policy_denied` 403 and the direct `AddChannelMember` — is
        // unreachable. It is written out rather than omitted so the shape of what is owed when a
        // licence appears is visible at the call site.
        if self.channel_access_controlled() {
            return Ok(JoinRequestOutcome::Joined);
        }

        let mut pending = ChannelJoinRequest {
            channel_id: channel.id.clone(),
            user_id: user.id.clone(),
            message: message.to_owned(),
            ..Default::default()
        };

        match self.store().channel_join_request().save(&mut pending).await {
            Ok(()) => {}
            Err(mm_store::StoreError::Conflict { .. }) => {
                // The caller already has a pending row: Go answers 201 with **that** row, so the
                // id and the original message are the ones from the first request.
                return match self
                    .store()
                    .channel_join_request()
                    .get_pending_for_channel_and_user(&channel.id, &user.id)
                    .await
                {
                    Ok(existing) => Ok(JoinRequestOutcome::Pending(Box::new(existing))),
                    // Only reachable if the conflicting row vanished between the two statements.
                    Err(_) => Err(AppError::boxed(
                        "RequestJoinChannel",
                        "api.channel.discoverable_join_request.duplicate.app_error",
                        None,
                        format!("channel_id={}", channel.id),
                        409,
                    )),
                };
            }
            // `if appErr, ok := err.(*model.AppError); ok` — `IsValid`'s 400 survives untouched.
            Err(mm_store::StoreError::Invalid { app_error, .. }) => return Err(app_error),
            Err(err) => {
                tracing::error!(error = %err, "failed to save a channel join request");
                return Err(AppError::boxed(
                    "RequestJoinChannel",
                    "app.channel.join_request.save.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        self.broadcast_channel_join_request_created(&channel, &pending)
            .await;
        Ok(JoinRequestOutcome::Pending(Box::new(pending)))
    }

    /// Port of `app.App.GetMyChannelJoinRequest` (channel_join_request.go:186).
    ///
    /// **A miss is `Ok(None)`, never an error.** "No pending request" is the ordinary state, and
    /// the handler turns it into a bodiless 404 of its own.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    pub async fn get_my_channel_join_request(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> AppResult<Option<ChannelJoinRequest>> {
        match self
            .store()
            .channel_join_request()
            .get_pending_for_channel_and_user(channel_id, user_id)
            .await
        {
            Ok(req) => Ok(Some(req)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => {
                tracing::error!(error = %err, "join request lookup failed");
                Err(get_error("GetMyChannelJoinRequest"))
            }
        }
    }

    /// Port of `app.App.WithdrawChannelJoinRequest` (channel_join_request.go:140).
    ///
    /// # A non-owner gets the same 404 a missing row gets
    ///
    /// Deliberate in Go ("Hide the row from non-owners") — the two branches raise the identical
    /// id, details and status, so there is no existence oracle. Collapsing them would be correct;
    /// *distinguishing* them would be a leak.
    ///
    /// # The broadcast's channel read cannot fail the request
    ///
    /// `GetChannel` after the update only warns on failure and still returns the updated row.
    #[tracing::instrument(skip(self), fields(request_id = %request_id, user_id = %user_id))]
    pub async fn withdraw_channel_join_request(
        &self,
        request_id: &str,
        user_id: &str,
    ) -> AppResult<ChannelJoinRequest> {
        let mut current = match self.store().channel_join_request().get(request_id).await {
            Ok(req) => req,
            Err(err) if err.is_not_found() => {
                return Err(not_found_error("WithdrawChannelJoinRequest", request_id));
            }
            Err(err) => {
                tracing::error!(error = %err, "join request lookup failed");
                return Err(get_error("WithdrawChannelJoinRequest"));
            }
        };

        if current.user_id != user_id {
            return Err(not_found_error("WithdrawChannelJoinRequest", request_id));
        }

        if current.status != CHANNEL_JOIN_REQUEST_STATUS_PENDING {
            return Err(not_pending_error("WithdrawChannelJoinRequest", request_id));
        }

        current.status = CHANNEL_JOIN_REQUEST_STATUS_WITHDRAWN.to_owned();
        current.message = String::new();

        self.update_join_request("WithdrawChannelJoinRequest", &mut current)
            .await?;

        match self.get_channel(&current.channel_id).await {
            Ok(channel) => {
                self.broadcast_channel_join_request_updated(&channel, &current)
                    .await;
            }
            Err(err) => {
                // Channel went away mid-flight — still report the update.
                tracing::warn!(
                    channel_id = %current.channel_id,
                    error = %err,
                    "WithdrawChannelJoinRequest: failed to load channel for broadcast"
                );
            }
        }

        Ok(current)
    }

    /// Port of `app.App.GetMyChannelJoinRequests` (channel_join_request.go:203).
    ///
    /// The store's `nil` slice reaches the wire as **`"requests": null`**, not `[]` — there is no
    /// normalising step here, unlike `GetViewsForChannel`. Measured against the oracle.
    #[tracing::instrument(skip(self, opts), fields(user_id = %user_id))]
    pub async fn get_my_channel_join_requests(
        &self,
        user_id: &str,
        opts: GetChannelJoinRequestsOpts,
    ) -> AppResult<ChannelJoinRequestList> {
        let opts = sanitize_list_opts(opts);
        let (rows, total) = self
            .store()
            .channel_join_request()
            .get_for_user(user_id, &opts)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "join request list failed");
                get_error("GetMyChannelJoinRequests")
            })?;
        Ok(list(rows, total))
    }

    /// Port of `app.App.GetChannelJoinRequests` (channel_join_request.go:216).
    ///
    /// No permission check here — the api4 handler owns it, via
    /// `PermissionManageChannelJoinRequests`. So this lists a channel the caller cannot see if
    /// called directly.
    #[tracing::instrument(skip(self, opts), fields(channel_id = %channel_id))]
    pub async fn get_channel_join_requests(
        &self,
        channel_id: &str,
        opts: GetChannelJoinRequestsOpts,
    ) -> AppResult<ChannelJoinRequestList> {
        let opts = sanitize_list_opts(opts);
        let (rows, total) = self
            .store()
            .channel_join_request()
            .get_for_channel(channel_id, &opts)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "join request list failed");
                get_error("GetChannelJoinRequests")
            })?;
        Ok(list(rows, total))
    }

    /// Port of `app.App.CountPendingChannelJoinRequests` (channel_join_request.go:228).
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id))]
    pub async fn count_pending_channel_join_requests(&self, channel_id: &str) -> AppResult<i64> {
        self.store()
            .channel_join_request()
            .count_pending(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "join request count failed");
                get_error("CountPendingChannelJoinRequests")
            })
    }

    /// Port of `app.App.UpdateChannelJoinRequest` (channel_join_request.go:242) — the admin
    /// review.
    ///
    /// # Only `approved` and `denied` are accepted
    ///
    /// `pending` and `withdrawn` are valid *statuses* and invalid *patches*: both answer
    /// `…invalid_patch.app_error` at 400, the same as a garbage string. So
    /// [`is_valid_channel_join_request_status`] is the wrong predicate here and using it would
    /// accept two transitions Go refuses.
    ///
    /// # The membership write happens **before** the row is updated
    ///
    /// Go's comment says why: the PDP gate inside `addUserToChannel` re-runs, so an admin cannot
    /// approve past an active ABAC policy, and the audit row is only written once the add
    /// succeeded. A port that flipped the order would record an approval that never took effect.
    ///
    /// # `denial_reason` is cleared unconditionally and then re-set
    ///
    /// So approving a request that somehow carried a reason wipes it, and denying without a
    /// reason leaves it empty — which matters because `IsValid` refuses a non-empty reason on any
    /// status but `denied`.
    #[tracing::instrument(skip(self, patch), fields(request_id = %request_id, channel_id = %channel_id, reviewer_id = %reviewer_id))]
    pub async fn update_channel_join_request(
        &self,
        request_id: &str,
        channel_id: &str,
        patch: &ChannelJoinRequestPatch,
        reviewer_id: &str,
    ) -> AppResult<MemberWrite<ChannelJoinRequest>> {
        if patch.status != CHANNEL_JOIN_REQUEST_STATUS_APPROVED
            && patch.status != CHANNEL_JOIN_REQUEST_STATUS_DENIED
        {
            return Err(AppError::boxed(
                "UpdateChannelJoinRequest",
                "api.channel.discoverable_join_request.invalid_patch.app_error",
                None,
                format!("status={}", patch.status),
                400,
            ));
        }

        let mut current = match self.store().channel_join_request().get(request_id).await {
            Ok(req) => req,
            Err(err) if err.is_not_found() => {
                return Err(not_found_error("UpdateChannelJoinRequest", request_id));
            }
            Err(err) => {
                tracing::error!(error = %err, "join request lookup failed");
                return Err(get_error("UpdateChannelJoinRequest"));
            }
        };

        // Defense in depth: a forged request id cannot be reviewed against a channel the admin
        // happens to own. Note this is a 404 and not a 403 — the handler's permission check has
        // already passed for `channel_id`, so a mismatch must not confirm the row exists.
        if current.channel_id != channel_id {
            return Err(not_found_error("UpdateChannelJoinRequest", request_id));
        }

        if current.status != CHANNEL_JOIN_REQUEST_STATUS_PENDING {
            return Err(not_pending_error("UpdateChannelJoinRequest", request_id));
        }

        let channel = self.get_channel(&current.channel_id).await?;

        if patch.status == CHANNEL_JOIN_REQUEST_STATUS_APPROVED {
            let opts = ChannelMemberOpts {
                user_requestor_id: reviewer_id.to_owned(),
                ..Default::default()
            };
            match self
                .add_channel_member(&current.user_id, &channel, &opts)
                .await?
            {
                MemberWrite::Done(_) => {}
                // Nothing has been written yet — `add_channel_member`'s forwards all happen
                // before its insert — so handing the whole request to Go replays it cleanly.
                MemberWrite::Forward(why) => return Ok(MemberWrite::Forward(why)),
            }
        }

        current.status = patch.status.clone();
        current.reviewed_by = reviewer_id.to_owned();
        current.reviewed_at = get_millis();
        current.denial_reason = String::new();
        if patch.status == CHANNEL_JOIN_REQUEST_STATUS_DENIED
            && let Some(reason) = patch.denial_reason.as_deref()
        {
            current.denial_reason = reason.to_owned();
        }
        // Drop the original message from the response; it served its purpose during review.
        current.message = String::new();

        self.update_join_request("UpdateChannelJoinRequest", &mut current)
            .await?;

        self.broadcast_channel_join_request_updated(&channel, &current)
            .await;
        Ok(MemberWrite::Done(current))
    }

    /// The `Update` call and its two error shapes, shared by the withdraw and review paths —
    /// Go writes the same six lines twice with a different `where`.
    async fn update_join_request(
        &self,
        where_: &'static str,
        req: &mut ChannelJoinRequest,
    ) -> AppResult {
        match self.store().channel_join_request().update(req).await {
            Ok(()) => Ok(()),
            // `IsValid`'s 400 survives untouched, as it does on the save path.
            Err(mm_store::StoreError::Invalid { app_error, .. }) => Err(app_error),
            Err(err) => {
                tracing::error!(error = %err, "join request update failed");
                Err(AppError::boxed(
                    where_,
                    "app.channel.join_request.update.app_error",
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }

    /// Port of `app.App.channelAdminUserIDs` (channel_join_request.go:376).
    ///
    /// Pages `GetMembers` 200 at a time and stops on a **short page**, so a channel whose member
    /// count is an exact multiple of 200 costs one extra empty query — Go's loop, reproduced.
    async fn channel_admin_user_ids(&self, channel_id: &str) -> AppResult<Vec<String>> {
        let mut admins = Vec::new();
        let mut page: i64 = 0;
        loop {
            let members: Vec<ChannelMember> = self
                .store()
                .channel()
                .get_members(
                    channel_id,
                    page * CHANNEL_MEMBERS_PAGE_SIZE,
                    CHANNEL_MEMBERS_PAGE_SIZE,
                )
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "channel member page failed");
                    AppError::boxed(
                        "channelAdminUserIDs",
                        "app.channel.get_members.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;

            let short_page = (members.len() as i64) < CHANNEL_MEMBERS_PAGE_SIZE;
            admins.extend(
                members
                    .into_iter()
                    .filter(|m| m.scheme_admin)
                    .map(|m| m.user_id),
            );
            if short_page {
                break;
            }
            page += 1;
        }
        Ok(admins)
    }

    /// Port of `app.App.broadcastChannelJoinRequestCreated` (channel_join_request.go:405).
    async fn broadcast_channel_join_request_created(
        &self,
        channel: &Channel,
        req: &ChannelJoinRequest,
    ) {
        self.publish_channel_join_request_event(
            channel,
            req,
            WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_CREATED,
        )
        .await;
    }

    /// Port of `app.App.broadcastChannelJoinRequestUpdated` (channel_join_request.go:412).
    ///
    /// **Two events, not one.** The requester gets a user-addressed copy because they are not a
    /// channel member yet and the channel-scoped broadcast would never reach them; the admins get
    /// the channel-scoped one. The user copy is sent **first**, and it carries no broadcast hook.
    async fn broadcast_channel_join_request_updated(
        &self,
        channel: &Channel,
        req: &ChannelJoinRequest,
    ) {
        if !req.user_id.is_empty() {
            let mut message = WebSocketEvent::new(
                WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_UPDATED,
                "",
                "",
                &req.user_id,
                None,
                "",
            );
            message.add(
                "request",
                serde_json::Value::String(marshal_join_request(req)),
            );
            message.add("channel_id", serde_json::Value::String(channel.id.clone()));
            self.publish(message).await;
        }
        self.publish_channel_join_request_event(
            channel,
            req,
            WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_UPDATED,
        )
        .await;
    }

    /// Port of `app.App.publishChannelJoinRequestEvent` (channel_join_request.go:428).
    ///
    /// `adminsOnly` is `true` at both call sites, so it is not a parameter here. A failure to
    /// compute the admin set **drops the event entirely** rather than broadcasting it unfiltered —
    /// Go returns before `Publish`, and that is the fail-closed half of a check whose filtering
    /// half [D-340] records as missing.
    async fn publish_channel_join_request_event(
        &self,
        channel: &Channel,
        req: &ChannelJoinRequest,
        event: &'static str,
    ) {
        let mut message = WebSocketEvent::new(event, "", &channel.id, "", None, "");
        message.add(
            "request",
            serde_json::Value::String(marshal_join_request(req)),
        );
        message.add("channel_id", serde_json::Value::String(channel.id.clone()));

        let admins = match self.channel_admin_user_ids(&channel.id).await {
            Ok(admins) => admins,
            Err(err) => {
                tracing::warn!(
                    channel_id = %channel.id,
                    error = %err,
                    "Failed to compute channel admin set for join request broadcast"
                );
                return;
            }
        };

        if let Some(broadcast) = message.broadcast.as_mut() {
            broadcast.add_hook(
                BROADCAST_ONLY_CHANNEL_ADMINS,
                [(
                    "channel_admin_user_ids".to_owned(),
                    serde_json::Value::Array(
                        admins.into_iter().map(serde_json::Value::String).collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            );
        }

        self.publish(message).await;
    }
}

/// Port of `marshalChannelJoinRequest` (channel_join_request.go:452) — the request as a JSON
/// **string**, nested inside the event's JSON. A marshal failure delivers `""` so the event still
/// arrives; `ChannelJoinRequest` is ten scalars and cannot fail, which is why this is a `default`
/// rather than a fabricated error branch.
fn marshal_join_request(req: &ChannelJoinRequest) -> String {
    serde_json::to_string(req).unwrap_or_else(|err| {
        tracing::warn!(request_id = %req.id, error = %err, "Failed to marshal ChannelJoinRequest for WS broadcast");
        String::new()
    })
}

/// `&model.ChannelJoinRequestList{Requests: rows, TotalCount: total}` — and an **empty page is
/// `null`**, because Go's store returns a nil slice and nothing normalises it.
fn list(rows: Vec<ChannelJoinRequest>, total: i64) -> ChannelJoinRequestList {
    ChannelJoinRequestList {
        requests: if rows.is_empty() { None } else { Some(rows) },
        total_count: total,
    }
}

/// `app.channel.join_request.get.app_error` at 500 — raised by five of the six read paths with
/// only the `where` differing.
fn get_error(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.channel.join_request.get.app_error",
        None,
        String::new(),
        500,
    )
}

/// `app.channel.join_request.not_found.app_error` at 404.
fn not_found_error(where_: &'static str, request_id: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.channel.join_request.not_found.app_error",
        None,
        format!("request_id={request_id}"),
        404,
    )
}

/// `api.channel.discoverable_join_request.not_pending.app_error` at **409**, not 400 — a review or
/// a withdrawal of a row that has already reached a terminal state is a conflict.
fn not_pending_error(where_: &'static str, request_id: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.channel.discoverable_join_request.not_pending.app_error",
        None,
        format!("request_id={request_id}"),
        409,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(status: &str, page: i64, per_page: i64) -> GetChannelJoinRequestsOpts {
        GetChannelJoinRequestsOpts {
            status: status.to_owned(),
            page,
            per_page,
        }
    }

    fn channel() -> Channel {
        Channel {
            id: "1111111111111111111111111".to_owned() + "1",
            channel_type: mm_model::channel::CHANNEL_TYPE_PRIVATE.to_owned(),
            discoverable: true,
            ..Default::default()
        }
    }

    fn user() -> User {
        User {
            id: "2222222222222222222222222".to_owned() + "2",
            roles: "system_user".to_owned(),
            ..Default::default()
        }
    }

    fn refusal(user: &User, channel: &Channel) -> (String, i32) {
        let err = request_join_channel_guard(user, channel).expect_err("a refusal");
        (err.id.clone(), err.status_code)
    }

    #[test]
    fn the_guard_accepts_a_live_discoverable_private_channel() {
        assert!(request_join_channel_guard(&user(), &channel()).is_ok());
    }

    #[test]
    fn an_archived_channel_is_refused_before_its_type_is_looked_at() {
        // Public *and* archived: `archived` wins, which is what pins the order of the first two
        // arms. Swapping them answers `not_private` here and nothing else changes.
        let mut channel = channel();
        channel.delete_at = 17;
        channel.channel_type = mm_model::channel::CHANNEL_TYPE_OPEN.to_owned();
        assert_eq!(
            refusal(&user(), &channel),
            (
                "api.channel.discoverable_join_request.archived.app_error".to_owned(),
                400
            )
        );
    }

    #[test]
    fn a_public_channel_is_not_private_and_that_is_a_400() {
        let mut channel = channel();
        channel.channel_type = mm_model::channel::CHANNEL_TYPE_OPEN.to_owned();
        assert_eq!(
            refusal(&user(), &channel),
            (
                "api.channel.discoverable_join_request.not_private.app_error".to_owned(),
                400
            )
        );
    }

    #[test]
    fn a_private_channel_that_is_not_discoverable_is_a_403_and_not_a_400() {
        let mut channel = channel();
        channel.discoverable = false;
        assert_eq!(
            refusal(&user(), &channel),
            (
                "api.channel.discoverable_join_request.not_discoverable.app_error".to_owned(),
                403
            )
        );
    }

    #[test]
    fn a_shared_channel_is_refused_after_discoverability_and_before_the_user_is_examined() {
        let mut channel = channel();
        channel.shared = Some(true);
        let mut guest = user();
        guest.roles = "system_guest".to_owned();
        assert_eq!(
            refusal(&guest, &channel),
            (
                "api.channel.discoverable_join_request.shared.app_error".to_owned(),
                400
            )
        );
    }

    #[test]
    fn a_guest_is_refused_before_a_deactivated_account_is_noticed() {
        let mut guest = user();
        guest.roles = "system_guest".to_owned();
        guest.delete_at = 5;
        assert_eq!(
            refusal(&guest, &channel()),
            (
                "api.channel.discoverable_join_request.guest.app_error".to_owned(),
                403
            )
        );
    }

    #[test]
    fn a_deactivated_user_borrows_add_channel_members_error_id() {
        let mut deleted = user();
        deleted.delete_at = 5;
        assert_eq!(
            refusal(&deleted, &channel()),
            (
                "app.channel.add_member.deleted_user.app_error".to_owned(),
                403
            )
        );
    }

    #[test]
    fn an_unrecognised_status_is_rewritten_to_pending_rather_than_refused() {
        assert_eq!(sanitize_list_opts(opts("bogus", 0, 0)).status, "pending");
        assert_eq!(sanitize_list_opts(opts("", 0, 0)).status, "pending");
        // A recognised one survives — including the two the *patch* refuses.
        for status in ["pending", "approved", "denied", "withdrawn"] {
            assert_eq!(sanitize_list_opts(opts(status, 0, 0)).status, status);
        }
    }

    #[test]
    fn per_page_is_clamped_between_the_default_and_the_maximum() {
        assert_eq!(sanitize_list_opts(opts("", 0, 0)).per_page, 60);
        assert_eq!(sanitize_list_opts(opts("", 0, -1)).per_page, 60);
        assert_eq!(sanitize_list_opts(opts("", 0, 1)).per_page, 1);
        assert_eq!(sanitize_list_opts(opts("", 0, 200)).per_page, 200);
        assert_eq!(sanitize_list_opts(opts("", 0, 201)).per_page, 200);
    }

    #[test]
    fn a_negative_page_becomes_the_first_page() {
        assert_eq!(sanitize_list_opts(opts("", -3, 10)).page, 0);
        assert_eq!(sanitize_list_opts(opts("", 4, 10)).page, 4);
    }

    #[test]
    fn an_empty_page_is_null_and_not_an_empty_array() {
        let empty = list(Vec::new(), 0);
        assert_eq!(empty.requests, None);
        assert_eq!(
            serde_json::to_value(&empty).expect("encodes"),
            serde_json::json!({"requests": null, "total_count": 0})
        );
    }

    #[test]
    fn a_marshalled_request_is_a_json_string_and_not_an_object() {
        let req = ChannelJoinRequest {
            id: "abc".to_owned(),
            status: "pending".to_owned(),
            ..Default::default()
        };
        let encoded = marshal_join_request(&req);
        assert!(encoded.starts_with('{'), "{encoded}");
        let round: ChannelJoinRequest = serde_json::from_str(&encoded).expect("decodes");
        assert_eq!(round.id, "abc");
    }
}
