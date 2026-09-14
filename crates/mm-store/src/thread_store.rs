//! Port of the read side of `SqlThreadStore` (channels/store/sqlstore/thread_store.go) that
//! `GET /users/{user_id}/teams/{team_id}/threads` reaches.
//!
//! # `Threads` has no row until the first reply
//!
//! Every query here joins **from** `Threads`, never to it, because a root post nobody has
//! replied to has no metadata row at all. That is also why the counters join
//! `ThreadMemberships` first and left-join `Threads`: a membership can outlive its thread row.
//!
//! # One predicate is repeated in all five queries, and it is the access check
//!
//! `channelMembershipPredicate` (thread_store.go:76) is
//! `ThreadTeamId = '' OR EXISTS (a ChannelMembers row for this user and this thread's channel)`.
//! The first arm is what lets a DM or GM through — those have no team — and the `EXISTS` is the
//! only thing stopping a user from seeing threads in channels they have left. It is repeated
//! rather than factored in Go, and repeated here for the same reason: sqlx needs one literal
//! statement per query.

use mm_model::thread::{Thread, ThreadMembership, ThreadResponse};
use mm_model::user::User;
use sqlx::PgPool;

use crate::error::StoreError;
use crate::post_store::{PostRow, post_from_row};

/// Port of `store.ThreadStore`, narrowed to the threads-list route.
pub trait ThreadStore {
    /// Port of `SqlThreadStore.UpdateTeamIdForChannelThreads` (thread_store.go:1148): every
    /// thread of the channel takes the new `ThreadTeamId` — provided the team exists, which the
    /// `EXISTS` guards so a bad id updates nothing rather than orphaning the threads.
    fn update_team_id_for_channel_threads(
        &self,
        channel_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlThreadStore.GetThreadsForUser` (thread_store.go:283), for the option set the
    /// api4 handler serves: a team, no cursor, not deleted, not unread-only.
    fn get_threads_for_user(
        &self,
        user_id: &str,
        team_id: &str,
        page_size: i64,
        include_is_urgent: bool,
    ) -> impl std::future::Future<Output = Result<Vec<ThreadResponse>, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetMembershipForUser` (thread_store.go:797).
    fn get_membership_for_user(
        &self,
        user_id: &str,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<ThreadMembership, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetThreadForUser` (thread_store.go:562).
    fn get_thread_for_user(
        &self,
        membership: &ThreadMembership,
        include_is_urgent: bool,
    ) -> impl std::future::Future<Output = Result<ThreadResponse, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetTotalThreads` (thread_store.go:184).
    fn get_total_threads(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetTotalUnreadThreads` (thread_store.go:169).
    fn get_total_unread_threads(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetTotalUnreadMentions` (thread_store.go:202).
    fn get_total_unread_mentions(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetTotalUnreadUrgentMentions` (thread_store.go:242).
    fn get_total_unread_urgent_mentions(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlThreadStore.MarkAllAsReadByChannels` (thread_store.go:634).
    fn mark_all_as_read_by_channels(
        &self,
        user_id: &str,
        channel_ids: &[String],
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlThreadStore.DeleteMembershipsForChannel` (thread_store.go:731).
    ///
    /// Called from `removeChannelMembership` (app/channel.go:2989) on every channel leave: a user
    /// who is no longer in a channel must stop following its threads, or their threads list keeps
    /// returning replies from a channel they cannot read.
    fn delete_memberships_for_channel(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlThreadStore.MarkAllAsReadByTeam` (thread_store.go:678).
    fn mark_all_as_read_by_team(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlThreadStore.MaintainMembership` (thread_store.go:851).
    fn maintain_membership(
        &self,
        user_id: &str,
        post_id: &str,
        opts: ThreadMembershipOpts,
    ) -> impl std::future::Future<Output = Result<ThreadMembership, StoreError>> + Send;

    /// Port of `SqlThreadStore.GetThreadFollowers` (thread_store.go:513): the user ids with a
    /// `ThreadMemberships` row on the thread, restricted to `Following = true` when
    /// `following_only` (Go's `fetchOnlyActive`).
    ///
    /// **Unordered.** Go's select has no `ORDER BY`, and neither does this one; the caller
    /// (`SendNotifications`) treats the result as a set. A missing thread is an empty list, not
    /// `NotFound`.
    fn get_thread_followers(
        &self,
        thread_id: &str,
        following_only: bool,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// Port of `SqlThreadStore.Get` (thread_store.go:119).
    fn get(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Thread>, StoreError>> + Send;

    /// Port of `SqlThreadStore.MarkAsRead` (thread_store.go:698).
    ///
    /// Despite the name it moves the read mark to **`timestamp`**, wherever that is — the
    /// set-unread route calls it with a reply's `CreateAt - 1` to move the mark backwards. It
    /// writes `LastViewed` and `LastUpdated` only; `UnreadMentions` is the caller's, through
    /// [`ThreadStore::update_membership`] one statement earlier.
    fn mark_as_read(
        &self,
        user_id: &str,
        thread_id: &str,
        timestamp: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlThreadStore.GetThreadUnreadReplyCount` (thread_store.go:1055): the undeleted
    /// replies created **strictly after** the membership's `LastViewed`.
    fn get_thread_unread_reply_count(
        &self,
        membership: &ThreadMembership,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlThreadStore.UpdateMembership` (thread_store.go:727): rewrites the four mutable
    /// columns of an existing row from the value given. Go hands the same pointer back; there is
    /// nothing in it the caller did not put there, so this returns `()`.
    fn update_membership(
        &self,
        membership: &ThreadMembership,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Port of `store.ThreadMembershipOpts` (store/store.go:1368), without `ImportData`.
///
/// **`ImportData` is deliberately absent.** It exists only for the bulk importer, which is not a
/// route, and it is the one option that reaches `updateThreadParticipantsForUserTx` on the
/// *existing-row* branch of `maintainMembershipTx`. Without it the participants write happens on
/// the insert branch alone — see [`ThreadStore::maintain_membership`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadMembershipOpts {
    /// The state `UpdateFollowing` moves the row to. Also the `Following` an inserted row gets,
    /// **whether or not `update_following` is set** — Go reads it unconditionally on the insert
    /// path (thread_store.go:962).
    pub following: bool,
    /// `+1` to `UnreadMentions` on an existing row, or `1` on a new one. Loses to
    /// `update_viewed_timestamp`, which zeroes the same column.
    pub increment_mentions: bool,
    /// Gates the `following` assignment on an **existing** row, and only when the value actually
    /// changes. Not a gate on the insert path.
    pub update_following: bool,
    /// Moves `LastViewed` to now and zeroes `UnreadMentions`. The follow route sets this to the
    /// same value as `following`, so following a thread marks it read and unfollowing does not.
    pub update_viewed_timestamp: bool,
    /// Go's `UpdateParticipants`: append the user to `Threads.Participants` when the membership
    /// row is **inserted**. Ignored on an existing row — Go reads it only after `saveMembership`
    /// (thread_store.go:986), so a follower who already has a row is never added to the list by
    /// this flag, even if the list does not name them. `SendNotifications` sets it for the
    /// poster of a reply; the follow and unfollow routes leave it `false`.
    pub update_participants: bool,
}

#[derive(Debug, Clone)]
pub struct SqlThreadStore {
    pool: PgPool,
}

impl SqlThreadStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `GetThreadsForUser`'s select: the thread columns, the caller's membership columns,
/// two computed columns, and the root post coalesced into the same eighteen columns every other
/// post query in this crate selects.
struct JoinedThreadRow {
    postid: String,
    replycount: i64,
    lastreplyat: i64,
    participants: Option<serde_json::Value>,
    threaddeleteat: i64,
    lastviewedat: i64,
    unreadmentions: i64,
    unreadreplies: i64,
    isurgent: bool,
    // The root post, in `PostRow`'s column order.
    p_id: String,
    p_createat: i64,
    p_updateat: i64,
    p_editat: i64,
    p_deleteat: i64,
    p_ispinned: bool,
    p_userid: String,
    p_channelid: String,
    p_rootid: String,
    p_originalid: String,
    p_message: String,
    p_type: String,
    p_props: Option<serde_json::Value>,
    p_hashtags: String,
    p_filenames: Option<String>,
    p_fileids: Option<String>,
    p_hasreactions: bool,
    p_remoteid: Option<String>,
}

impl JoinedThreadRow {
    /// Port of `(*JoinedThread).toThreadResponse` (thread_store.go:36), minus the participant
    /// resolution the caller does.
    ///
    /// `Post.ReplyCount`, `LastReplyAt` and `Participants` are **not** selected — this query
    /// carries no reply-count subquery — so the embedded post reports `reply_count: 0` and
    /// `participants: null` however many replies the thread has. Measured on the running server;
    /// the thread's *own* `reply_count` beside it is the real number.
    fn into_response(self, participants: Option<Vec<User>>) -> Result<ThreadResponse, StoreError> {
        let post = post_from_row(PostRow {
            id: self.p_id,
            create_at: self.p_createat,
            update_at: self.p_updateat,
            edit_at: self.p_editat,
            delete_at: self.p_deleteat,
            is_pinned: self.p_ispinned,
            user_id: self.p_userid,
            channel_id: self.p_channelid,
            root_id: self.p_rootid,
            original_id: self.p_originalid,
            message: self.p_message,
            post_type: self.p_type,
            props: self.p_props,
            hashtags: self.p_hashtags,
            filenames: self.p_filenames,
            file_ids: self.p_fileids,
            has_reactions: self.p_hasreactions,
            remote_id: self.p_remoteid,
            reply_count: 0,
        })?;

        Ok(ThreadResponse {
            post_id: self.postid,
            reply_count: self.replycount,
            last_reply_at: self.lastreplyat,
            last_viewed_at: self.lastviewedat,
            participants,
            // `ToNilIfInvalid` — a post whose id is empty becomes `null`. The join is an inner
            // one, so this cannot fire here; kept because it is Go's and because a future
            // left-joined caller would need it.
            post: (!post.id.is_empty()).then(|| Box::new(post)),
            unread_replies: self.unreadreplies,
            unread_mentions: self.unreadmentions,
            is_urgent: self.isurgent,
            delete_at: self.threaddeleteat,
        })
    }

    /// The `Threads.Participants` jsonb column, which holds an array of user ids.
    fn participant_ids(&self) -> Vec<String> {
        self.participants
            .as_ref()
            .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
            .unwrap_or_default()
    }
}

impl ThreadStore for SqlThreadStore {
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, team_id = %team_id))]
    async fn update_team_id_for_channel_threads(
        &self,
        channel_id: &str,
        team_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE threads
               SET threadteamid = $2
             WHERE channelid = $1
               AND EXISTS (SELECT 1 FROM teams WHERE id = $2)
            "#,
            channel_id,
            team_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update threads team id for channel id={channel_id}"),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(
        skip(self),
        fields(user_id = %user_id, team_id = %team_id, page_size, found)
    )]
    async fn get_threads_for_user(
        &self,
        user_id: &str,
        team_id: &str,
        page_size: i64,
        include_is_urgent: bool,
    ) -> Result<Vec<ThreadResponse>, StoreError> {
        let rows = sqlx::query_as!(
            JoinedThreadRow,
            r#"
            SELECT t.postid                          AS "postid!",
                   COALESCE(t.replycount, 0)         AS "replycount!",
                   COALESCE(t.lastreplyat, 0)        AS "lastreplyat!",
                   t.participants                    AS "participants?",
                   COALESCE(t.threaddeleteat, 0)     AS "threaddeleteat!",
                   COALESCE(tm.lastviewed, 0)        AS "lastviewedat!",
                   COALESCE(tm.unreadmentions, 0)    AS "unreadmentions!",
                   (SELECT COUNT(r.id)
                      FROM posts r
                     WHERE r.rootid = tm.postid
                       AND r.createat > tm.lastviewed
                       AND r.deleteat = 0)           AS "unreadreplies!",
                   -- `pp` is left-joined, so `pp.priority = 'urgent'` is **NULL** for a thread
                   -- with no priority row and `TRUE AND NULL` is NULL, not false. Go's
                   -- `sq.Case().When(...).Else("false")` returns the literal `false` there; the
                   -- COALESCE is that `Else`.
                   COALESCE($3 AND pp.priority = 'urgent', FALSE) AS "isurgent!",
                   COALESCE(p.id, '')                AS "p_id!",
                   COALESCE(p.createat, 0)           AS "p_createat!",
                   COALESCE(p.updateat, 0)           AS "p_updateat!",
                   COALESCE(p.editat, 0)             AS "p_editat!",
                   COALESCE(p.deleteat, 0)           AS "p_deleteat!",
                   COALESCE(p.ispinned, false)       AS "p_ispinned!",
                   COALESCE(p.userid, '')            AS "p_userid!",
                   COALESCE(p.channelid, '')         AS "p_channelid!",
                   COALESCE(p.rootid, '')            AS "p_rootid!",
                   COALESCE(p.originalid, '')        AS "p_originalid!",
                   COALESCE(p.message, '')           AS "p_message!",
                   COALESCE(p.type, '')              AS "p_type!",
                   p.props                           AS "p_props?",
                   COALESCE(p.hashtags, '')          AS "p_hashtags!",
                   p.filenames                       AS "p_filenames?",
                   p.fileids                         AS "p_fileids?",
                   COALESCE(p.hasreactions, false)   AS "p_hasreactions!",
                   p.remoteid                        AS "p_remoteid?"
              FROM threads t
              JOIN posts p ON p.id = t.postid
              JOIN threadmemberships tm ON tm.postid = t.postid
              LEFT JOIN postspriority pp ON pp.postid = t.postid
             WHERE tm.userid = $1
               AND tm.following = TRUE
               AND (COALESCE(t.threadteamid, '') = ''
                    OR EXISTS (SELECT 1
                                 FROM channelmembers cm
                                WHERE cm.channelid = t.channelid
                                  AND cm.userid = tm.userid))
               AND (COALESCE(t.threadteamid, '') = $2
                    OR COALESCE(t.threadteamid, '') = '')
               AND COALESCE(t.threaddeleteat, 0) = 0
             ORDER BY t.lastreplyat DESC
             LIMIT $4
            "#,
            user_id,
            team_id,
            include_is_urgent,
            page_size,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to fetch threads for user id={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

        // Go resolves participants for the whole page at once — one id set across every thread,
        // de-duplicated — and only then maps each row. Reproduced, because the extended variant
        // is a single `GetProfileByIds` and doing it per row would be N queries.
        let mut ids: Vec<String> = rows.iter().flat_map(|row| row.participant_ids()).collect();
        mm_model::utils::remove_duplicate_strings(&mut ids);

        let stubs: std::collections::HashMap<String, User> = ids
            .into_iter()
            .map(|id| {
                (
                    id.clone(),
                    User {
                        id,
                        ..User::default()
                    },
                )
            })
            .collect();

        rows.into_iter()
            .map(|row| {
                let participants = row
                    .participant_ids()
                    .into_iter()
                    .filter_map(|id| stubs.get(&id).cloned())
                    .collect();
                row.into_response(Some(participants))
            })
            .collect()
    }

    /// Port of `SqlThreadStore.GetMembershipForUser` (thread_store.go:797).
    ///
    /// Six columns and one row. `sql.ErrNoRows` is `ErrNotFound`, which the app layer turns into
    /// a 404 — the only outcome the api4 route can produce for a thread nobody follows.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, post_id = %post_id, found))]
    async fn get_membership_for_user(
        &self,
        user_id: &str,
        post_id: &str,
    ) -> Result<ThreadMembership, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT postid                        AS "post_id!",
                   userid                        AS "user_id!",
                   COALESCE(following, FALSE)    AS "following!",
                   COALESCE(lastviewed, 0)       AS "last_viewed!",
                   COALESCE(lastupdated, 0)      AS "last_updated!",
                   COALESCE(unreadmentions, 0)   AS "unread_mentions!"
              FROM threadmemberships
             WHERE userid = $1
               AND postid = $2
            "#,
            user_id,
            post_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get thread membership with userid={user_id}"),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            return Err(StoreError::NotFound {
                entity: "ThreadMembership",
                criteria: format!("userId={user_id}, postId={post_id}"),
            });
        };
        tracing::Span::current().record("found", true);

        Ok(ThreadMembership {
            post_id: row.post_id,
            user_id: row.user_id,
            following: row.following,
            last_updated: row.last_updated,
            last_viewed: row.last_viewed,
            unread_mentions: row.unread_mentions,
        })
    }

    /// Port of `SqlThreadStore.GetThreadForUser` (thread_store.go:562).
    ///
    /// # It refuses an unfollowed thread **before touching the database**
    ///
    /// `if !threadMembership.Following { return ErrNotFound }` is the first line. The caller has
    /// already found the row, so this refusal is specifically *unfollowed*, and it reaches the
    /// wire as `app.user.get_threads_for_user.not_found` — a different id from the 404 a caller
    /// with no row at all gets. Deleting this check would serve unfollowed threads Go hides.
    ///
    /// # No membership or team predicate anywhere
    ///
    /// Unlike [`Self::get_threads_for_user`], this query filters on the root post id alone — the
    /// access check lives in the handler, as `SessionHasPermissionToReadPost`. Moving it here, or
    /// adding the list query's `channelMembershipPredicate` for symmetry, would refuse threads Go
    /// serves.
    ///
    /// # `LastViewedAt` and `UnreadMentions` come from the membership, not the join
    ///
    /// Go assigns them from the `ThreadMembership` argument after the query returns
    /// (thread_store.go:607). The unread-replies subquery binds that same `LastViewed` as a
    /// value rather than joining the row.
    ///
    /// # The post is `LEFT JOIN`ed, so `null` is reachable here
    ///
    /// The list query's join is inner; this one's is not. A `Threads` row can outlive its root
    /// post — the fixture database held 3,190 such rows before the sweep that closed [D-155] —
    /// and Go answers `"post": null` for one, through `ToNilIfInvalid`. The columns are coalesced
    /// so the empty id reaches that check rather than failing the scan.
    #[tracing::instrument(skip(self, membership), fields(post_id = %membership.post_id, found))]
    async fn get_thread_for_user(
        &self,
        membership: &ThreadMembership,
        include_is_urgent: bool,
    ) -> Result<ThreadResponse, StoreError> {
        if !membership.following {
            return Err(StoreError::NotFound {
                entity: "ThreadMembership",
                criteria: "<following>".to_owned(),
            });
        }

        let row = sqlx::query_as!(
            JoinedThreadRow,
            r#"
            SELECT t.postid                          AS "postid!",
                   COALESCE(t.replycount, 0)         AS "replycount!",
                   COALESCE(t.lastreplyat, 0)        AS "lastreplyat!",
                   t.participants                    AS "participants?",
                   COALESCE(t.threaddeleteat, 0)     AS "threaddeleteat!",
                   $2::bigint                        AS "lastviewedat!",
                   $3::bigint                        AS "unreadmentions!",
                   (SELECT COUNT(r.id)
                      FROM posts r
                     WHERE r.rootid = t.postid
                       AND r.createat > $2
                       AND r.deleteat = 0)           AS "unreadreplies!",
                   COALESCE($4 AND pp.priority = 'urgent', FALSE) AS "isurgent!",
                   COALESCE(p.id, '')                AS "p_id!",
                   COALESCE(p.createat, 0)           AS "p_createat!",
                   COALESCE(p.updateat, 0)           AS "p_updateat!",
                   COALESCE(p.editat, 0)             AS "p_editat!",
                   COALESCE(p.deleteat, 0)           AS "p_deleteat!",
                   COALESCE(p.ispinned, false)       AS "p_ispinned!",
                   COALESCE(p.userid, '')            AS "p_userid!",
                   COALESCE(p.channelid, '')         AS "p_channelid!",
                   COALESCE(p.rootid, '')            AS "p_rootid!",
                   COALESCE(p.originalid, '')        AS "p_originalid!",
                   COALESCE(p.message, '')           AS "p_message!",
                   COALESCE(p.type, '')              AS "p_type!",
                   p.props                           AS "p_props?",
                   COALESCE(p.hashtags, '')          AS "p_hashtags!",
                   p.filenames                       AS "p_filenames?",
                   p.fileids                         AS "p_fileids?",
                   COALESCE(p.hasreactions, false)   AS "p_hasreactions!",
                   p.remoteid                        AS "p_remoteid?"
              FROM threads t
              LEFT JOIN posts p ON p.id = t.postid
              LEFT JOIN postspriority pp ON pp.postid = t.postid
             WHERE t.postid = $1
            "#,
            membership.post_id,
            membership.last_viewed,
            membership.unread_mentions,
            include_is_urgent,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get thread for user id={}, post id={}",
                membership.user_id, membership.post_id
            ),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            return Err(StoreError::NotFound {
                entity: "Thread",
                criteria: membership.post_id.clone(),
            });
        };
        tracing::Span::current().record("found", true);

        let participants = row
            .participant_ids()
            .into_iter()
            .map(|id| User {
                id,
                ..User::default()
            })
            .collect();
        row.into_response(Some(participants))
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    async fn get_total_threads(&self, user_id: &str, team_id: &str) -> Result<i64, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT COUNT(tm.postid) AS "count!"
              FROM threadmemberships tm
              LEFT JOIN threads t ON t.postid = tm.postid
             WHERE tm.userid = $1
               AND tm.following = TRUE
               AND (COALESCE(t.threadteamid, '') = ''
                    OR EXISTS (SELECT 1
                                 FROM channelmembers cm
                                WHERE cm.channelid = t.channelid
                                  AND cm.userid = tm.userid))
               AND (COALESCE(t.threadteamid, '') = $2
                    OR COALESCE(t.threadteamid, '') = '')
               AND COALESCE(t.threaddeleteat, 0) = 0
            "#,
            user_id,
            team_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count threads for user id={user_id}"),
            source,
        })?;
        Ok(row.count)
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    async fn get_total_unread_threads(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<i64, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT COUNT(tm.postid) AS "count!"
              FROM threadmemberships tm
              LEFT JOIN threads t ON t.postid = tm.postid
             WHERE tm.userid = $1
               AND tm.following = TRUE
               AND (COALESCE(t.threadteamid, '') = ''
                    OR EXISTS (SELECT 1
                                 FROM channelmembers cm
                                WHERE cm.channelid = t.channelid
                                  AND cm.userid = tm.userid))
               AND (COALESCE(t.threadteamid, '') = $2
                    OR COALESCE(t.threadteamid, '') = '')
               AND COALESCE(t.threaddeleteat, 0) = 0
               AND tm.lastviewed < t.lastreplyat
            "#,
            user_id,
            team_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count unread threads for user id={user_id}"),
            source,
        })?;
        Ok(row.count)
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    async fn get_total_unread_mentions(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<i64, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(tm.unreadmentions), 0)::bigint AS "sum!"
              FROM threadmemberships tm
              LEFT JOIN threads t ON t.postid = tm.postid
             WHERE tm.userid = $1
               AND tm.following = TRUE
               AND (COALESCE(t.threadteamid, '') = ''
                    OR EXISTS (SELECT 1
                                 FROM channelmembers cm
                                WHERE cm.channelid = t.channelid
                                  AND cm.userid = tm.userid))
               AND (COALESCE(t.threadteamid, '') = $2
                    OR COALESCE(t.threadteamid, '') = '')
               AND COALESCE(t.threaddeleteat, 0) = 0
            "#,
            user_id,
            team_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count unread mentions for user id={user_id}"),
            source,
        })?;
        Ok(row.sum)
    }

    /// The one counter that **inner**-joins both `PostsPriority` and `Threads`, so a thread whose
    /// root has no priority row contributes nothing — and the deleted-thread filter is the same
    /// `COALESCE(ThreadDeleteAt, 0) = 0` the others use.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    async fn get_total_unread_urgent_mentions(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<i64, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(tm.unreadmentions), 0)::bigint AS "sum!"
              FROM threadmemberships tm
              JOIN postspriority pp ON pp.postid = tm.postid
              JOIN threads t ON t.postid = tm.postid
             WHERE tm.userid = $1
               AND tm.following = TRUE
               AND pp.priority = 'urgent'
               AND (COALESCE(t.threadteamid, '') = ''
                    OR EXISTS (SELECT 1
                                 FROM channelmembers cm
                                WHERE cm.channelid = t.channelid
                                  AND cm.userid = tm.userid))
               AND (COALESCE(t.threadteamid, '') = $2
                    OR COALESCE(t.threadteamid, '') = '')
               AND COALESCE(t.threaddeleteat, 0) = 0
            "#,
            user_id,
            team_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count urgent mentions for user id={user_id}"),
            source,
        })?;
        Ok(row.sum)
    }

    /// **Three predicates and every one of them bounds the write.**
    ///
    /// - `Threads.PostId = ThreadMemberships.PostId` is the join — written as a `WHERE` because
    ///   Go builds it with squirrel's `Update(...).From(...)`, and Postgres's `UPDATE … FROM`
    ///   has no `ON`. Dropping it updates every membership the user has against every thread.
    /// - `Threads.ChannelId = ANY(...)` is the scope: only threads rooted in these channels.
    /// - `Threads.LastReplyAt > ThreadMemberships.LastViewed` is what keeps the statement cheap
    ///   for the "mark a whole team read" callers, which pass **every** channel the user is in
    ///   rather than only the unread ones. It also means a membership already caught up is not
    ///   touched, so its `LastUpdated` does not move.
    ///
    /// `LastViewed` and `LastUpdated` are set from **one** `GetMillis()` call, so the two columns
    /// are equal for every row in a single call; reading the clock twice would let them differ.
    ///
    /// An empty `channel_ids` is a no-op with no statement sent, matching Go's early return.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channels = channel_ids.len()))]
    async fn mark_all_as_read_by_channels(
        &self,
        user_id: &str,
        channel_ids: &[String],
    ) -> Result<(), StoreError> {
        if channel_ids.is_empty() {
            return Ok(());
        }

        let now = mm_model::utils::get_millis();

        sqlx::query!(
            r#"
            UPDATE threadmemberships
               SET lastviewed = $1,
                   unreadmentions = 0,
                   lastupdated = $1
              FROM threads
             WHERE threadmemberships.userid = $2
               AND threads.postid = threadmemberships.postid
               AND threads.channelid = ANY($3)
               AND threads.lastreplyat > threadmemberships.lastviewed
            "#,
            now,
            user_id,
            channel_ids,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to mark all threads as read by channels for user id={user_id}"
            ),
            source,
        })?;

        Ok(())
    }

    /// The `EXISTS` subquery is the whole point: `ThreadMemberships` has no channel column, so the
    /// channel is reached through `Threads.PostId = ThreadMemberships.PostId`. Dropping it would
    /// delete **every** thread membership the user holds anywhere, which no test of the leave
    /// route's response body could see.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id, removed))]
    async fn delete_memberships_for_channel(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"
            DELETE FROM threadmemberships tm
             WHERE tm.userid = $1
               AND EXISTS (
                   SELECT 1
                     FROM threads t
                    WHERE t.postid = tm.postid
                      AND t.channelid = $2
               )
            "#,
            user_id,
            channel_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to remove thread memberships with userid={user_id} channelid={channel_id}"
            ),
            source,
        })?;

        tracing::Span::current().record("removed", result.rows_affected());
        Ok(())
    }

    /// **Not the same statement as [`Self::mark_all_as_read_by_channels`], and the differences
    /// are all widenings.** This one carries neither `Following = TRUE`, nor the channel
    /// membership `EXISTS`, nor `ThreadDeleteAt = 0`, nor `LastReplyAt > LastViewed`. So "mark
    /// the team read" touches memberships the threads *list* would never show the user: threads
    /// they unfollowed, threads in channels they have left, threads whose root is deleted, and
    /// threads already caught up — whose `LastUpdated` it therefore moves. Adding any of those
    /// predicates for symmetry would leave rows unread that Go reads.
    ///
    /// # `ThreadTeamId` is compared **without** `COALESCE`
    ///
    /// Go writes `sq.Or{sq.Eq{"Threads.ThreadTeamId": teamId}, sq.Eq{"Threads.ThreadTeamId": ""}}`
    /// — two plain equalities. The column is nullable, and `NULL = ''` is NULL, not true, so a
    /// thread with a NULL team is **skipped** here. Wrapping it in `COALESCE` to match the read
    /// queries in this file would mark DM threads read that Go leaves alone.
    ///
    /// One `GetMillis()` for both `LastViewed` and `LastUpdated`, as Go does — reading the clock
    /// twice would let the two columns differ within a single call.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, updated))]
    async fn mark_all_as_read_by_team(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<(), StoreError> {
        let now = mm_model::utils::get_millis();

        let result = sqlx::query!(
            r#"
            UPDATE threadmemberships
               SET lastviewed = $1,
                   unreadmentions = 0,
                   lastupdated = $1
              FROM threads
             WHERE threads.postid = threadmemberships.postid
               AND threadmemberships.userid = $2
               AND (threads.threadteamid = $3 OR threads.threadteamid = '')
            "#,
            now,
            user_id,
            team_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update thread read state for user id={user_id}"),
            source,
        })?;

        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }

    /// Port of `maintainMembershipTx` (thread_store.go:898) for the option sets a ported caller
    /// builds — see [`ThreadMembershipOpts`], which carries no `ImportData`.
    ///
    /// # `update_participants` appends, and only on the insert branch
    ///
    /// `updateThreadParticipantsForUserTx` (thread_store.go:1125) is one statement on Postgres:
    /// `SET Participants = Participants || '["<user>"]' WHERE PostId = ? AND NOT Participants ?
    /// '<user>'`. A user already in the list is **left where they are**, not moved to the end —
    /// there is no read-modify-write. A root post with no `Threads` row (nobody has replied yet)
    /// matches nothing and that is not an error; nor is a `NULL` list, which `||` leaves `NULL`.
    /// Go runs it after `saveMembership` and never on the update branch unless `ImportData` is
    /// set, so a re-follow with the flag does not repair a list the user is missing from.
    ///
    /// # The update branch is guarded, and an unfollow of an unfollowed thread writes nothing
    ///
    /// An existing row is rewritten only when `followingNeedsUpdate || IncrementMentions ||
    /// UpdateViewedTimestamp`. The unfollow route sets the last two to `false`, so unfollowing a
    /// thread that is already unfollowed leaves `LastUpdated` where it was — observable, because
    /// `LastUpdated` is what the websocket reconnect query and the retention policy read.
    ///
    /// # Following a thread you already follow still moves the read mark
    ///
    /// `followingNeedsUpdate` is false there, but `UpdateViewedTimestamp` is `state` — `true` for
    /// a follow — so `LastViewed` jumps to now and `UnreadMentions` is zeroed anyway. A `PUT
    /// …/following` is therefore not idempotent in its effect on unread state.
    ///
    /// # Unfollowing a thread with no row **creates** one
    ///
    /// The not-found path inserts unconditionally, with `Following = opts.Following` — `false`
    /// for an unfollow. The row it leaves behind changes what `GET …/threads/{id}` answers: a
    /// caller who had no row gets `app.user.get_thread_membership_for_user.not_found`, and after
    /// this insert gets the store's `app.user.get_threads_for_user.not_found` instead.
    ///
    /// # `UpdateViewedTimestamp` wins over `IncrementMentions` on an existing row, and not on a
    /// new one
    ///
    /// Go writes `if UpdateViewedTimestamp { … } else if IncrementMentions { … }` for the update
    /// and two independent `if`s for the insert. Neither ported route sets `increment_mentions`,
    /// so the asymmetry is unreachable today; it is reproduced rather than tidied because the
    /// post-create path that does set it is the next caller.
    ///
    /// The whole thing runs in one transaction, as Go's does: the read and the write must not be
    /// split by a concurrent follow, and the insert has **no** `ON CONFLICT` — a lost race is an
    /// error on both servers.
    #[tracing::instrument(
        skip(self),
        fields(user_id = %user_id, post_id = %post_id, existing, wrote, participants_updated)
    )]
    async fn maintain_membership(
        &self,
        user_id: &str,
        post_id: &str,
        opts: ThreadMembershipOpts,
    ) -> Result<ThreadMembership, StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        let existing = sqlx::query!(
            r#"
            SELECT postid                        AS "post_id!",
                   userid                        AS "user_id!",
                   COALESCE(following, FALSE)    AS "following!",
                   COALESCE(lastviewed, 0)       AS "last_viewed!",
                   COALESCE(lastupdated, 0)      AS "last_updated!",
                   COALESCE(unreadmentions, 0)   AS "unread_mentions!"
              FROM threadmemberships
             WHERE userid = $1
               AND postid = $2
            "#,
            user_id,
            post_id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get thread membership with userid={user_id}"),
            source,
        })?;

        let now = mm_model::utils::get_millis();

        let membership = match existing {
            Some(row) => {
                tracing::Span::current().record("existing", true);
                let mut membership = ThreadMembership {
                    post_id: row.post_id,
                    user_id: row.user_id,
                    following: row.following,
                    last_updated: row.last_updated,
                    last_viewed: row.last_viewed,
                    unread_mentions: row.unread_mentions,
                };

                let following_needs_update =
                    opts.update_following && membership.following != opts.following;
                let wrote = following_needs_update
                    || opts.increment_mentions
                    || opts.update_viewed_timestamp;
                tracing::Span::current().record("wrote", wrote);

                if wrote {
                    if following_needs_update {
                        membership.following = opts.following;
                    }
                    if opts.update_viewed_timestamp {
                        membership.last_viewed = now;
                        membership.unread_mentions = 0;
                    } else if opts.increment_mentions {
                        membership.unread_mentions += 1;
                    }
                    membership.last_updated = now;

                    sqlx::query!(
                        r#"
                        UPDATE threadmemberships
                           SET following = $1,
                               lastviewed = $2,
                               lastupdated = $3,
                               unreadmentions = $4
                         WHERE postid = $5
                           AND userid = $6
                        "#,
                        membership.following,
                        membership.last_viewed,
                        membership.last_updated,
                        membership.unread_mentions,
                        membership.post_id,
                        membership.user_id,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(|source| StoreError::Db {
                        context: format!(
                            "failed to update thread membership with postid={post_id} userid={user_id}"
                        ),
                        source,
                    })?;
                }

                membership
            }
            None => {
                tracing::Span::current().record("existing", false);
                tracing::Span::current().record("wrote", true);
                let membership = ThreadMembership {
                    post_id: post_id.to_owned(),
                    user_id: user_id.to_owned(),
                    following: opts.following,
                    last_updated: now,
                    // Two independent `if`s in Go, not the update path's `else if`.
                    last_viewed: if opts.update_viewed_timestamp { now } else { 0 },
                    unread_mentions: i64::from(opts.increment_mentions),
                };

                sqlx::query!(
                    r#"
                    INSERT INTO threadmemberships
                                (postid, userid, following, lastviewed, lastupdated, unreadmentions)
                         VALUES ($1, $2, $3, $4, $5, $6)
                    "#,
                    membership.post_id,
                    membership.user_id,
                    membership.following,
                    membership.last_viewed,
                    membership.last_updated,
                    membership.unread_mentions,
                )
                .execute(&mut *tx)
                .await
                .map_err(|source| StoreError::Db {
                    context: format!(
                        "failed to save thread membership with postid={post_id} userid={user_id}"
                    ),
                    source,
                })?;

                if opts.update_participants {
                    update_thread_participants_for_user(&mut tx, post_id, user_id).await?;
                }

                membership
            }
        };

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;

        Ok(membership)
    }

    #[tracing::instrument(skip(self), fields(thread_id = %thread_id, following_only = following_only, found))]
    async fn get_thread_followers(
        &self,
        thread_id: &str,
        following_only: bool,
    ) -> Result<Vec<String>, StoreError> {
        // `sq.Eq{"Following": true}` is an equality, so a NULL `Following` fails it just as
        // `false` does; `$2 = FALSE` short-circuits to Go's unfiltered select.
        let rows = sqlx::query_scalar!(
            r#"
            SELECT userid AS "user_id!"
              FROM threadmemberships
             WHERE postid = $1
               AND (NOT $2 OR following = TRUE)
            "#,
            thread_id,
            following_only,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get thread followers for thread id={thread_id}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows)
    }

    /// **A missing row is `Ok(None)`, not `ErrNotFound`.** Go returns `nil, nil` for
    /// `sql.ErrNoRows` here, unlike every other getter in this file, and its one caller
    /// (`UpdateThreadFollowForUser`) leans on it: a thread whose metadata row does not exist
    /// answers `reply_count: 0` on the websocket event rather than failing the request. Turning
    /// this into a `NotFound` would 500 the follow of a root post nobody has replied to.
    ///
    /// The two columns are aliased in Go and are aliased here for the same reason
    /// (`ThreadDeleteAt` → `delete_at`, `ThreadTeamId` → `team_id`); `COALESCE` matches Go's
    /// select list, which wraps exactly those two and leaves `ReplyCount` and `LastReplyAt` bare
    /// — they are nullable columns, so the model's `i64` needs the coalesce Go's does not.
    #[tracing::instrument(skip(self), fields(post_id = %post_id, found))]
    async fn get(&self, post_id: &str) -> Result<Option<Thread>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT postid                        AS "post_id!",
                   COALESCE(channelid, '')       AS "channel_id!",
                   COALESCE(replycount, 0)       AS "reply_count!",
                   COALESCE(lastreplyat, 0)      AS "last_reply_at!",
                   participants                  AS "participants?",
                   COALESCE(threaddeleteat, 0)   AS "delete_at!",
                   COALESCE(threadteamid, '')    AS "team_id!"
              FROM threads
             WHERE postid = $1
            "#,
            post_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get thread with id={post_id}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());

        Ok(row.map(|row| Thread {
            post_id: row.post_id,
            channel_id: row.channel_id,
            reply_count: row.reply_count,
            last_reply_at: row.last_reply_at,
            participants: row
                .participants
                .and_then(|value| serde_json::from_value(value).ok()),
            delete_at: row.delete_at,
            team_id: row.team_id,
        }))
    }

    /// # `LastViewed` is the argument, `LastUpdated` is the clock
    ///
    /// The two columns are set from **different** sources here, unlike the team sweep beside it
    /// which writes one `GetMillis()` into both. A port that copied the sweep would move
    /// `LastViewed` to now on a set-unread, which is exactly the opposite of what that route
    /// asks for.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, thread_id = %thread_id, timestamp, updated))]
    async fn mark_as_read(
        &self,
        user_id: &str,
        thread_id: &str,
        timestamp: i64,
    ) -> Result<(), StoreError> {
        let now = mm_model::utils::get_millis();

        let result = sqlx::query!(
            r#"
            UPDATE threadmemberships
               SET lastviewed = $1,
                   lastupdated = $2
             WHERE userid = $3
               AND postid = $4
            "#,
            timestamp,
            now,
            user_id,
            thread_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to update thread read state for user id={user_id} thread_id={thread_id}"
            ),
            source,
        })?;

        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }

    /// # Strictly greater, on the root's replies only
    ///
    /// `Posts.RootId = PostId AND Posts.CreateAt > LastViewed AND DeleteAt = 0`. The root post
    /// itself has an empty `RootId` and is never counted, and a reply created at exactly
    /// `LastViewed` is read. `GetThreadForUser`'s own unread-replies subquery says the same, and
    /// the set-unread route relies on the two agreeing: it moves the mark to `CreateAt - 1` so
    /// that the reply lands on the unread side of a `>`.
    #[tracing::instrument(
        skip(self, membership),
        fields(post_id = %membership.post_id, last_viewed = membership.last_viewed)
    )]
    async fn get_thread_unread_reply_count(
        &self,
        membership: &ThreadMembership,
    ) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(posts.id) AS "count!"
              FROM posts
             WHERE posts.rootid = $1
               AND posts.createat > $2
               AND posts.deleteat = 0
            "#,
            membership.post_id,
            membership.last_viewed,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to count unread reply count for post id={}",
                membership.post_id
            ),
            source,
        })?;

        Ok(count)
    }

    /// # No guard, no clock
    ///
    /// Unlike [`Self::maintain_membership`], this writes every column from the argument
    /// unconditionally, `LastUpdated` included — so a caller that wants the clock in that column
    /// sets it first, and `UpdateThreadReadForUser` does not: it leaves the row's old
    /// `LastUpdated` in place here and lets [`Self::mark_as_read`] stamp it a statement later.
    /// A row that does not exist is a no-op, not an error, on both servers.
    #[tracing::instrument(
        skip(self, membership),
        fields(post_id = %membership.post_id, user_id = %membership.user_id, updated)
    )]
    async fn update_membership(&self, membership: &ThreadMembership) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"
            UPDATE threadmemberships
               SET following = $3,
                   lastviewed = $4,
                   lastupdated = $5,
                   unreadmentions = $6
             WHERE postid = $1
               AND userid = $2
            "#,
            membership.post_id,
            membership.user_id,
            membership.following,
            membership.last_viewed,
            membership.last_updated,
            membership.unread_mentions,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to update thread membership with postid={} userid={}",
                membership.post_id, membership.user_id
            ),
            source,
        })?;

        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }
}

/// Port of `updateThreadParticipantsForUserTx` (thread_store.go:1125), in the caller's
/// transaction.
///
/// `jsonb_build_array($2::text)` is Go's `jsonArray([]string{userID})` — the one-element JSON
/// array `||` appends. The guard is jsonb's `?` (key/element exists), which on a JSON array tests
/// string membership, so the append is idempotent per user. `Threads` has no row for a root
/// nobody has replied to, and the affected-row count is recorded rather than checked: Go ignores
/// it too.
async fn update_thread_participants_for_user(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    post_id: &str,
    user_id: &str,
) -> Result<(), StoreError> {
    let result = sqlx::query!(
        r#"
        UPDATE threads
           SET participants = participants || jsonb_build_array($2::text)
         WHERE postid = $1
           AND NOT participants ? $2
        "#,
        post_id,
        user_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to update thread participants with postid={post_id} userid={user_id}"
        ),
        source,
    })?;
    tracing::Span::current().record("participants_updated", result.rows_affected());
    Ok(())
}
