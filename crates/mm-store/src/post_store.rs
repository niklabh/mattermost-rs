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
}

/// The eighteen selected columns plus the `ReplyCount` subquery, before the JSON columns are
/// decoded.
struct PostRow {
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
    reply_count: i64,
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

fn post_from_row(row: PostRow) -> Result<Post, StoreError> {
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
