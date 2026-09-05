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

use mm_model::thread::ThreadResponse;
use mm_model::user::User;
use sqlx::PgPool;

use crate::error::StoreError;
use crate::post_store::{PostRow, post_from_row};

/// Port of `store.ThreadStore`, narrowed to the threads-list route.
pub trait ThreadStore {
    /// Port of `SqlThreadStore.GetThreadsForUser` (thread_store.go:283), for the option set the
    /// api4 handler serves: a team, no cursor, not deleted, not unread-only.
    fn get_threads_for_user(
        &self,
        user_id: &str,
        team_id: &str,
        page_size: i64,
        include_is_urgent: bool,
    ) -> impl std::future::Future<Output = Result<Vec<ThreadResponse>, StoreError>> + Send;

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
}
