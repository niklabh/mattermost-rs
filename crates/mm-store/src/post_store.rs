//! Port of `SqlPostStore` (channels/store/sqlstore/post_store.go), `GetSingle` only, plus the
//! two single-post metadata reads `getPost` reaches through `PreparePostForClient`:
//! `SqlPostPriorityStore.GetForPostWithContext` and `SqlPostAcknowledgementStore.GetForPost`.
//!
//! # The columns are not the wire fields
//!
//! `postSliceColumnsWithTypes` (post_store.go:53) selects **eighteen** columns, and the `Post`
//! wire type has more fields than that. `PendingPostId`, `LastReplyAt`, `Participants`,
//! `IsFollowing`, `Metadata` and `MessageSource` are never selected here — they stay at their
//! zero values, which is why a freshly read post serialises `"pending_post_id":""`,
//! `"last_reply_at":0` and `"participants":null`. Adding any of them to this query would put
//! values on the wire that the Go server does not send.
//!
//! # `ReplyCount` is a correlated subquery, appended last
//!
//! Go builds it with `.Column(sq.Alias(replyCountSubQuery, "ReplyCount"))`, so it lands after
//! `RemoteId` in the select list. It counts the **thread**, not the post's own children: the
//! root id is `Posts.RootId` when the post is a reply and `Posts.Id` when it is a root, so a
//! reply reports the number of siblings *including itself*. Reading it as "replies to this post"
//! is the mistake this comment exists to prevent.
//!
//! # A NULL `props` column is an empty map; a NULL `fileids` column is nil
//!
//! Reading the two `Scan` methods (model/utils.go:118, :185) says otherwise — both return early
//! on a NULL and leave the field at its zero value, which for a map is nil and marshals as
//! `null`. That is what this store did until it was measured, and it was wrong for one of them.
//!
//! The scan never reaches `Props`. **sqlx materialises a nil map field before scanning into it**
//! (`reflectx.FieldByIndexes` calls `reflect.MakeMap` for any nil map on the path), so
//! `StringInterface.Scan` receives its early-return NULL with an *already empty* map underneath
//! and the field marshals as `{}`. `Filenames` and `FileIds` are slices, which that code path
//! does not allocate, so they really do stay nil.
//!
//! Measured against the running 11.11.0 server on three routes, and discriminated from the
//! neighbouring cases rather than inferred from one of them:
//!
//! | `posts.props` | Go answers |
//! |---|---|
//! | SQL `NULL` | `"props":{}` |
//! | jsonb `'null'` | `"props":null` — `json.Unmarshal` of `null` sets the map back to nil |
//! | jsonb `'[1,2]'` | 500, `app.post.get.app_error` |
//!
//! The middle row is what makes the first one a finding rather than a guess: if the empty map
//! came from anywhere later in the pipeline, both would answer `{}`.
//!
//! This is a property of **sqlx and a nil map**, not of posts, so every ported store that scans
//! a Go map field out of a nullable column has the same question open — see [D-158].

use mm_model::post::Post;
use mm_model::post_acknowledgement::PostAcknowledgement;
use mm_model::post_list::PostList;
use mm_model::post_metadata::PostPriority;
use mm_model::user::User;
use mm_model::utils::{StringArray, StringInterface};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.PostStore`, narrowed to what `GET /posts/{post_id}` reaches.
pub trait PostStore {
    /// Port of `SqlPostStore.GetSingle` (post_store.go:918).
    fn get_single(
        &self,
        id: &str,
        incl_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Post, StoreError>> + Send;

    /// Port of `SqlPostStore.GetPostsByIds` (post_store.go:2592).
    fn get_posts_by_ids(
        &self,
        post_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Post>, StoreError>> + Send;

    /// Port of `SqlPostPriorityStore.GetForPostWithContext` (post_priority_store.go:29).
    fn get_priority_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<PostPriority>, StoreError>> + Send;

    /// Port of `SqlPostAcknowledgementStore.GetForPost` (post_acknowledgements_store.go:121).
    fn get_acknowledgements_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<PostAcknowledgement>, StoreError>> + Send;

    /// Port of `SqlPostStore.GetPosts` (post_store.go:1355), both of its branches.
    ///
    /// Returns the page as Go assembles it, before the app layer touches it: the order is the
    /// window's own, the map may hold posts the order does not name, and `make_non_nil` has
    /// been applied on exactly the branch Go applies it to.
    fn get_posts(
        &self,
        opts: GetPostsOptions<'_>,
    ) -> impl std::future::Future<Output = Result<PostList, StoreError>> + Send;

    /// The `UpdateAt` half of `SqlPostStore.GetEtag` (post_store.go:951): the newest
    /// `Posts.UpdateAt` in the channel, or `None` when the channel is empty **or the query
    /// failed**.
    ///
    /// Infallible on purpose. Go swallows the error — `err != nil` and "no rows" take the same
    /// branch, which stamps `CurrentVersion.<now>` and serves the request anyway — so a caller
    /// that could see the failure would have to invent a behaviour Go does not have. The
    /// formatting itself lives in [`mm_app`] rather than here, so it can be tested without a
    /// database; nothing about that is on the wire.
    ///
    /// **`collapsedThreads` is deliberately not a parameter.** Go takes it, and then drops it:
    /// `q.Where(sq.Eq{"RootId": ""})` at post_store.go:954 discards the returned builder, since
    /// squirrel's builders are values. So the etag is the same for both modes, and adding the
    /// filter here would make our 304s disagree with Go's.
    fn get_etag(&self, channel_id: &str) -> impl std::future::Future<Output = Option<i64>> + Send;

    /// Port of `SqlPostStore.getPostIdAroundTime` (post_store.go:1822), which
    /// `GetPostIdBeforeTime` and `GetPostIdAfterTime` are one-line wrappers over.
    ///
    /// An empty string is Go's "no such post": it swallows `sql.ErrNoRows` and returns the zero
    /// value, and the cursor fields carry `""` to the client.
    fn get_post_id_around_time(
        &self,
        channel_id: &str,
        time: i64,
        before: bool,
        collapsed_threads: bool,
    ) -> impl std::future::Future<Output = Result<String, StoreError>> + Send;

    /// Port of `SqlPostStore.GetVisiblePostIdAroundTime` (post_store.go:1887) — the cursor
    /// lookup used **whenever burn-on-read is enabled**, which on a default-configured server it
    /// is: `ServiceSettings.EnableBurnOnRead` and `FeatureFlags.BurnOnRead` both default to
    /// `true`, so this is the live path and [`PostStore::get_post_id_around_time`] is the
    /// fallback, not the other way round.
    ///
    /// The extra predicate is `burnOnReadVisibleCondition` (post_store.go:1930): a post is
    /// visible unless it is a burn-on-read post, written by somebody else, whose read receipt
    /// for this user has already expired. Its `now` is stamped when the query is built, so two
    /// calls a millisecond apart can legitimately differ — the cursor is a live value, not a
    /// stable one.
    fn get_visible_post_id_around_time(
        &self,
        channel_id: &str,
        time: i64,
        before: bool,
        collapsed_threads: bool,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<String, StoreError>> + Send;

    /// Port of `SqlPostPriorityStore.GetForPosts` (post_priority_store.go:45).
    fn get_priority_for_posts(
        &self,
        post_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<PostPriority>, StoreError>> + Send;

    /// Port of `SqlPostAcknowledgementStore.GetForPosts` (post_acknowledgements_store.go:140).
    fn get_acknowledgements_for_posts(
        &self,
        post_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<PostAcknowledgement>, StoreError>> + Send;

    /// Port of `SqlPostStore.Get` (post_store.go:746) and the
    /// `getPostWithCollapsedThreads` (:620) it delegates to — the thread behind
    /// `GET /posts/{post_id}/thread`.
    ///
    /// Go names this `Get`, beside a `GetSingle` that really does return one post. It returns a
    /// whole [`PostList`], and which posts are in it depends on three of the options in ways
    /// that are not symmetric between the two branches — see [`GetPostThreadOptions`].
    /// Port of `SqlPostStore.GetEditHistoryForPost` (post_store.go:2610).
    ///
    /// **Zero rows is `ErrNotFound`, not an empty list** — a post that has never been edited is
    /// a 404 through this route, not a `[]`.
    fn get_edit_history_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Post>, StoreError>> + Send;

    /// Port of `SqlPostStore.GetPostsBefore`/`GetPostsAfter` (post_store.go:1578, :1582), which
    /// are one function, `getPostsAround` (:1701), with a flag.
    fn get_posts_around(
        &self,
        opts: GetPostsAroundOptions<'_>,
        before: bool,
    ) -> impl std::future::Future<Output = Result<PostList, StoreError>> + Send;

    fn get_thread(
        &self,
        id: &str,
        opts: GetPostThreadOptions<'_>,
    ) -> impl std::future::Future<Output = Result<PostList, StoreError>> + Send;
}

/// Port of `model.GetPostsOptions` (post.go:456), narrowed to the fields the page branch of
/// `getPostsForChannel` reaches.
///
/// The eight fields Go declares and this omits — `PostId`, `FromPost`, `FromCreateAt`,
/// `FromUpdateAt`, `Direction`, `UpdatesOnly`, `IncludePostPriority` and
/// `ExcludeExpiredBurnOnReadPosts` — belong to the cursor and burn-on-read paths, which this
/// server forwards. Adding one before its query exists would be a field with no reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetPostsOptions<'a> {
    pub channel_id: &'a str,
    /// The **session's** user, not a post author: it is the `ThreadMemberships` join key that
    /// decides `is_following`, and it is read on the collapsed-threads branch only.
    pub user_id: &'a str,
    pub page: i64,
    pub per_page: i64,
    pub skip_fetch_threads: bool,
    pub collapsed_threads: bool,
    pub include_deleted: bool,
}

impl GetPostsOptions<'_> {
    /// `offset := options.PerPage * options.Page` (post_store.go:1362), computed in Go's `int`.
    ///
    /// Wrapping rather than saturating for the reason `mm_api::channels::page_offset` gives:
    /// `int` is 64-bit on every platform this runs on, and the overflow is reachable from the
    /// query string because `strconv.Atoi` accepts `9223372036854775807`.
    fn offset(&self) -> i64 {
        self.per_page.wrapping_mul(self.page)
    }
}

/// `model.GetPostsOptions.Direction` (post.go:456), which only ever holds three values.
///
/// Go carries it as a bare `string` and compares it twice per query — once to pick the sort
/// order and once to pick the cursor's comparison operator. Modelling it as an enum makes the
/// query selection total: `getPostThread` has already rejected everything that is not `""`,
/// `"up"` or `"down"` with a 400, so a fourth spelling cannot reach the store.
///
/// **The mapping is the reverse of the intuitive one.** `"up"` sorts **descending** and
/// `"down"` sorts **ascending** (post_store.go:659) — the names describe which way the client
/// is scrolling, not which way the rows come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadDirection {
    /// Go's `""`: **no `ORDER BY` at all**, so the row order is whatever Postgres returns. Not
    /// "ascending by default" — the clause is simply absent, which is why this port has a
    /// separate statement for it rather than a degenerate sort key.
    #[default]
    Unset,
    /// `"up"` — `ORDER BY … DESC`, and the cursor comparisons are `<`.
    Up,
    /// `"down"` — `ORDER BY … ASC`, and the cursor comparisons are `>`.
    Down,
}

impl ThreadDirection {
    /// Go's `sort == "DESC"`.
    fn descending(self) -> bool {
        self == ThreadDirection::Up
    }

    /// Go's `opts.Direction == "down"`, the test both cursor blocks make. Note that `Unset`
    /// answers `false` here and therefore takes the **same** branch as `Up`: an unordered
    /// request carrying `fromCreateAt` still filters with `<`.
    fn is_down(self) -> bool {
        self == ThreadDirection::Down
    }
}

/// Port of `model.GetPostsOptions` (post.go:456) for the fields `SqlPostStore.Get` reads.
///
/// A second struct rather than more fields on [`GetPostsOptions`]: Go has one type serving both
/// queries, but the two overlap in only three fields and disagree about two of those. Keeping
/// them apart is what lets each struct's documentation say what its own query does with the
/// value.
///
/// # The two branches disagree about three of these
///
/// | | non-collapsed (`Get`) | collapsed (`getPostWithCollapsedThreads`) |
/// |---|---|---|
/// | `skip_fetch_threads` | skips the reply query **and** `has_next` entirely | ignored |
/// | `from_update_at` | filters in **both** directions | filters **only** when `direction == Down` |
/// | reply `reply_count` | the thread's count, from a CTE | always `0` — `postsQuery` has no such column |
///
/// `collapsed_threads_extended` is absent for the reason it is absent from [`GetPostsOptions`]:
/// it replaces each stub participant with a `SanitizeProfile`d user, and `mm_api::posts`
/// forwards that request rather than reproducing config-dependent output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetPostThreadOptions<'a> {
    /// The **session's** user — the `ThreadMemberships` join key that decides `is_following`,
    /// read on the collapsed branch only, exactly as in [`GetPostsOptions`].
    pub user_id: &'a str,
    pub skip_fetch_threads: bool,
    pub collapsed_threads: bool,
    /// Swaps `CreateAt` for `UpdateAt` **in the `ORDER BY` only**. Neither cursor predicate
    /// consults it: `from_create_at` always compares `CreateAt` and `from_update_at` always
    /// compares `UpdateAt`, whatever this says.
    pub updates_only: bool,
    /// `0` means **no limit** — the clause is not emitted. Go then fetches `per_page + 1` rows
    /// and uses the extra one to set `has_next`.
    pub per_page: i64,
    pub direction: ThreadDirection,
    /// The tie-break for a cursor whose timestamp is not unique. Empty means the cursor is the
    /// timestamp alone, and Go's test is on this field being non-empty rather than on the
    /// request carrying the key.
    pub from_post: &'a str,
    pub from_create_at: i64,
    pub from_update_at: i64,
}

/// Port of the `model.GetPostsOptions` fields `getPostsAround` (post_store.go:1701) reads.
///
/// A third options struct, for the reason the second one exists: this query overlaps
/// [`GetPostsOptions`] in four fields and disagrees with it about two more, and one struct
/// serving all three would have to document every field three times.
///
/// # `include_deleted` is absent, and that is a claim about the caller
///
/// `getPostsForChannelAroundLastUnread` builds its `GetPostsOptions` literals without it
/// (app/post.go:1961, :1968), so `DeleteAt = 0` is unconditional on both the window and the
/// parents query, and the reply-count subquery keeps its own `DeleteAt = 0` too. A second caller
/// that needs the flag adds it here **and** to all three places, not to one.
///
/// # `collapsed_threads_extended` is absent for the usual reason
///
/// It replaces each stub participant with a `SanitizeProfile`d user; `mm_api::posts` forwards
/// that request rather than reproducing config-dependent output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetPostsAroundOptions<'a> {
    pub channel_id: &'a str,
    /// The cursor. Its own `CreateAt` is read by a subquery inside the predicate, so a `post_id`
    /// naming nothing yields `CreateAt <> NULL` — which matches no row and returns an empty
    /// list rather than an error.
    pub post_id: &'a str,
    /// The **session's** user, used by the burn-on-read visibility predicate and, on the
    /// collapsed branch, as the `ThreadMemberships` join key.
    pub user_id: &'a str,
    pub page: i64,
    pub per_page: i64,
    pub skip_fetch_threads: bool,
    pub collapsed_threads: bool,
}

impl GetPostsAroundOptions<'_> {
    /// Go's `options.Page * options.PerPage`, computed before either reaches the SQL.
    fn offset(&self) -> i64 {
        self.page * self.per_page
    }
}

/// Port of `SqlPostStore` plus the priority and acknowledgement stores.
///
/// Go keeps `PostPriority` and `PostAcknowledgement` in their own store objects. They are folded
/// in here because both are keyed on `PostId`, are only ever read by a post handler, and neither
/// has a second caller to justify its own file.
#[derive(Debug, Clone)]
pub struct SqlPostStore {
    pool: PgPool,
}

impl SqlPostStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Port of `SqlPostStore.getRootPosts` (post_store.go:1949).
    ///
    /// **The name is Go's and it is a lie**: there is no `RootId = ''` predicate, so the window
    /// is every post in the channel — replies included — newest first. The collapsed-threads
    /// query is the one that filters to roots.
    ///
    /// `skip_fetch_threads` decides whether `ReplyCount` is computed at all. Go builds two
    /// different SQL strings; the `CASE WHEN` here is the same truth table in one statement the
    /// `query_as!` macro can check, and it keeps both predicates visible to a mutation. When the
    /// flag is off every post reports `reply_count: 0` — that is Go's answer too, because the
    /// column is simply absent from its select list.
    async fn get_root_posts(&self, opts: GetPostsOptions<'_>) -> Result<Vec<Post>, StoreError> {
        let rows = sqlx::query_as!(
            PostRow,
            r#"
            SELECT p.id,
                   p.createat     AS "create_at!",
                   p.updateat     AS "update_at!",
                   p.editat       AS "edit_at!",
                   p.deleteat     AS "delete_at!",
                   p.ispinned     AS "is_pinned!",
                   p.userid       AS "user_id!",
                   p.channelid    AS "channel_id!",
                   p.rootid       AS "root_id!",
                   p.originalid   AS "original_id!",
                   p.message      AS "message!",
                   p.type         AS "post_type!",
                   p.props        AS "props?",
                   p.hashtags     AS "hashtags!",
                   p.filenames    AS "filenames?",
                   p.fileids      AS "file_ids?",
                   p.hasreactions AS "has_reactions!",
                   p.remoteid     AS "remote_id?",
                   CASE WHEN $4 THEN (SELECT COUNT(*)
                                        FROM posts sub
                                       WHERE sub.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                                         AND ($5 OR sub.deleteat = 0))
                        ELSE 0::bigint END AS "reply_count!"
              FROM posts p
             WHERE p.channelid = $1
               AND ($5 OR p.deleteat = 0)
             ORDER BY p.createat DESC
             LIMIT $2 OFFSET $3
            "#,
            opts.channel_id,
            opts.per_page,
            opts.offset(),
            opts.skip_fetch_threads,
            opts.include_deleted,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Posts".to_owned(),
            source,
        })?;

        rows.into_iter().map(post_from_row).collect()
    }

    /// Port of `SqlPostStore.getParentsPosts` (post_store.go:1973).
    ///
    /// The window is the same one [`Self::get_root_posts`] returns, but only its `RootId`s are
    /// kept; the outer select then fetches the *threads* those ids name. What "the thread"
    /// means depends on `skip_fetch_threads`, and this is the branch where the flag earns its
    /// name: with it off, the join is `q1.RootId = q2.Id OR q1.RootId = q2.RootId`, which pulls
    /// in every sibling reply of every thread touched by the window. With it on, only the root
    /// post itself is fetched and the client is expected to ask for the thread separately.
    ///
    /// Nothing here reaches `order` — Go calls `AddPost` and not `AddOrder` — so these posts are
    /// in the response's `posts` map without appearing in its `order`.
    async fn get_parents_posts(&self, opts: GetPostsOptions<'_>) -> Result<Vec<Post>, StoreError> {
        let rows = sqlx::query_as!(
            PostRow,
            r#"
            SELECT q2.id,
                   q2.createat     AS "create_at!",
                   q2.updateat     AS "update_at!",
                   q2.editat       AS "edit_at!",
                   q2.deleteat     AS "delete_at!",
                   q2.ispinned     AS "is_pinned!",
                   q2.userid       AS "user_id!",
                   q2.channelid    AS "channel_id!",
                   q2.rootid       AS "root_id!",
                   q2.originalid   AS "original_id!",
                   q2.message      AS "message!",
                   q2.type         AS "post_type!",
                   q2.props        AS "props?",
                   q2.hashtags     AS "hashtags!",
                   q2.filenames    AS "filenames?",
                   q2.fileids      AS "file_ids?",
                   q2.hasreactions AS "has_reactions!",
                   q2.remoteid     AS "remote_id?",
                   CASE WHEN $4 THEN (SELECT COUNT(*)
                                        FROM posts sub
                                       WHERE sub.rootid = (CASE WHEN q2.rootid = '' THEN q2.id ELSE q2.rootid END)
                                         AND ($5 OR sub.deleteat = 0))
                        ELSE 0::bigint END AS "reply_count!"
              FROM posts q2
             INNER JOIN (SELECT DISTINCT q3.rootid
                           FROM (SELECT posts.rootid
                                   FROM posts
                                  WHERE posts.channelid = $1
                                    AND ($5 OR posts.deleteat = 0)
                                  ORDER BY posts.createat DESC
                                  LIMIT $2 OFFSET $3) q3
                          WHERE q3.rootid != '') q1
                ON (q1.rootid = q2.id OR (NOT $4 AND q1.rootid = q2.rootid))
             WHERE q2.channelid = $1
               AND ($5 OR q2.deleteat = 0)
             ORDER BY q2.createat
            "#,
            opts.channel_id,
            opts.per_page,
            opts.offset(),
            opts.skip_fetch_threads,
            opts.include_deleted,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Posts with channelId={}", opts.channel_id),
            source,
        })?;

        rows.into_iter().map(post_from_row).collect()
    }

    /// The second half of `getPostsAround` (post_store.go:1773): the *threads* the window's
    /// posts belong to.
    ///
    /// # `skip_fetch_threads` decides how much of each thread comes back
    ///
    /// With it off, `Id IN (ids) OR RootId IN (ids)` pulls in every sibling reply of every thread
    /// the window touched. With it on, only the posts named by `ids` themselves — which is to say
    /// the window's own posts and the roots of any replies in it. The `ids` list is built from
    /// each post's own id **and** its `RootId`, so a reply in the window brings its root back
    /// either way.
    ///
    /// `ORDER BY CreateAt DESC` and nothing reaches `order`, so the ordering decides only which
    /// duplicate wins the `posts` map — and ids are unique, so nothing does. It is here because
    /// it is in Go's query.
    async fn get_posts_around_parents(
        &self,
        opts: &GetPostsAroundOptions<'_>,
        root_ids: &[String],
    ) -> Result<Vec<Post>, StoreError> {
        let rows = sqlx::query_as!(
            PostRow,
            r#"
            SELECT p.id,
                   p.createat     AS "create_at!",
                   p.updateat     AS "update_at!",
                   p.editat       AS "edit_at!",
                   p.deleteat     AS "delete_at!",
                   p.ispinned     AS "is_pinned!",
                   p.userid       AS "user_id!",
                   p.channelid    AS "channel_id!",
                   p.rootid       AS "root_id!",
                   p.originalid   AS "original_id!",
                   p.message      AS "message!",
                   p.type         AS "post_type!",
                   p.props        AS "props?",
                   p.hashtags     AS "hashtags!",
                   p.filenames    AS "filenames?",
                   p.fileids      AS "file_ids?",
                   p.hasreactions AS "has_reactions!",
                   p.remoteid     AS "remote_id?",
                   (SELECT COUNT(*)
                      FROM posts sub
                     WHERE sub.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                       AND sub.deleteat = 0) AS "reply_count!"
              FROM posts p
             WHERE (p.id = ANY($2) OR (NOT $3 AND p.rootid = ANY($2)))
               AND p.channelid = $1
               AND p.deleteat = 0
             ORDER BY p.createat DESC
            "#,
            opts.channel_id,
            root_ids,
            opts.skip_fetch_threads,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Posts with channelId={}", opts.channel_id),
            source,
        })?;

        rows.into_iter().map(post_from_row).collect()
    }

    /// Port of `SqlPostStore.getPostsCollapsedThreads` (post_store.go:1322) and the
    /// `prepareThreadedResponse` (:1259) it hands to.
    ///
    /// # This branch ignores two of its own options
    ///
    /// `include_deleted` and `skip_fetch_threads` are both in the options struct and neither
    /// appears in the query: it is always `Posts.DeleteAt = 0`, and the reply count always comes
    /// from the `Threads` table rather than a subquery. So `?include_deleted=true` on a
    /// collapsed-threads request silently returns no deleted posts — and Go's 403 gate in front
    /// of it still fires, which is the only observable trace the parameter leaves.
    ///
    /// # Three columns the other branch never sets
    ///
    /// `last_reply_at` and `participants` come straight from `Threads`, and `is_following` from
    /// the caller's `ThreadMemberships` row — a `*bool`, so **no membership row is `null` on the
    /// wire, not `false`**. A non-collapsed page leaves all three at their zero values, which is
    /// how `"participants":null` and an absent `is_following` reach a client that did not ask
    /// for collapsed threads.
    ///
    /// `extended` is not a parameter here: `collapsedThreadsExtended=true` replaces the stub
    /// participants with sanitized profiles, and `mm_api::posts` forwards that request rather
    /// than reproducing `SanitizeProfile`'s config-dependent output.
    async fn get_posts_collapsed_threads(
        &self,
        opts: GetPostsOptions<'_>,
    ) -> Result<PostList, StoreError> {
        let rows = sqlx::query_as!(
            ThreadedPostRow,
            r#"
            SELECT posts.id,
                   posts.createat     AS "create_at!",
                   posts.updateat     AS "update_at!",
                   posts.editat       AS "edit_at!",
                   posts.deleteat     AS "delete_at!",
                   posts.ispinned     AS "is_pinned!",
                   posts.userid       AS "user_id!",
                   posts.channelid    AS "channel_id!",
                   posts.rootid       AS "root_id!",
                   posts.originalid   AS "original_id!",
                   posts.message      AS "message!",
                   posts.type         AS "post_type!",
                   posts.props        AS "props?",
                   posts.hashtags     AS "hashtags!",
                   posts.filenames    AS "filenames?",
                   posts.fileids      AS "file_ids?",
                   posts.hasreactions AS "has_reactions!",
                   posts.remoteid     AS "remote_id?",
                   COALESCE(threads.replycount, 0)          AS "thread_reply_count!",
                   COALESCE(threads.lastreplyat, 0)         AS "last_reply_at!",
                   COALESCE(threads.participants, '[]'::jsonb) AS "thread_participants!",
                   threadmemberships.following              AS "is_following?"
              FROM posts
              LEFT JOIN threads ON threads.postid = posts.id
              LEFT JOIN threadmemberships ON threadmemberships.postid = posts.id
                                         AND threadmemberships.userid = $4
             WHERE posts.deleteat = 0
               AND posts.channelid = $1
               AND posts.rootid = ''
             ORDER BY posts.createat DESC
             LIMIT $2 OFFSET $3
            "#,
            opts.channel_id,
            opts.per_page,
            opts.offset(),
            opts.user_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Posts with channelId={}", opts.channel_id),
            source,
        })?;

        let mut list = PostList::new();
        for row in rows {
            let post = threaded_post_from_row(row)?;
            let id = post.id.clone();
            list.add_post(post);
            list.add_order(id);
        }
        Ok(list)
    }

    /// The single-post fetch at the head of `SqlPostStore.Get` (post_store.go:754).
    ///
    /// Its `ReplyCount` subquery resolves the **thread's** root before counting
    /// (`CASE WHEN p.RootId = '' THEN p.Id ELSE p.RootId END`), so asking for a reply's thread
    /// reports the number of posts in the thread it belongs to, not the number of answers to
    /// that reply. `GetSingle`'s column is the same expression.
    ///
    /// `DeleteAt = 0` is unconditional here: this route has no `include_deleted`, so a
    /// soft-deleted post is a 404 even for an administrator.
    async fn get_thread_root(&self, id: &str) -> Result<Option<Post>, StoreError> {
        let row = sqlx::query_as!(
            PostRow,
            r#"
            SELECT p.id,
                   p.createat     AS "create_at!",
                   p.updateat     AS "update_at!",
                   p.editat       AS "edit_at!",
                   p.deleteat     AS "delete_at!",
                   p.ispinned     AS "is_pinned!",
                   p.userid       AS "user_id!",
                   p.channelid    AS "channel_id!",
                   p.rootid       AS "root_id!",
                   p.originalid   AS "original_id!",
                   p.message      AS "message!",
                   p.type         AS "post_type!",
                   p.props        AS "props?",
                   p.hashtags     AS "hashtags!",
                   p.filenames    AS "filenames?",
                   p.fileids      AS "file_ids?",
                   p.hasreactions AS "has_reactions!",
                   p.remoteid     AS "remote_id?",
                   (SELECT COUNT(*)
                      FROM posts sub
                     WHERE sub.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                       AND sub.deleteat = 0) AS "reply_count!"
              FROM posts p
             WHERE p.id = $1
               AND p.deleteat = 0
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Post with id={id}"),
            source,
        })?;

        row.map(post_from_row).transpose()
    }

    /// The single-post fetch at the head of `getPostWithCollapsedThreads` (post_store.go:637).
    ///
    /// The same row as [`Self::get_thread_root`] with the three `Threads` columns and the
    /// caller's `ThreadMemberships.Following` joined on — and **no `ReplyCount` subquery**, so
    /// the count comes from `Threads.ReplyCount` instead. The two counts are not the same
    /// number: the subquery counts rows and `Threads.ReplyCount` is a maintained counter.
    async fn get_collapsed_thread_root(
        &self,
        id: &str,
        user_id: &str,
    ) -> Result<Option<Post>, StoreError> {
        let row = sqlx::query_as!(
            ThreadedPostRow,
            r#"
            SELECT posts.id,
                   posts.createat     AS "create_at!",
                   posts.updateat     AS "update_at!",
                   posts.editat       AS "edit_at!",
                   posts.deleteat     AS "delete_at!",
                   posts.ispinned     AS "is_pinned!",
                   posts.userid       AS "user_id!",
                   posts.channelid    AS "channel_id!",
                   posts.rootid       AS "root_id!",
                   posts.originalid   AS "original_id!",
                   posts.message      AS "message!",
                   posts.type         AS "post_type!",
                   posts.props        AS "props?",
                   posts.hashtags     AS "hashtags!",
                   posts.filenames    AS "filenames?",
                   posts.fileids      AS "file_ids?",
                   posts.hasreactions AS "has_reactions!",
                   posts.remoteid     AS "remote_id?",
                   COALESCE(threads.replycount, 0)             AS "thread_reply_count!",
                   COALESCE(threads.lastreplyat, 0)            AS "last_reply_at!",
                   COALESCE(threads.participants, '[]'::jsonb) AS "thread_participants!",
                   threadmemberships.following                 AS "is_following?"
              FROM posts
              LEFT JOIN threads ON threads.postid = posts.id
              LEFT JOIN threadmemberships ON threadmemberships.postid = posts.id
                                         AND threadmemberships.userid = $2
             WHERE posts.deleteat = 0
               AND posts.id = $1
            "#,
            id,
            user_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Post with id={id}"),
            source,
        })?;

        row.map(threaded_post_from_row).transpose()
    }

    /// The reply fetch in `SqlPostStore.Get` (post_store.go:779), the branch behind
    /// `!skipFetchThreads`.
    ///
    /// # The window is the whole thread, root included
    ///
    /// `p.Id = rootId OR p.RootId = rootId`, so the root post comes back here as well as from
    /// [`Self::get_thread_root`]. Go drops the duplicate by id afterwards — but only **after**
    /// it has counted the rows for `has_next`, which is why the caller must not filter it here.
    ///
    /// # `ReplyCount` is one number for every row
    ///
    /// It comes from a `WITH replycount` CTE cross-joined into the select, not from a
    /// correlated subquery, so **every post in the thread reports the same count** — the root's.
    /// A reply therefore carries a `reply_count` describing its parent, which is Go's answer and
    /// not an obvious one.
    ///
    /// # Three dynamic clauses, expressed as two statements
    ///
    /// Go builds the SQL with squirrel and this needs one literal per statement, so the cursor
    /// predicates and the limit are parameterised (identical truth tables, and each predicate
    /// stays visible to a mutation) while the `ORDER BY` is not. It cannot be: `direction == ""`
    /// emits **no** `ORDER BY` in Go, and a degenerate sort key is not the same thing — it would
    /// let Postgres reorder rows that Go returns in scan order. Hence the two statements below,
    /// which differ only in that clause. Within the ordered one, `descending` and `updates_only`
    /// select the key with `CASE`, which is safe because both arms are real columns.
    async fn get_thread_replies(
        &self,
        root_id: &str,
        opts: &GetPostThreadOptions<'_>,
    ) -> Result<Vec<Post>, StoreError> {
        let rows = if opts.direction == ThreadDirection::Unset {
            sqlx::query_as!(
                PostRow,
                r#"
                WITH replycount AS (
                    SELECT COUNT(*) AS num
                      FROM posts
                     WHERE posts.rootid = $1
                       AND posts.deleteat = 0
                )
                SELECT p.id,
                       p.createat     AS "create_at!",
                       p.updateat     AS "update_at!",
                       p.editat       AS "edit_at!",
                       p.deleteat     AS "delete_at!",
                       p.ispinned     AS "is_pinned!",
                       p.userid       AS "user_id!",
                       p.channelid    AS "channel_id!",
                       p.rootid       AS "root_id!",
                       p.originalid   AS "original_id!",
                       p.message      AS "message!",
                       p.type         AS "post_type!",
                       p.props        AS "props?",
                       p.hashtags     AS "hashtags!",
                       p.filenames    AS "filenames?",
                       p.fileids      AS "file_ids?",
                       p.hasreactions AS "has_reactions!",
                       p.remoteid     AS "remote_id?",
                       replycount.num AS "reply_count!"
                  FROM posts p, replycount
                 WHERE (p.id = $1 OR p.rootid = $1)
                   AND p.deleteat = 0
                   AND ($2::bigint = 0
                        OR CASE WHEN $3 THEN p.createat > $2 ELSE p.createat < $2 END
                        OR ($4 <> '' AND p.createat = $2
                            AND CASE WHEN $3 THEN p.id > $4 ELSE p.id < $4 END))
                   AND ($5::bigint = 0
                        OR CASE WHEN $3 THEN p.updateat > $5 ELSE p.updateat < $5 END
                        OR ($4 <> '' AND p.updateat = $5
                            AND CASE WHEN $3 THEN p.id > $4 ELSE p.id < $4 END))
                 LIMIT CASE WHEN $6::bigint <> 0 THEN $6::bigint + 1 END
                "#,
                root_id,
                opts.from_create_at,
                opts.direction.is_down(),
                opts.from_post,
                opts.from_update_at,
                opts.per_page,
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                PostRow,
                r#"
                WITH replycount AS (
                    SELECT COUNT(*) AS num
                      FROM posts
                     WHERE posts.rootid = $1
                       AND posts.deleteat = 0
                )
                SELECT p.id,
                       p.createat     AS "create_at!",
                       p.updateat     AS "update_at!",
                       p.editat       AS "edit_at!",
                       p.deleteat     AS "delete_at!",
                       p.ispinned     AS "is_pinned!",
                       p.userid       AS "user_id!",
                       p.channelid    AS "channel_id!",
                       p.rootid       AS "root_id!",
                       p.originalid   AS "original_id!",
                       p.message      AS "message!",
                       p.type         AS "post_type!",
                       p.props        AS "props?",
                       p.hashtags     AS "hashtags!",
                       p.filenames    AS "filenames?",
                       p.fileids      AS "file_ids?",
                       p.hasreactions AS "has_reactions!",
                       p.remoteid     AS "remote_id?",
                       replycount.num AS "reply_count!"
                  FROM posts p, replycount
                 WHERE (p.id = $1 OR p.rootid = $1)
                   AND p.deleteat = 0
                   AND ($2::bigint = 0
                        OR CASE WHEN $3 THEN p.createat > $2 ELSE p.createat < $2 END
                        OR ($4 <> '' AND p.createat = $2
                            AND CASE WHEN $3 THEN p.id > $4 ELSE p.id < $4 END))
                   AND ($5::bigint = 0
                        OR CASE WHEN $3 THEN p.updateat > $5 ELSE p.updateat < $5 END
                        OR ($4 <> '' AND p.updateat = $5
                            AND CASE WHEN $3 THEN p.id > $4 ELSE p.id < $4 END))
                 ORDER BY CASE WHEN $7 THEN NULL
                               ELSE (CASE WHEN $8 THEN p.updateat ELSE p.createat END) END ASC,
                          CASE WHEN $7 THEN NULL ELSE p.id END ASC,
                          CASE WHEN $7 THEN (CASE WHEN $8 THEN p.updateat ELSE p.createat END)
                               END DESC,
                          CASE WHEN $7 THEN p.id END DESC
                 LIMIT CASE WHEN $6::bigint <> 0 THEN $6::bigint + 1 END
                "#,
                root_id,
                opts.from_create_at,
                opts.direction.is_down(),
                opts.from_post,
                opts.from_update_at,
                opts.per_page,
                opts.direction.descending(),
                opts.updates_only,
            )
            .fetch_all(&self.pool)
            .await
        };

        rows.map_err(|source| StoreError::Db {
            context: "failed to find Posts".to_owned(),
            source,
        })?
        .into_iter()
        .map(post_from_row)
        .collect()
    }

    /// The reply fetch in `getPostWithCollapsedThreads` (post_store.go:652).
    ///
    /// Three differences from [`Self::get_thread_replies`], all of them on the wire:
    ///
    /// 1. **The window is `RootId = id` only.** The requested post is not in it — it was
    ///    fetched separately — and, more importantly, `id` is used *literally* rather than
    ///    resolved to a thread root. Asking for a **reply**'s thread with
    ///    `collapsedThreads=true` therefore returns the reply and nothing else, where the
    ///    non-collapsed branch returns the whole thread it belongs to.
    /// 2. **There is no `ReplyCount` column.** `postsQuery` selects the eighteen post columns
    ///    and stops, so every reply reports `reply_count: 0` — hard-coded here rather than
    ///    computed, because that is what Go puts on the wire.
    /// 3. **`from_update_at` is ignored unless `direction == Down`.** The non-collapsed branch
    ///    applies it in both directions. That is the `NOT $3 OR` below, and dropping it would
    ///    make an `up` request with `fromUpdateAt` return a filtered list where Go returns an
    ///    unfiltered one.
    async fn get_collapsed_thread_replies(
        &self,
        id: &str,
        opts: &GetPostThreadOptions<'_>,
    ) -> Result<Vec<Post>, StoreError> {
        let rows = if opts.direction == ThreadDirection::Unset {
            sqlx::query_as!(
                PostRow,
                r#"
                SELECT posts.id,
                       posts.createat     AS "create_at!",
                       posts.updateat     AS "update_at!",
                       posts.editat       AS "edit_at!",
                       posts.deleteat     AS "delete_at!",
                       posts.ispinned     AS "is_pinned!",
                       posts.userid       AS "user_id!",
                       posts.channelid    AS "channel_id!",
                       posts.rootid       AS "root_id!",
                       posts.originalid   AS "original_id!",
                       posts.message      AS "message!",
                       posts.type         AS "post_type!",
                       posts.props        AS "props?",
                       posts.hashtags     AS "hashtags!",
                       posts.filenames    AS "filenames?",
                       posts.fileids      AS "file_ids?",
                       posts.hasreactions AS "has_reactions!",
                       posts.remoteid     AS "remote_id?",
                       0::bigint          AS "reply_count!"
                  FROM posts
                 WHERE posts.rootid = $1
                   AND posts.deleteat = 0
                   AND ($2::bigint = 0
                        OR CASE WHEN $3 THEN posts.createat > $2 ELSE posts.createat < $2 END
                        OR ($4 <> '' AND posts.createat = $2
                            AND CASE WHEN $3 THEN posts.id > $4 ELSE posts.id < $4 END))
                   AND (NOT $3
                        OR $5::bigint = 0
                        OR posts.updateat > $5
                        OR ($4 <> '' AND posts.updateat = $5 AND posts.id > $4))
                 LIMIT CASE WHEN $6::bigint <> 0 THEN $6::bigint + 1 END
                "#,
                id,
                opts.from_create_at,
                opts.direction.is_down(),
                opts.from_post,
                opts.from_update_at,
                opts.per_page,
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                PostRow,
                r#"
                SELECT posts.id,
                       posts.createat     AS "create_at!",
                       posts.updateat     AS "update_at!",
                       posts.editat       AS "edit_at!",
                       posts.deleteat     AS "delete_at!",
                       posts.ispinned     AS "is_pinned!",
                       posts.userid       AS "user_id!",
                       posts.channelid    AS "channel_id!",
                       posts.rootid       AS "root_id!",
                       posts.originalid   AS "original_id!",
                       posts.message      AS "message!",
                       posts.type         AS "post_type!",
                       posts.props        AS "props?",
                       posts.hashtags     AS "hashtags!",
                       posts.filenames    AS "filenames?",
                       posts.fileids      AS "file_ids?",
                       posts.hasreactions AS "has_reactions!",
                       posts.remoteid     AS "remote_id?",
                       0::bigint          AS "reply_count!"
                  FROM posts
                 WHERE posts.rootid = $1
                   AND posts.deleteat = 0
                   AND ($2::bigint = 0
                        OR CASE WHEN $3 THEN posts.createat > $2 ELSE posts.createat < $2 END
                        OR ($4 <> '' AND posts.createat = $2
                            AND CASE WHEN $3 THEN posts.id > $4 ELSE posts.id < $4 END))
                   AND (NOT $3
                        OR $5::bigint = 0
                        OR posts.updateat > $5
                        OR ($4 <> '' AND posts.updateat = $5 AND posts.id > $4))
                 ORDER BY CASE WHEN $7 THEN NULL
                               ELSE (CASE WHEN $8 THEN posts.updateat
                                          ELSE posts.createat END) END ASC,
                          CASE WHEN $7 THEN NULL ELSE posts.id END ASC,
                          CASE WHEN $7 THEN (CASE WHEN $8 THEN posts.updateat
                                                  ELSE posts.createat END) END DESC,
                          CASE WHEN $7 THEN posts.id END DESC
                 LIMIT CASE WHEN $6::bigint <> 0 THEN $6::bigint + 1 END
                "#,
                id,
                opts.from_create_at,
                opts.direction.is_down(),
                opts.from_post,
                opts.from_update_at,
                opts.per_page,
                opts.direction.descending(),
                opts.updates_only,
            )
            .fetch_all(&self.pool)
            .await
        };

        rows.map_err(|source| StoreError::Db {
            context: format!("failed to find Posts for thread {id}"),
            source,
        })?
        .into_iter()
        .map(post_from_row)
        .collect()
    }
}

/// The eighteen selected columns plus the `ReplyCount` subquery, before the JSON columns are
/// decoded.
///
/// `pub(crate)` because Go hangs one more query returning exactly these columns off the
/// **channel** store — `SqlChannelStore.GetPinnedPosts` (channel_store.go:959) — and that port
/// follows Go's placement rather than moving the query here to keep the row type private.
pub(crate) struct PostRow {
    pub(crate) id: String,
    pub(crate) create_at: i64,
    pub(crate) update_at: i64,
    pub(crate) edit_at: i64,
    pub(crate) delete_at: i64,
    pub(crate) is_pinned: bool,
    pub(crate) user_id: String,
    pub(crate) channel_id: String,
    pub(crate) root_id: String,
    pub(crate) original_id: String,
    pub(crate) message: String,
    pub(crate) post_type: String,
    pub(crate) props: Option<serde_json::Value>,
    pub(crate) hashtags: String,
    pub(crate) filenames: Option<String>,
    pub(crate) file_ids: Option<String>,
    pub(crate) has_reactions: bool,
    pub(crate) remote_id: Option<String>,
    pub(crate) reply_count: i64,
}

/// Port of `postWithExtra` (post_store.go:43): the seventeen post columns plus the four the
/// collapsed-threads join adds.
///
/// Go embeds `model.Post` and lets `sqlx` scatter the columns across the embedded struct, so
/// `LastReplyAt` lands on the post itself while `ThreadReplyCount`, `ThreadParticipants` and
/// `IsFollowing` sit beside it until `processPost` moves them across. The flattening here is the
/// same shape without the embedding.
struct ThreadedPostRow {
    id: String,
    create_at: i64,
    update_at: i64,
    edit_at: i64,
    delete_at: i64,
    is_pinned: bool,
    user_id: String,
    channel_id: String,
    root_id: String,
    original_id: String,
    message: String,
    post_type: String,
    props: Option<serde_json::Value>,
    hashtags: String,
    filenames: Option<String>,
    file_ids: Option<String>,
    has_reactions: bool,
    remote_id: Option<String>,
    thread_reply_count: i64,
    last_reply_at: i64,
    thread_participants: serde_json::Value,
    is_following: Option<bool>,
}

/// Port of `prepareThreadedResponse`'s `processPost` closure (post_store.go:1288), for the
/// `extended == false` case.
///
/// Each participant is a `&model.User{Id: userId}` — an otherwise **zero** user, which
/// serialises with every one of `User`'s non-`omitempty` keys at its zero value. That is the
/// shape a collapsed-threads client receives unless it also asks for
/// `collapsedThreadsExtended`, and reproducing it means constructing the same zero value rather
/// than something tidier.
///
/// An empty participants array leaves the field **nil**, not `[]`: Go appends into a nil slice
/// and never allocates when there is nothing to append. `participants` carries no `omitempty`,
/// so that is `"participants":null` on the wire.
fn threaded_post_from_row(row: ThreadedPostRow) -> Result<Post, StoreError> {
    let participant_ids: Vec<String> =
        serde_json::from_value(row.thread_participants).map_err(|source| StoreError::Decode {
            entity: "Thread",
            column: "participants",
            source,
        })?;

    let mut post = post_from_row(PostRow {
        id: row.id,
        create_at: row.create_at,
        update_at: row.update_at,
        edit_at: row.edit_at,
        delete_at: row.delete_at,
        is_pinned: row.is_pinned,
        user_id: row.user_id,
        channel_id: row.channel_id,
        root_id: row.root_id,
        original_id: row.original_id,
        message: row.message,
        post_type: row.post_type,
        props: row.props,
        hashtags: row.hashtags,
        filenames: row.filenames,
        file_ids: row.file_ids,
        has_reactions: row.has_reactions,
        remote_id: row.remote_id,
        // `postWithExtra` has no `ReplyCount` column of its own; the thread's count is copied
        // over the post's in `processPost`, which is the line below.
        reply_count: 0,
    })?;

    post.reply_count = row.thread_reply_count;
    post.last_reply_at = row.last_reply_at;
    post.is_following = row.is_following;
    if !participant_ids.is_empty() {
        post.participants = Some(
            participant_ids
                .into_iter()
                .map(|id| User {
                    id,
                    ..User::default()
                })
                .collect(),
        );
    }

    Ok(post)
}

/// `StringArray.Scan` (model/utils.go:118): NULL stays nil, anything else is parsed as JSON.
///
/// The column is a `varchar` holding JSON text, not a `jsonb`, so the parse is ours to do.
fn decode_string_array(
    column: &'static str,
    raw: Option<String>,
) -> Result<Option<StringArray>, StoreError> {
    raw.map(|raw| serde_json::from_str::<StringArray>(&raw))
        .transpose()
        .map_err(|source| StoreError::Decode {
            entity: "Post",
            column,
            source,
        })
}

pub(crate) fn post_from_row(row: PostRow) -> Result<Post, StoreError> {
    // `StringInterface.Scan` on a JSON value that is not an object is an error in Go too —
    // `json.Unmarshal` into a `map[string]any` rejects an array or a scalar.
    let props = match row.props {
        // SQL NULL: sqlx handed `StringInterface.Scan` a freshly made empty map and it returned
        // without touching it. See the module docs — this is not the same as a JSON `null`.
        None => Some(StringInterface::new()),
        // jsonb `null`: `json.Unmarshal` sets the map back to nil, so this one really is nil.
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(map)) => Some(StringInterface::from_iter(map)),
        Some(other) => {
            return Err(StoreError::Decode {
                entity: "Post",
                column: "props",
                source: serde::de::Error::custom(format!(
                    "props is a {}, not an object",
                    match other {
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Number(_) => "number",
                        _ => "boolean",
                    }
                )),
            });
        }
    };

    Ok(Post {
        id: row.id,
        create_at: row.create_at,
        update_at: row.update_at,
        edit_at: row.edit_at,
        delete_at: row.delete_at,
        is_pinned: row.is_pinned,
        user_id: row.user_id,
        channel_id: row.channel_id,
        root_id: row.root_id,
        original_id: row.original_id,
        message: row.message,
        // Never selected — see the module docs.
        message_source: String::new(),
        post_type: row.post_type,
        props,
        hashtags: row.hashtags,
        filenames: decode_string_array("filenames", row.filenames)?.unwrap_or_default(),
        file_ids: decode_string_array("fileids", row.file_ids)?,
        pending_post_id: String::new(),
        has_reactions: row.has_reactions,
        remote_id: row.remote_id,
        reply_count: row.reply_count,
        last_reply_at: 0,
        participants: None,
        is_following: None,
        metadata: None,
    })
}

impl PostStore for SqlPostStore {
    /// Port of `SqlPostStore.GetPostsByIds` (post_store.go:2592).
    ///
    /// # There is no `DeleteAt` filter
    ///
    /// The only predicate is `p.Id IN (…)`. Every other multi-post read in this store excludes
    /// soft-deleted rows; this one does not, so `POST /api/v4/posts/ids` returns a deleted post
    /// with its `delete_at` set and its message intact. That is Go's answer, verified against the
    /// running server, and it is the single most surprising thing about the route.
    ///
    /// # `ReplyCount` is correlated, not a CTE
    ///
    /// `[`Self::get_thread_replies`]` computes one count for the whole thread; this computes one
    /// per row, resolving each post's own thread root first — `CASE WHEN p.RootId = '' THEN p.Id
    /// ELSE p.RootId END` — so a root reports its replies and a reply reports its parent's. The
    /// inner count *does* exclude deleted replies even though the outer query does not exclude
    /// deleted posts.
    ///
    /// # Zero rows is `ErrNotFound`
    ///
    /// Not an empty list (post_store.go:2604). The app layer turns it into a **404**, so a body
    /// naming only ids that exist nowhere is `app.post.get.app_error` rather than `[]`. An empty
    /// id list would reach the same place — squirrel renders `IN ()` as `(1=0)` here rather than
    /// as the syntax error `constructArrayArgs` produces elsewhere — but the handler's own
    /// `len == 0` check answers 400 first, so that path is unreachable from the wire.
    #[tracing::instrument(skip(self, post_ids), fields(asked = post_ids.len(), found))]
    async fn get_posts_by_ids(&self, post_ids: &[String]) -> Result<Vec<Post>, StoreError> {
        let rows = sqlx::query_as!(
            PostRow,
            r#"
            SELECT p.id,
                   p.createat     AS "create_at!",
                   p.updateat     AS "update_at!",
                   p.editat       AS "edit_at!",
                   p.deleteat     AS "delete_at!",
                   p.ispinned     AS "is_pinned!",
                   p.userid       AS "user_id!",
                   p.channelid    AS "channel_id!",
                   p.rootid       AS "root_id!",
                   p.originalid   AS "original_id!",
                   p.message      AS "message!",
                   p.type         AS "post_type!",
                   p.props        AS "props?",
                   p.hashtags     AS "hashtags!",
                   p.filenames    AS "filenames?",
                   p.fileids      AS "file_ids?",
                   p.hasreactions AS "has_reactions!",
                   p.remoteid     AS "remote_id?",
                   (SELECT count(*)
                      FROM posts r
                     WHERE r.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                       AND r.deleteat = 0) AS "reply_count!"
              FROM posts p
             WHERE p.id = ANY($1::text[])
             ORDER BY p.createat DESC
            "#,
            post_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Posts".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

        if rows.is_empty() {
            return Err(StoreError::NotFound {
                entity: "Post",
                criteria: format!("postIds={post_ids:?}"),
            });
        }

        rows.into_iter().map(post_from_row).collect()
    }

    #[tracing::instrument(skip(self), fields(post_id = %id, incl_deleted))]
    async fn get_single(&self, id: &str, incl_deleted: bool) -> Result<Post, StoreError> {
        // Go appends `AND Posts.DeleteAt = 0` to the builder only when `!inclDeleted`. A
        // compile-checked macro needs one literal statement, so the branch is expressed as a
        // parameter instead: `incl_deleted OR deleteat = 0` has the identical truth table, and
        // the predicate stays visible to a mutation.
        let row = sqlx::query_as!(
            PostRow,
            r#"
            SELECT posts.id,
                   posts.createat   AS "create_at!",
                   posts.updateat   AS "update_at!",
                   posts.editat     AS "edit_at!",
                   posts.deleteat   AS "delete_at!",
                   posts.ispinned   AS "is_pinned!",
                   posts.userid     AS "user_id!",
                   posts.channelid  AS "channel_id!",
                   posts.rootid     AS "root_id!",
                   posts.originalid AS "original_id!",
                   posts.message    AS "message!",
                   posts.type       AS "post_type!",
                   posts.props      AS "props?",
                   posts.hashtags   AS "hashtags!",
                   posts.filenames  AS "filenames?",
                   posts.fileids    AS "file_ids?",
                   posts.hasreactions AS "has_reactions!",
                   posts.remoteid   AS "remote_id?",
                   (SELECT COUNT(*)
                      FROM posts p
                     WHERE p.rootid = (CASE WHEN posts.rootid = '' THEN posts.id ELSE posts.rootid END)
                       AND p.deleteat = 0) AS "reply_count!"
              FROM posts
             WHERE posts.id = $1
               AND ($2 OR posts.deleteat = 0)
            "#,
            id,
            incl_deleted
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Post with id={id}"),
            source,
        })?;

        let Some(row) = row else {
            return Err(StoreError::NotFound {
                entity: "Post",
                criteria: id.to_owned(),
            });
        };

        post_from_row(row)
    }

    /// **No row is not an error.** Go's app layer swallows `sql.ErrNoRows` specifically
    /// (`post_priority.go:24`) and returns `(nil, nil)`, so a post with no priority row leaves
    /// `metadata.priority` unset rather than failing the request.
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn get_priority_for_post(
        &self,
        post_id: &str,
    ) -> Result<Option<PostPriority>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT postid                  AS "post_id!",
                   channelid               AS "channel_id!",
                   priority                AS "priority!",
                   requestedack            AS "requested_ack?",
                   persistentnotifications AS "persistent_notifications?"
              FROM postspriority
             WHERE postid = $1
            "#,
            post_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get PostPriority with postId={post_id}"),
            source,
        })?;

        Ok(row.map(|row| PostPriority {
            // `Priority` is a `*string` in Go over a NOT NULL column, so it is always non-nil
            // here — and the tag has no `omitempty`, so it reaches the wire either way.
            priority: Some(row.priority),
            requested_ack: row.requested_ack,
            persistent_notifications: row.persistent_notifications,
            post_id: row.post_id,
            channel_id: row.channel_id,
        }))
    }

    /// The `AcknowledgedAt != 0` predicate is Go's soft delete: unacknowledging writes `0`
    /// rather than deleting the row (post_acknowledgements_store.go:127). Dropping it would
    /// resurrect every acknowledgement a user has withdrawn.
    ///
    /// **Go issues no `ORDER BY`**, so the row order is Postgres's own. Reproduced as-is; a
    /// post with two or more acknowledgements is therefore not guaranteed to serialise in the
    /// same order on both servers. See the parity note in `MIGRATION.md`.
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn get_acknowledgements_for_post(
        &self,
        post_id: &str,
    ) -> Result<Vec<PostAcknowledgement>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT postid         AS "post_id!",
                   userid         AS "user_id!",
                   channelid      AS "channel_id?",
                   acknowledgedat AS "acknowledged_at!",
                   remoteid       AS "remote_id?"
              FROM postacknowledgements
             WHERE acknowledgedat != 0
               AND postid = $1
            "#,
            post_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get PostAcknowledgements for postID={post_id}"),
            source,
        })?;

        Ok(rows
            .into_iter()
            .map(|row| PostAcknowledgement {
                user_id: row.user_id,
                post_id: row.post_id,
                acknowledged_at: row.acknowledged_at,
                channel_id: row.channel_id.unwrap_or_default(),
                remote_id: row.remote_id,
            })
            .collect())
    }

    /// The two branches are Go's, and so is the asymmetry between them: the non-collapsed one
    /// ends with `MakeNonNil`, the collapsed one does not.
    ///
    /// That is not cosmetic. `Post.MakeNonNil` materialises a nil `Props` into an empty map, and
    /// `props` has no `omitempty` — so the same post with no props reaches the client as
    /// `"props":{}` from a plain page and `"props":null` from a collapsed-threads page.
    ///
    /// Go runs the two non-collapsed queries in parallel goroutines and this awaits them in
    /// sequence. Nothing observable turns on it: neither query writes, and the second's result
    /// is merged into a map keyed by id.
    ///
    /// **Go's `PerPage > 1000` guard is not reproduced.** It returns `ErrInvalidInput`, which the
    /// app layer turns into a 400, and it is unreachable through this route: `parse_per_page`
    /// clamps to `PerPageMaximum` (200) before the value gets here. A second caller with an
    /// unclamped `per_page` would need it, and would need a `StoreError` variant that maps to
    /// 400 to carry it.
    #[tracing::instrument(skip(self), fields(channel_id = %opts.channel_id, collapsed = opts.collapsed_threads))]
    async fn get_posts(&self, opts: GetPostsOptions<'_>) -> Result<PostList, StoreError> {
        if opts.collapsed_threads {
            return self.get_posts_collapsed_threads(opts).await;
        }

        let posts = self.get_root_posts(opts).await?;
        let parents = self.get_parents_posts(opts).await?;

        let mut list = PostList::new();
        for post in posts {
            let id = post.id.clone();
            list.add_post(post);
            list.add_order(id);
        }
        // Parents reach `posts` and never `order` — see `get_parents_posts`.
        for post in parents {
            list.add_post(post);
        }
        list.make_non_nil();

        Ok(list)
    }

    #[tracing::instrument(skip(self), fields(channel_id = %channel_id))]
    async fn get_etag(&self, channel_id: &str) -> Option<i64> {
        let row = sqlx::query!(
            r#"
            SELECT updateat AS "update_at!"
              FROM posts
             WHERE channelid = $1
             ORDER BY updateat DESC
             LIMIT 1
            "#,
            channel_id
        )
        .fetch_optional(&self.pool)
        .await;

        match row {
            Ok(row) => row.map(|row| row.update_at),
            // Go's `if err != nil` covers this and the empty channel alike; both fall to the
            // clock-stamped etag the caller builds from `None`.
            Err(err) => {
                tracing::warn!(error = %err, channel_id, "post etag lookup failed");
                None
            }
        }
    }

    /// Two literal statements rather than one with a swapped comparison and sort direction: the
    /// direction *is* the behaviour here, and a parameterised `ORDER BY` would hide the half a
    /// mutation needs to be able to flip.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, time, before, collapsed_threads))]
    async fn get_post_id_around_time(
        &self,
        channel_id: &str,
        time: i64,
        before: bool,
        collapsed_threads: bool,
    ) -> Result<String, StoreError> {
        // `sq.Lt`/`sq.Gt` are strict, so a post created in the same millisecond as the cursor is
        // in neither direction and is skipped by both.
        let found = if before {
            sqlx::query_scalar!(
                r#"
                SELECT id AS "id!"
                  FROM posts
                 WHERE createat < $2
                   AND channelid = $1
                   AND deleteat = 0
                   AND (NOT $3 OR rootid = '')
                 ORDER BY createat DESC
                 LIMIT 1
                "#,
                channel_id,
                time,
                collapsed_threads,
            )
            .fetch_optional(&self.pool)
            .await
        } else {
            sqlx::query_scalar!(
                r#"
                SELECT id AS "id!"
                  FROM posts
                 WHERE createat > $2
                   AND channelid = $1
                   AND deleteat = 0
                   AND (NOT $3 OR rootid = '')
                 ORDER BY createat ASC
                 LIMIT 1
                "#,
                channel_id,
                time,
                collapsed_threads,
            )
            .fetch_optional(&self.pool)
            .await
        };

        // Go returns `("", nil)` for no rows and only wraps a real driver error.
        Ok(found
            .map_err(|source| StoreError::Db {
                context: format!("failed to get Post id with channelId={channel_id}"),
                source,
            })?
            .unwrap_or_default())
    }

    /// Port of `SqlPostStore.GetEditHistoryForPost` (post_store.go:2610).
    ///
    /// # The link is `OriginalId`, not `RootId`
    ///
    /// Editing a post in Mattermost **inserts a copy of the old version** carrying
    /// `OriginalId = <the live post's id>`, so the history is a set of tombstoned siblings rather
    /// than a chain. `postsQuery` selects the eighteen plain columns and no reply-count subquery,
    /// so every row comes back with `reply_count: 0`.
    ///
    /// # Zero rows is a 404, not an empty list
    ///
    /// Go raises `store.NewErrNotFound` for an empty result, which the app layer turns into a
    /// 404. A post that has simply never been edited is therefore indistinguishable, over HTTP,
    /// from one that does not exist — and the handler's own permission block has already made
    /// sure the caller could have seen it either way.
    ///
    /// `ORDER BY EditAt DESC` — most recent edit first. There is no `DeleteAt = 0` here: the
    /// history rows are themselves deleted copies, so filtering them out would empty every
    /// answer.
    #[tracing::instrument(skip(self), fields(post_id = %post_id, count))]
    async fn get_edit_history_for_post(&self, post_id: &str) -> Result<Vec<Post>, StoreError> {
        let rows = sqlx::query_as!(
            PostRow,
            r#"
            SELECT posts.id,
                   posts.createat     AS "create_at!",
                   posts.updateat     AS "update_at!",
                   posts.editat       AS "edit_at!",
                   posts.deleteat     AS "delete_at!",
                   posts.ispinned     AS "is_pinned!",
                   posts.userid       AS "user_id!",
                   posts.channelid    AS "channel_id!",
                   posts.rootid       AS "root_id!",
                   posts.originalid   AS "original_id!",
                   posts.message      AS "message!",
                   posts.type         AS "post_type!",
                   posts.props        AS "props?",
                   posts.hashtags     AS "hashtags!",
                   posts.filenames    AS "filenames?",
                   posts.fileids      AS "file_ids?",
                   posts.hasreactions AS "has_reactions!",
                   posts.remoteid     AS "remote_id?",
                   0::bigint          AS "reply_count!"
              FROM posts
             WHERE posts.originalid = $1
             ORDER BY posts.editat DESC
            "#,
            post_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("error getting posts edit history with postId={post_id}"),
            source,
        })?;

        tracing::Span::current().record("count", rows.len());

        if rows.is_empty() {
            return Err(StoreError::NotFound {
                entity: "failed to find post history",
                criteria: post_id.to_owned(),
            });
        }

        rows.into_iter().map(post_from_row).collect()
    }

    /// Port of `SqlPostStore.getPostsAround` (post_store.go:1701), reached as `GetPostsBefore`
    /// and `GetPostsAfter`.
    ///
    /// # `before` decides three things, and they cancel out
    ///
    /// The comparison (`<` / `>`), the `ORDER BY` (`DESC` / `ASC`) — and then
    /// `prepareThreadedResponse`'s `reversed` flag, which Go passes as **`!before`**. So the
    /// *after* window is selected oldest-first and then walked backwards, and both directions
    /// hand back a list ordered newest-first. Getting any one of the three wrong flips the
    /// response's `order`.
    ///
    /// # The cursor is a subquery, so an unknown `post_id` is empty rather than an error
    ///
    /// `CreateAt < (SELECT CreateAt FROM Posts WHERE Id = ?)` yields NULL for an id that names
    /// nothing, the comparison is NULL, no row matches, and the answer is an empty list.
    ///
    /// # `reply_count` is computed, zeroed, and then restored — all three steps are Go's
    ///
    /// `getPostsAround` scans into `postWithExtra`, which embeds `model.Post`; the non-collapsed
    /// query aliases its subquery `ReplyCount`, which sqlx maps onto the **embedded**
    /// `Post.ReplyCount`. Then `processPost` runs `p.Post.ReplyCount = p.ThreadReplyCount`
    /// unconditionally (post_store.go:1288), and `ThreadReplyCount` is selected only on the
    /// *collapsed* branch — so every non-collapsed window post leaves that function reporting
    /// zero replies.
    ///
    /// And then the parents pass puts it back. [`Self::get_posts_around_parents`] re-fetches
    /// every window post — its id list is built from each post's **own** id, not just its root —
    /// into a plain row type `processPost` never touches, and `AddPost` overwrites the map entry.
    /// So the zeroing is **unobservable through this route**, and a client sees real counts.
    ///
    /// All three steps are reproduced rather than cancelled out on paper, because only the third
    /// one is load-bearing by accident: narrow the parents query and the zero becomes visible.
    /// Measured against the running server — the parity suite predicted zeroes and Go answered
    /// the real count.
    #[tracing::instrument(skip(self), fields(channel_id = %opts.channel_id, before, collapsed = opts.collapsed_threads))]
    async fn get_posts_around(
        &self,
        opts: GetPostsAroundOptions<'_>,
        before: bool,
    ) -> Result<PostList, StoreError> {
        // `burnOnReadVisibleCondition` (post_store.go:1869) stamps `model.GetMillis()` into the
        // SQL as a literal; binding it is the same value read at the same moment. Go applies the
        // condition whenever `isBurnOnReadEnabled()`, which is the default — see
        // `crate::post_store`'s sibling `get_visible_post_id_around_time`.
        let now = mm_model::utils::get_millis();
        let burn_on_read = mm_model::post::POST_TYPE_BURN_ON_READ;

        let mut list = PostList::new();

        if opts.collapsed_threads {
            let rows = if before {
                sqlx::query_as!(
                    ThreadedPostRow,
                    r#"
                SELECT
                       p.id,
                       p.createat     AS "create_at!",
                       p.updateat     AS "update_at!",
                       p.editat       AS "edit_at!",
                       p.deleteat     AS "delete_at!",
                       p.ispinned     AS "is_pinned!",
                       p.userid       AS "user_id!",
                       p.channelid    AS "channel_id!",
                       p.rootid       AS "root_id!",
                       p.originalid   AS "original_id!",
                       p.message      AS "message!",
                       p.type         AS "post_type!",
                       p.props        AS "props?",
                       p.hashtags     AS "hashtags!",
                       p.filenames    AS "filenames?",
                       p.fileids      AS "file_ids?",
                       p.hasreactions AS "has_reactions!",
                       p.remoteid     AS "remote_id?",
                       COALESCE(threads.replycount, 0)     AS "thread_reply_count!",
                       COALESCE(threads.lastreplyat, 0)    AS "last_reply_at!",
                       COALESCE(threads.participants, '[]') AS "thread_participants!",
                       threadmemberships.following         AS "is_following?"
                  FROM posts p
                  LEFT JOIN threads ON threads.postid = p.id
                  LEFT JOIN threadmemberships
                         ON threadmemberships.postid = p.id
                        AND threadmemberships.userid = $3
                 WHERE p.createat < (SELECT createat FROM posts WHERE id = $2)
                   AND p.channelid = $1
                   AND (p.type <> $4 OR p.userid = $3
                        OR NOT EXISTS (SELECT 1
                                         FROM readreceipts rr
                                        WHERE rr.postid = p.id
                                          AND rr.userid = $3
                                          AND rr.expireat < $5))
                   AND p.deleteat = 0
                   AND p.rootid = ''
                 ORDER BY p.createat DESC
                 LIMIT $6 OFFSET $7
                "#,
                    opts.channel_id,
                    opts.post_id,
                    opts.user_id,
                    burn_on_read,
                    now,
                    opts.per_page,
                    opts.offset(),
                )
                .fetch_all(&self.pool)
                .await
            } else {
                sqlx::query_as!(
                    ThreadedPostRow,
                    r#"
                SELECT
                       p.id,
                       p.createat     AS "create_at!",
                       p.updateat     AS "update_at!",
                       p.editat       AS "edit_at!",
                       p.deleteat     AS "delete_at!",
                       p.ispinned     AS "is_pinned!",
                       p.userid       AS "user_id!",
                       p.channelid    AS "channel_id!",
                       p.rootid       AS "root_id!",
                       p.originalid   AS "original_id!",
                       p.message      AS "message!",
                       p.type         AS "post_type!",
                       p.props        AS "props?",
                       p.hashtags     AS "hashtags!",
                       p.filenames    AS "filenames?",
                       p.fileids      AS "file_ids?",
                       p.hasreactions AS "has_reactions!",
                       p.remoteid     AS "remote_id?",
                       COALESCE(threads.replycount, 0)     AS "thread_reply_count!",
                       COALESCE(threads.lastreplyat, 0)    AS "last_reply_at!",
                       COALESCE(threads.participants, '[]') AS "thread_participants!",
                       threadmemberships.following         AS "is_following?"
                  FROM posts p
                  LEFT JOIN threads ON threads.postid = p.id
                  LEFT JOIN threadmemberships
                         ON threadmemberships.postid = p.id
                        AND threadmemberships.userid = $3
                 WHERE p.createat > (SELECT createat FROM posts WHERE id = $2)
                   AND p.channelid = $1
                   AND (p.type <> $4 OR p.userid = $3
                        OR NOT EXISTS (SELECT 1
                                         FROM readreceipts rr
                                        WHERE rr.postid = p.id
                                          AND rr.userid = $3
                                          AND rr.expireat < $5))
                   AND p.deleteat = 0
                   AND p.rootid = ''
                 ORDER BY p.createat ASC
                 LIMIT $6 OFFSET $7
                "#,
                    opts.channel_id,
                    opts.post_id,
                    opts.user_id,
                    burn_on_read,
                    now,
                    opts.per_page,
                    opts.offset(),
                )
                .fetch_all(&self.pool)
                .await
            }
            .map_err(|source| StoreError::Db {
                context: format!("failed to find Posts with channelId={}", opts.channel_id),
                source,
            })?;

            // `prepareThreadedResponse`'s `reversed` is `!before`, so the *after* window — read
            // oldest-first — is walked backwards and both directions end newest-first.
            let mut posts = rows
                .into_iter()
                .map(threaded_post_from_row)
                .collect::<Result<Vec<_>, _>>()?;
            if !before {
                posts.reverse();
            }
            for post in posts {
                let id = post.id.clone();
                list.add_post(post);
                list.add_order(id);
            }
            return Ok(list);
        }

        let rows = if before {
            sqlx::query_as!(
                PostRow,
                r#"
                SELECT
                       p.id,
                       p.createat     AS "create_at!",
                       p.updateat     AS "update_at!",
                       p.editat       AS "edit_at!",
                       p.deleteat     AS "delete_at!",
                       p.ispinned     AS "is_pinned!",
                       p.userid       AS "user_id!",
                       p.channelid    AS "channel_id!",
                       p.rootid       AS "root_id!",
                       p.originalid   AS "original_id!",
                       p.message      AS "message!",
                       p.type         AS "post_type!",
                       p.props        AS "props?",
                       p.hashtags     AS "hashtags!",
                       p.filenames    AS "filenames?",
                       p.fileids      AS "file_ids?",
                       p.hasreactions AS "has_reactions!",
                       p.remoteid     AS "remote_id?",
                       (SELECT COUNT(*)
                          FROM posts sub
                         WHERE sub.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                           AND sub.deleteat = 0) AS "reply_count!"
                  FROM posts p
                 WHERE p.createat < (SELECT createat FROM posts WHERE id = $2)
                   AND p.channelid = $1
                   AND (p.type <> $4 OR p.userid = $3
                        OR NOT EXISTS (SELECT 1
                                         FROM readreceipts rr
                                        WHERE rr.postid = p.id
                                          AND rr.userid = $3
                                          AND rr.expireat < $5))
                   AND p.deleteat = 0
                 ORDER BY p.createat DESC
                 LIMIT $6 OFFSET $7
                "#,
                opts.channel_id,
                opts.post_id,
                opts.user_id,
                burn_on_read,
                now,
                opts.per_page,
                opts.offset(),
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                PostRow,
                r#"
                SELECT
                       p.id,
                       p.createat     AS "create_at!",
                       p.updateat     AS "update_at!",
                       p.editat       AS "edit_at!",
                       p.deleteat     AS "delete_at!",
                       p.ispinned     AS "is_pinned!",
                       p.userid       AS "user_id!",
                       p.channelid    AS "channel_id!",
                       p.rootid       AS "root_id!",
                       p.originalid   AS "original_id!",
                       p.message      AS "message!",
                       p.type         AS "post_type!",
                       p.props        AS "props?",
                       p.hashtags     AS "hashtags!",
                       p.filenames    AS "filenames?",
                       p.fileids      AS "file_ids?",
                       p.hasreactions AS "has_reactions!",
                       p.remoteid     AS "remote_id?",
                       (SELECT COUNT(*)
                          FROM posts sub
                         WHERE sub.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                           AND sub.deleteat = 0) AS "reply_count!"
                  FROM posts p
                 WHERE p.createat > (SELECT createat FROM posts WHERE id = $2)
                   AND p.channelid = $1
                   AND (p.type <> $4 OR p.userid = $3
                        OR NOT EXISTS (SELECT 1
                                         FROM readreceipts rr
                                        WHERE rr.postid = p.id
                                          AND rr.userid = $3
                                          AND rr.expireat < $5))
                   AND p.deleteat = 0
                 ORDER BY p.createat ASC
                 LIMIT $6 OFFSET $7
                "#,
                opts.channel_id,
                opts.post_id,
                opts.user_id,
                burn_on_read,
                now,
                opts.per_page,
                opts.offset(),
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Posts with channelId={}", opts.channel_id),
            source,
        })?;

        let mut posts = rows
            .into_iter()
            .map(post_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        // `processPost` overwrites the count the subquery just produced with the zero
        // `ThreadReplyCount` — and the parents pass below puts it back. See the doc comment;
        // neither step is a simplification to remove.
        for post in &mut posts {
            post.reply_count = 0;
        }
        if !before {
            posts.reverse();
        }

        // The window's ids, in the order `AddOrder` will see them.
        let mut root_ids: Vec<String> = Vec::with_capacity(posts.len() * 2);
        for post in &posts {
            root_ids.push(post.id.clone());
            if !post.root_id.is_empty() {
                root_ids.push(post.root_id.clone());
            }
        }

        for post in posts {
            let id = post.id.clone();
            list.add_post(post);
            list.add_order(id);
        }

        // `if !options.CollapsedThreads && len(posts) > 0` — the parents query does not run for
        // an empty window, which matters because `Id IN ()` would otherwise match nothing and
        // cost a round trip.
        if root_ids.is_empty() {
            return Ok(list);
        }

        let parents = self.get_posts_around_parents(&opts, &root_ids).await?;
        // `AddPost` and **not** `AddOrder`: these land in `posts` without appearing in `order`,
        // exactly as in `getParentsPosts`.
        for post in parents {
            list.add_post(post);
        }

        Ok(list)
    }

    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, time, before, collapsed_threads))]
    async fn get_visible_post_id_around_time(
        &self,
        channel_id: &str,
        time: i64,
        before: bool,
        collapsed_threads: bool,
        user_id: &str,
    ) -> Result<String, StoreError> {
        // Go stamps `model.GetMillis()` into the SQL as a literal at build time; binding it is
        // the same value, read at the same moment.
        let now = mm_model::utils::get_millis();
        let burn_on_read = mm_model::post::POST_TYPE_BURN_ON_READ;

        let found = if before {
            sqlx::query_scalar!(
                r#"
                SELECT posts.id AS "id!"
                  FROM posts
                 WHERE posts.createat < $2
                   AND posts.channelid = $1
                   AND posts.deleteat = 0
                   AND (posts.type != $5
                        OR posts.userid = $4
                        OR NOT EXISTS (SELECT 1
                                         FROM readreceipts rr
                                        WHERE rr.postid = posts.id
                                          AND rr.userid = $4
                                          AND rr.expireat < $6))
                   AND (NOT $3 OR posts.rootid = '')
                 ORDER BY posts.createat DESC
                 LIMIT 1
                "#,
                channel_id,
                time,
                collapsed_threads,
                user_id,
                burn_on_read,
                now,
            )
            .fetch_optional(&self.pool)
            .await
        } else {
            sqlx::query_scalar!(
                r#"
                SELECT posts.id AS "id!"
                  FROM posts
                 WHERE posts.createat > $2
                   AND posts.channelid = $1
                   AND posts.deleteat = 0
                   AND (posts.type != $5
                        OR posts.userid = $4
                        OR NOT EXISTS (SELECT 1
                                         FROM readreceipts rr
                                        WHERE rr.postid = posts.id
                                          AND rr.userid = $4
                                          AND rr.expireat < $6))
                   AND (NOT $3 OR posts.rootid = '')
                 ORDER BY posts.createat ASC
                 LIMIT 1
                "#,
                channel_id,
                time,
                collapsed_threads,
                user_id,
                burn_on_read,
                now,
            )
            .fetch_optional(&self.pool)
            .await
        };

        Ok(found
            .map_err(|source| StoreError::Db {
                context: format!("failed to get visible Post id with channelId={channel_id}"),
                source,
            })?
            .unwrap_or_default())
    }

    /// Go pages the `IN (…)` list 200 ids at a time and concatenates the batches; `= ANY` needs
    /// no such loop. The two agree as long as a single page is at most 200 ids — which it is,
    /// because `per_page` is clamped to `PerPageMaximum` before the page query runs, and this
    /// list is that page's `order`. A larger caller would see a different **row order** from a
    /// single query than from concatenated batches; nothing else changes.
    #[tracing::instrument(skip_all, fields(posts = post_ids.len()))]
    async fn get_priority_for_posts(
        &self,
        post_ids: &[String],
    ) -> Result<Vec<PostPriority>, StoreError> {
        // Go's loop body never executes for an empty list, so no query is issued at all.
        if post_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT postid                  AS "post_id!",
                   channelid               AS "channel_id!",
                   priority                AS "priority!",
                   requestedack            AS "requested_ack?",
                   persistentnotifications AS "persistent_notifications?"
              FROM postspriority
             WHERE postid = ANY($1::varchar[])
            "#,
            post_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get PostPriority for post list".to_owned(),
            source,
        })?;

        Ok(rows
            .into_iter()
            .map(|row| PostPriority {
                priority: Some(row.priority),
                requested_ack: row.requested_ack,
                persistent_notifications: row.persistent_notifications,
                post_id: row.post_id,
                channel_id: row.channel_id,
            })
            .collect())
    }

    /// `AcknowledgedAt != 0` is the soft delete, exactly as in the single-post read beside it.
    ///
    /// **No `ORDER BY`, in Go or here**, so two acknowledgements on one post are serialised in
    /// whatever order Postgres returns them.
    #[tracing::instrument(skip_all, fields(posts = post_ids.len()))]
    async fn get_acknowledgements_for_posts(
        &self,
        post_ids: &[String],
    ) -> Result<Vec<PostAcknowledgement>, StoreError> {
        if post_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT postid         AS "post_id!",
                   userid         AS "user_id!",
                   channelid      AS "channel_id?",
                   acknowledgedat AS "acknowledged_at!",
                   remoteid       AS "remote_id?"
              FROM postacknowledgements
             WHERE postid = ANY($1::varchar[])
               AND acknowledgedat != 0
            "#,
            post_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get PostAcknowledgements for post list".to_owned(),
            source,
        })?;

        Ok(rows
            .into_iter()
            .map(|row| PostAcknowledgement {
                user_id: row.user_id,
                post_id: row.post_id,
                acknowledged_at: row.acknowledged_at,
                channel_id: row.channel_id.unwrap_or_default(),
                remote_id: row.remote_id,
            })
            .collect())
    }

    /// # `has_next` has three states and only two of them are a boolean
    ///
    /// The collapsed branch always sets it, so `collapsedThreads=true` puts `"has_next":false`
    /// on the wire even for an unpaginated request. The non-collapsed branch sets it **inside**
    /// the `!skipFetchThreads` block, so `skipFetchThreads=true` omits the key entirely — the
    /// field is a `*bool` with `omitempty`. Same route, same list type, three answers.
    ///
    /// # The `id == ""` guard is not ported
    ///
    /// Go opens both branches with `store.NewErrInvalidInput("Post", "id", "")`, which the app
    /// layer turns into a 400. `RequirePostId` has already answered that with its own 400 before
    /// the handler runs, so the branch is unreachable through this route and reproducing it
    /// would need a `StoreError` variant with no other caller — the same call
    /// [`PostStore::get_posts`] makes about `PerPage > 1000`.
    ///
    /// # Go's explicit `BurnOnReadPosts` filing is redundant and is not reproduced
    ///
    /// The non-collapsed loop files a burn-on-read reply into `pl.BurnOnReadPosts` **before**
    /// skipping the duplicate at `p.Id == id` (post_store.go:891). The only post that skip can
    /// reach is the requested one, and it was filed a few lines earlier by `AddPost` — which
    /// files burn-on-read posts itself. So the explicit line can only ever re-file a post that
    /// is already there. The map is `json:"-"` in any case, and `mm_api::posts` forwards every
    /// list carrying a burn-on-read post because the metadata pipeline refuses it.
    #[tracing::instrument(skip(self), fields(post_id = %id, collapsed = opts.collapsed_threads))]
    async fn get_thread(
        &self,
        id: &str,
        opts: GetPostThreadOptions<'_>,
    ) -> Result<PostList, StoreError> {
        let not_found = || StoreError::NotFound {
            entity: "Post",
            criteria: format!("id={id}"),
        };

        if opts.collapsed_threads {
            let root = self
                .get_collapsed_thread_root(id, opts.user_id)
                .await?
                .ok_or_else(not_found)?;

            // Go runs this query before it builds the list, and the order matters only because
            // a failure here must not leave a half-built response. Both are reads.
            let mut replies = self.get_collapsed_thread_replies(id, &opts).await?;
            let has_next = shave_extra_row(&mut replies, opts.per_page);

            let mut list = PostList::new();
            let root_id = root.id.clone();
            list.add_post(root);
            list.add_order(root_id);
            for reply in replies {
                let reply_id = reply.id.clone();
                list.add_post(reply);
                list.add_order(reply_id);
            }
            list.has_next = Some(has_next);
            return Ok(list);
        }

        let post = self.get_thread_root(id).await?.ok_or_else(not_found)?;

        let mut list = PostList::new();
        // `rootId := post.RootId; if rootId == "" { rootId = post.Id }` — a root post is its own
        // thread. Go then guards `rootId == ""` a second time and returns `errors.Wrapf(err,
        // ...)` on a **nil** `err`, which `Wrapf` turns into a nil error: the branch is
        // unreachable (an id that fetched a row is not empty) and would return `nil, nil` if it
        // were.
        let root_id = if post.root_id.is_empty() {
            post.id.clone()
        } else {
            post.root_id.clone()
        };
        let post_id = post.id.clone();
        list.add_post(post);
        list.add_order(post_id);

        if opts.skip_fetch_threads {
            return Ok(list);
        }

        let mut replies = self.get_thread_replies(&root_id, &opts).await?;
        // Counted before the duplicate is dropped, because Go counts it too: a page whose extra
        // row *is* the requested post still reports `has_next: true` while returning one fewer
        // reply than asked for.
        let has_next = shave_extra_row(&mut replies, opts.per_page);

        for reply in replies {
            // The window is `p.Id = rootId OR p.RootId = rootId`, so it contains the requested
            // post whenever that post is the thread root — already added above.
            if reply.id == id {
                continue;
            }
            let reply_id = reply.id.clone();
            list.add_post(reply);
            list.add_order(reply_id);
        }
        list.has_next = Some(has_next);

        Ok(list)
    }
}

/// Go's `hasNext` block, which both branches of `SqlPostStore.Get` repeat verbatim
/// (post_store.go:723 and :880).
///
/// The query asked for `per_page + 1` rows; getting exactly that many means there is another
/// page. Note the test is `==` and not `>=`, and that `per_page == 0` — which emitted no `LIMIT`
/// at all — reports `false` however many rows came back.
///
/// **A negative `per_page` is where this and Go part company.** `Limit(uint64(perPage + 1))`
/// makes `-1` a `LIMIT 0`, so Go matches zero rows against `perPage+1 == 0`, sets `hasNext` and
/// then panics on `posts[:len(posts)-1]`. `Vec::pop` on an empty vector is `None`, so this
/// returns an empty list instead. `mm_api::posts` forwards a negative `perPage` rather than
/// answering a request Go cannot.
fn shave_extra_row(posts: &mut Vec<Post>, per_page: i64) -> bool {
    let has_next = per_page != 0 && posts.len() as i64 == per_page + 1;
    if has_next {
        posts.pop();
    }
    has_next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> PostRow {
        PostRow {
            id: "post00000000000000000000000".to_owned(),
            create_at: 1,
            update_at: 2,
            edit_at: 3,
            delete_at: 0,
            is_pinned: true,
            user_id: "user".to_owned(),
            channel_id: "chan".to_owned(),
            root_id: String::new(),
            original_id: String::new(),
            message: "hi".to_owned(),
            post_type: String::new(),
            props: Some(serde_json::json!({"a": 1})),
            hashtags: "#x".to_owned(),
            filenames: Some("[\"a.txt\"]".to_owned()),
            file_ids: Some("[\"fileid\"]".to_owned()),
            has_reactions: false,
            remote_id: Some(String::new()),
            reply_count: 7,
        }
    }

    /// The six fields the query never selects must come back at Go's zero values, because those
    /// zero values are on the wire: `"pending_post_id":""`, `"last_reply_at":0`,
    /// `"participants":null`.
    #[test]
    fn unselected_fields_stay_at_gos_zero_values() {
        let post = post_from_row(row()).expect("decodes");
        assert_eq!(post.pending_post_id, "");
        assert_eq!(post.last_reply_at, 0);
        assert_eq!(post.participants, None);
        assert_eq!(post.is_following, None);
        assert_eq!(post.metadata, None);
        assert_eq!(post.message_source, "");
    }

    #[test]
    fn json_columns_decode() {
        let post = post_from_row(row()).expect("decodes");
        assert_eq!(
            post.file_ids.as_deref(),
            Some(["fileid".to_owned()].as_ref())
        );
        assert_eq!(post.filenames, vec!["a.txt".to_owned()]);
        assert_eq!(
            post.props.as_ref().and_then(|p| p.get("a")),
            Some(&serde_json::json!(1))
        );
        assert_eq!(post.reply_count, 7);
    }

    /// A NULL `props` column is `"props":{}` on the wire — **not** `null`, which is the answer
    /// reading `StringInterface.Scan` gives you and the answer this store used to produce.
    ///
    /// sqlx makes the map before it scans into it (`reflectx.FieldByIndexes`), so `Scan`'s
    /// early return on NULL leaves an empty map rather than a nil one. Measured against the
    /// running Go server on `GET /posts/{id}` and on both branches of
    /// `GET /channels/{id}/posts`; see the module docs for the table.
    #[test]
    fn a_null_props_column_becomes_an_empty_map() {
        let post = post_from_row(PostRow {
            props: None,
            ..row()
        })
        .expect("a NULL props column is not an error");

        assert_eq!(post.props, Some(StringInterface::new()));

        let json = serde_json::to_value(&post).expect("serialises");
        assert_eq!(json["props"], serde_json::json!({}));
    }

    /// A jsonb `null` is the case that really does reach the client as `null`: `Scan` gets the
    /// four bytes, `json.Unmarshal` sets the map back to nil, and `omitempty` is absent from the
    /// tag. Keeping the two apart is the whole point of the test above.
    #[test]
    fn a_json_null_props_column_stays_nil() {
        let post = post_from_row(PostRow {
            props: Some(serde_json::Value::Null),
            ..row()
        })
        .expect("a JSON null props column is not an error");

        assert_eq!(post.props, None);

        let json = serde_json::to_value(&post).expect("serialises");
        assert_eq!(json["props"], serde_json::Value::Null);
    }

    /// A NULL `remoteid` is `omitempty` on a nil pointer — the key disappears. A column holding
    /// the empty string is a **non-nil** pointer and reaches the wire as `""`, which is what
    /// every post the Go server writes actually looks like.
    #[test]
    fn remote_id_distinguishes_null_from_empty() {
        let absent = post_from_row(PostRow {
            remote_id: None,
            ..row()
        })
        .expect("decodes");
        let json = serde_json::to_value(&absent).expect("serialises");
        assert!(json.get("remote_id").is_none(), "nil pointer is omitted");

        let empty = post_from_row(row()).expect("decodes");
        let json = serde_json::to_value(&empty).expect("serialises");
        assert_eq!(json["remote_id"], "");
    }

    #[test]
    fn a_props_column_that_is_not_an_object_is_a_decode_error() {
        let err = post_from_row(PostRow {
            props: Some(serde_json::json!([1, 2])),
            ..row()
        })
        .expect_err("an array is not a StringInterface");
        assert!(matches!(
            err,
            StoreError::Decode {
                column: "props",
                ..
            }
        ));
    }
}
