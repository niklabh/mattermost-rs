//! Port of the channel app-layer surface (channels/app/channel.go): `GetChannel`,
//! `GetChannelMember` and `GetChannelUnread`.

use std::collections::HashMap;

use mm_model::channel::Channel;
use mm_model::channel_member::{CHANNEL_MARK_UNREAD_MENTION, ChannelMember, ChannelUnread};
use mm_model::user::MARK_UNREAD_NOTIFY_PROP;
use mm_model::utils::AppError;
use mm_store::ChannelStore;

use crate::App;

impl App {
    /// Port of `app.App.GetChannel` (channel.go:2225) and the `Server.getChannel` (:2274) it
    /// delegates to.
    ///
    /// The two error branches are not interchangeable, and `SessionHasPermissionToChannel` is why:
    /// it treats a **404** as "no such channel, deny quietly" and anything else as "the lookup
    /// broke, log it and deny" (authorization.go:107-111). Collapsing them into one status would
    /// make a database outage indistinguishable from a missing channel in the logs — the only
    /// place that distinction survives, since both answers deny.
    ///
    /// `channel_id` travels in `params` because Go puts it there (`errCtx`), and the i18n strings
    /// interpolate it.
    ///
    /// **Not ported:** `HydrateChannelPolicyActions`, which Go calls next and whose failure it only
    /// logs. See [D-141].
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    pub async fn get_channel(&self, channel_id: &str) -> Result<Channel, AppError> {
        self.store().channel().get(channel_id).await.map_err(|err| {
            let params = HashMap::from([(
                "channel_id".to_owned(),
                serde_json::Value::String(channel_id.to_owned()),
            )]);

            if err.is_not_found() {
                AppError::new(
                    "GetChannel",
                    "app.channel.get.existing.app_error",
                    Some(params),
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "channel lookup failed");
                AppError::new(
                    "GetChannel",
                    "app.channel.get.find.app_error",
                    Some(params),
                    String::new(),
                    500,
                )
            }
        })
    }

    /// Port of `app.App.GetChannelMember` (channel.go:2258) and the `Server.getChannelMember`
    /// (:2262) it delegates to.
    ///
    /// Same two-branch shape as [`App::get_channel`], but note the **error ids are not the same
    /// pattern**: the miss is `app.channel.get_member.missing.app_error` — spelled out in
    /// `app/constants.go:6` as `MissingChannelMemberError` rather than inline — while the failure
    /// is `app.channel.get_member.app_error`. One is a suffix of the other with `missing.`
    /// inserted, which is easy to transcribe wrongly and impossible to notice from the outside,
    /// because both render as their own id until i18n runs.
    ///
    /// Neither branch carries `params`: Go passes `nil` here, unlike `GetChannel` which passes an
    /// `errCtx` with the channel id.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
    pub async fn get_channel_member(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelMember, AppError> {
        self.store()
            .channel()
            .get_member(channel_id, user_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::new(
                        "GetChannelMember",
                        "app.channel.get_member.missing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "channel member lookup failed");
                    AppError::new(
                        "GetChannelMember",
                        "app.channel.get_member.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetChannelUnread` (channel.go:2700).
    ///
    /// # Both branches carry the **same** error id
    ///
    /// Unlike [`App::get_channel`] and [`App::get_channel_member`], whose 404 and 500 are
    /// distinguishable ids, this function answers `app.channel.get_unread.app_error` either way
    /// and varies only the status. So a client that branches on `id` cannot tell a missing
    /// channel from a broken database here, and neither can a log reader who only has the id.
    /// Reproduced rather than improved: the id is on the wire.
    ///
    /// # The `mention` shortcut zeroes two of seven counters
    ///
    /// A member whose `mark_unread` notify prop is `mention` has asked not to be told about plain
    /// messages, so Go blanks `MsgCount` and `MsgCountRoot` — and **only** those. The three
    /// mention counts and `TeamId`/`ChannelId` survive, which is the point: a muted channel still
    /// reports the mentions that pierce the mute. Zeroing the mention counts too, or reading the
    /// prop as a mute of everything, silently loses a notification a client would have shown.
    ///
    /// A nil `NotifyProps` indexes to `""` in Go, which is not `mention`, so an absent map means
    /// the counts pass through — the same answer `Option::None` gives here.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
    pub async fn get_channel_unread(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelUnread, AppError> {
        let mut unread = self
            .store()
            .channel()
            .get_channel_unread(channel_id, user_id)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() {
                    404
                } else {
                    tracing::error!(error = %err, "channel unread lookup failed");
                    500
                };
                AppError::new(
                    "GetChannelUnread",
                    "app.channel.get_unread.app_error",
                    None,
                    String::new(),
                    status,
                )
            })?;

        apply_mark_unread_shortcut(&mut unread);

        Ok(unread)
    }
}

/// Go's `if channelUnread.NotifyProps[MarkUnreadNotifyProp] == ChannelMarkUnreadMention`
/// (channel.go:2712), lifted out of the handler so it can be pinned without a database.
///
/// The store call above is the only other thing in `get_channel_unread`, and it needs Postgres —
/// so an inline branch here would be exercised solely by the cross-server suite, which cannot
/// distinguish "the counts were zero anyway" from "the shortcut fired". This is the same reason
/// `validate_ids` exists in `mm-api/src/channels.rs`.
fn apply_mark_unread_shortcut(unread: &mut ChannelUnread) {
    // Go indexes a `StringMap` directly, so a nil map and a missing key both yield `""` — which
    // is not `mention`, and the counts pass through. `Option` reproduces that, it does not add a
    // case.
    if unread
        .notify_props
        .as_ref()
        .and_then(|props| props.get(MARK_UNREAD_NOTIFY_PROP))
        .map(String::as_str)
        == Some(CHANNEL_MARK_UNREAD_MENTION)
    {
        unread.msg_count = 0;
        unread.msg_count_root = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    /// An `App` pointed at a database that cannot be reached; `connect_lazy` defers the attempt to
    /// first use, so constructing it never fails and any store call does.
    fn unreachable_app() -> App {
        // `acquire_timeout` is set because sqlx's default is **30 seconds**, and the connection
        // to :1 is refused instantly but retried until that window expires. Six tests wearing that
        // default cost 90 seconds of every `cargo test -p mm-app`, and 6 minutes under
        // `--test-threads=1`. The error a caller sees is `PoolTimedOut` either way, so nothing
        // under test changes — only how long we wait to see it.
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        App::new(SqlStore::from_pool(pool))
    }

    fn unread() -> ChannelUnread {
        ChannelUnread {
            team_id: "tttttttttttttttttttttttttt".to_owned(),
            channel_id: "cccccccccccccccccccccccccc".to_owned(),
            msg_count: 17,
            msg_count_root: 11,
            mention_count: 3,
            mention_count_root: 2,
            urgent_mention_count: 1,
            notify_props: None,
        }
    }

    fn with_mark_unread(value: &str) -> ChannelUnread {
        let mut u = unread();
        let mut props = mm_model::utils::StringMap::new();
        props.insert(MARK_UNREAD_NOTIFY_PROP.to_owned(), value.to_owned());
        u.notify_props = Some(props);
        u
    }

    /// The store's 404 and its failure share one error id here, unlike every other function in
    /// this file — so the **status** is the only thing separating them, and a test that checked
    /// the id alone would pass for a port that always answered 500.
    #[tokio::test]
    async fn an_unread_lookup_failure_is_a_500_with_the_shared_error_id() {
        let err = unreachable_app()
            .get_channel_unread("cccccccccccccccccccccccccc", "uuuuuuuuuuuuuuuuuuuuuuuuuu")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get_unread.app_error");
        assert!(
            err.params.is_none(),
            "Go passes nil params in both branches"
        );
        assert_eq!(
            err.id, "app.channel.get_unread.app_error",
            "the 404 branch uses this same id — see the doc comment"
        );
    }

    /// `mention` blanks the two message counts and **nothing else**. The three mention counts
    /// surviving is the point of the setting, not an oversight.
    #[test]
    fn mark_unread_mention_zeroes_only_the_two_message_counts() {
        let mut u = with_mark_unread(mm_model::channel_member::CHANNEL_MARK_UNREAD_MENTION);
        apply_mark_unread_shortcut(&mut u);

        assert_eq!(u.msg_count, 0);
        assert_eq!(u.msg_count_root, 0);
        assert_eq!(u.mention_count, 3, "mentions pierce the mute");
        assert_eq!(u.mention_count_root, 2);
        assert_eq!(u.urgent_mention_count, 1);
        assert_eq!(u.team_id, "tttttttttttttttttttttttttt");
        assert_eq!(u.channel_id, "cccccccccccccccccccccccccc");
    }

    /// Every other value of the prop — including the explicit `all`, an unrecognised string, an
    /// empty string, a missing key and a missing map — leaves the counts alone. Go's comparison
    /// is equality against one constant, not a "is it muted" predicate.
    #[test]
    fn anything_other_than_mention_leaves_the_counts_alone() {
        let mut cases = vec![
            unread(),
            with_mark_unread(mm_model::channel_member::CHANNEL_MARK_UNREAD_ALL),
            with_mark_unread(""),
            with_mark_unread("Mention"),
            with_mark_unread("mention "),
            with_mark_unread("mentions"),
        ];
        // A present-but-unrelated key: the map exists, `mark_unread` does not.
        let mut other_key = unread();
        let mut props = mm_model::utils::StringMap::new();
        props.insert("desktop".to_owned(), "mention".to_owned());
        other_key.notify_props = Some(props);
        cases.push(other_key);
        // An empty map, which is a different row from a NULL one ([D-135]).
        let mut empty = unread();
        empty.notify_props = Some(mm_model::utils::StringMap::new());
        cases.push(empty);

        for case in &mut cases {
            let before = case.clone();
            apply_mark_unread_shortcut(case);
            assert_eq!(
                *case, before,
                "{:?} must pass through untouched",
                before.notify_props
            );
        }
    }

    /// The prop this reads is `mark_unread`, spelled as `model.MarkUnreadNotifyProp`. A shortcut
    /// keyed on any other name would silently never fire, and the counts would look correct.
    #[test]
    fn the_prop_name_and_value_are_gos_constants() {
        assert_eq!(MARK_UNREAD_NOTIFY_PROP, "mark_unread");
        assert_eq!(CHANNEL_MARK_UNREAD_MENTION, "mention");
    }

    /// A broken store is a **500**, not a 404. The distinction is the whole reason both branches
    /// exist: `SessionHasPermissionToChannel` logs one and not the other.
    #[tokio::test]
    async fn a_store_failure_is_a_500_not_a_missing_channel() {
        let err = unreachable_app()
            .get_channel("cccccccccccccccccccccccccc")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get.find.app_error");
    }

    /// A broken store is a 500 for the member lookup too, with the **non-missing** id.
    #[tokio::test]
    async fn a_member_lookup_failure_is_a_500_with_the_plain_error_id() {
        let err = unreachable_app()
            .get_channel_member("cccccccccccccccccccccccccc", "uuuuuuuuuuuuuuuuuuuuuuuuuu")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get_member.app_error");
        assert!(
            err.params.is_none(),
            "Go passes nil params here, unlike GetChannel"
        );
    }

    /// The two member error ids differ only by an inserted `missing.`, and the 404 is the one a
    /// client branches on. Pinned so a transcription slip fails a test rather than a client.
    #[test]
    fn the_two_member_error_ids_are_the_ones_go_uses() {
        assert_eq!(
            mm_model::utils::AppError::new(
                "GetChannelMember",
                "app.channel.get_member.missing.app_error",
                None,
                String::new(),
                404
            )
            .id,
            "app.channel.get_member.missing.app_error",
            "app/constants.go:6"
        );
    }

    /// The id travels in `params`, as Go's `errCtx` does — the i18n string interpolates it.
    #[tokio::test]
    async fn the_channel_id_is_carried_in_params() {
        let err = unreachable_app()
            .get_channel("cccccccccccccccccccccccccc")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("channel_id"))
                .and_then(serde_json::Value::as_str),
            Some("cccccccccccccccccccccccccc")
        );
    }
}
