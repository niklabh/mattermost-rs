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
use mm_model::post_search_results::PostSearchResults;
use mm_model::preference::PREFERENCE_CATEGORY_FLAGGED_POST;
use mm_model::search_params::{SearchParams, is_search_params_list_valid};
use mm_model::unicode::contains_cjk;
use mm_model::user::User;
use mm_model::utils::{
    StringArray, StringInterface, array_to_json, get_millis, go_to_upper, is_go_digit,
    is_go_letter, is_go_mark, new_id,
};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.PostStore`, narrowed to what `GET /posts/{post_id}` reaches.
pub trait PostStore {
    /// Port of `SqlPostPersistentNotificationStore.GetSingle`
    /// (post_persistent_notification_store.go:25), narrowed to the one question its only reachable
    /// caller asks: **does an undeleted persistent-notification row exist for this post**.
    ///
    /// It lives on `PostStore` rather than in a store of its own because nothing else in the
    /// migrated surface touches `PersistentNotifications`, and a five-column table with one live
    /// reader does not earn a file. Note the table is named `PersistentNotifications` while the
    /// Go store is named for the *post*.
    ///
    /// Returns a bool rather than the row: `ResolvePersistentNotification` discards everything but
    /// the existence, and a row this server cannot act on is not worth modelling.
    fn has_persistent_notification(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlPostStore.GetSingle` (post_store.go:918).
    fn get_single(
        &self,
        id: &str,
        incl_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Post, StoreError>> + Send;

    /// Port of `SqlPostStore.getFlaggedPosts` (post_store.go:535) and its three exported
    /// wrappers, folded into the two filters they differ by. An empty `channel_id` or `team_id`
    /// means "no filter", exactly as Go's clause builders decide.
    fn get_flagged_posts(
        &self,
        user_id: &str,
        channel_id: &str,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<PostList, StoreError>> + Send;

    /// Port of `SqlPostStore.GetPostsByIds` (post_store.go:2592).
    fn get_posts_by_ids(
        &self,
        post_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Post>, StoreError>> + Send;

    /// Port of `SqlPostStore.GetPostsByThread` (post_store.go:1686): the undeleted replies to
    /// `thread_id` created at or after `since`, in no particular order. The root itself is not a
    /// reply to anything and is not in the result.
    fn get_posts_by_thread(
        &self,
        thread_id: &str,
        since: i64,
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

    /// Port of `SqlPostStore.AnalyticsPostCount` (post_store.go:2523) for the **one** option set
    /// any migrated route asks for: `{ExcludeDeleted: true, UsersPostsOnly: true,
    /// AllowFromCache: true}`, which is what `App.GetPostsUsage` (app/usage.go:15) passes.
    ///
    /// Go builds the query from a nine-field `model.PostCountOptions`. Only three of those fields
    /// are set by anything this server answers, and the other six each add a predicate — a team
    /// join, a file-or-filenames disjunction, a hashtag test, a `system_%` exclusion and an
    /// update-at cursor pair. Porting them behind flags would mean `query_as!` could no longer
    /// check the SQL, since the predicate would have to be assembled at runtime; porting them as
    /// six more literal queries would mean six untested branches. So the reachable combination is
    /// one checked literal and the rest arrives with the route that needs it.
    ///
    /// `AllowFromCache` is not a query option at all — it is read by the cache layer above the
    /// store, which we do not have ([D-087]), so it has no effect here and none in the SQL Go
    /// runs either.
    fn analytics_posts_usage_count(
        &self,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlPostStore.AnalyticsPostCountByTeam` (post_store.go:2455), which is
    /// `countByTeam`: `COALESCE(SUM(num), 0)` over the **`posts_by_team_day` materialized
    /// view**, not a count over `Posts`. The view is refreshed by a job, so the figure lags the
    /// table on both servers by the same amount — that is what makes it comparable.
    fn analytics_post_count_by_team(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlPostStore.AnalyticsPostCountsByDay` (post_store.go:2469) after its date
    /// arithmetic: `countPostsByDay` / `countBotPostsByDay` over the `posts_by_team_day` /
    /// `bot_posts_by_team_day` views, `day` between the two bounds inclusive, newest first,
    /// **30 rows**. With a team the row is the view's own `num`; without one the days are
    /// summed across teams.
    ///
    /// The bounds are dates here where Go binds `YYYY-MM-DD` strings against the `date`
    /// column: Postgres casts the string, and binding the date directly is the same comparison
    /// with the cast done on this side of the wire.
    fn analytics_post_counts_by_day(
        &self,
        team_id: &str,
        bots_only: bool,
        start_day: chrono::NaiveDate,
        end_day: chrono::NaiveDate,
    ) -> impl std::future::Future<
        Output = Result<mm_model::analytics_row::AnalyticsRows, StoreError>,
    > + Send;

    /// Port of `SqlPostStore.AnalyticsUserCountsWithPostsByDay` (post_store.go:2482): distinct
    /// authors per calendar day of `CreateAt` (the **database's** zone, since
    /// `TO_TIMESTAMP` is evaluated there), between the two millisecond bounds inclusive, newest
    /// first, 30 rows. This one reads `Posts` itself, deleted rows and all — there is no
    /// `DeleteAt` predicate and no type filter.
    fn analytics_user_counts_with_posts_by_day(
        &self,
        team_id: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> impl std::future::Future<
        Output = Result<mm_model::analytics_row::AnalyticsRows, StoreError>,
    > + Send;

    /// Port of `SqlPostStore.GetMaxPostSize` (post_store.go:2747) and the `determineMaxPostSize`
    /// (:2721) it memoises.
    ///
    /// **A deployment artifact, not a constant** — the same shape as
    /// [`crate::draft_store::DraftStore::max_draft_size`], with one difference that matters: this
    /// one takes `max(bytes/4, PostMessageMaxRunesV2)`, so a server whose `Posts.Message` column
    /// was never widened still reports 16383 rather than 1000. The floor makes the *value* stable
    /// on every deployment this project has seen; it does not make the query pointless, because a
    /// column widened past 65535 bytes raises the limit above the floor.
    ///
    /// Go swallows the query's failure with a `mlog.Warn` and leaves the byte count at zero, which
    /// the floor then rescues — so a broken `information_schema` read still yields a working
    /// limit. Reproduced: the error is logged here and `0` is used, rather than propagated.
    fn max_post_size(&self) -> impl std::future::Future<Output = Result<usize, StoreError>> + Send;

    /// Port of `SqlPostStore.Update` (post_store.go:393) — the write behind every edit,
    /// `PUT /posts/{id}`, `PUT /posts/{id}/patch` and both pin routes alike.
    ///
    /// # It is four statements and **no transaction**
    ///
    /// Go runs them straight off `GetMaster()`: the `UPDATE Posts`, the `UPDATE Channels` that
    /// moves `LastPostAt`, an `UPDATE Posts` on the thread root, and finally the `INSERT` of the
    /// old version. A failure part-way leaves the earlier statements committed, and the edit
    /// history row is the *last* thing written — so a post whose row was updated but whose
    /// history insert failed simply loses that history entry. Reproduced as written; wrapping
    /// these in a transaction would be a different behaviour under failure, and this store's job
    /// is to be the same one.
    ///
    /// # Editing a post writes a row, it does not move one
    ///
    /// `oldPost` is turned into the history entry in place: `DeleteAt` and `UpdateAt` both become
    /// the new post's `UpdateAt`, `OriginalId` becomes the id it had, and it is given a **fresh
    /// id**. That is the shape `get_edit_history_for_post` reads back (`OriginalId = <live id>`),
    /// and it is why pinning a post — which changes nothing a reader can see except `is_pinned` —
    /// still adds an entry to its edit history.
    ///
    /// # `IsValid` runs here, and its error reaches the client unwrapped
    ///
    /// After `UpdateAt` and `PreCommit`, which is the order that decides the answer: `PreCommit`
    /// sorts and de-duplicates `FileIds`, so the length `IsValid` measures is the length of the
    /// *de-duplicated* JSON. It is returned as [`StoreError::Invalid`] because `App.UpdatePost`
    /// does `errors.As(nErr, &appErr)` and hands it straight back — a message over the limit
    /// answers `model.post.is_valid.message_length.app_error`, not `app.post.update.app_error`.
    ///
    /// `ValidateProps` is deliberately absent: it only ever logs (post.go:899).
    ///
    /// # The `LastPostAt` bump is guarded and the root bump is guarded differently
    ///
    /// `LastPostAt = time WHERE Id = ? AND LastPostAt < time` — an editing client cannot drag a
    /// channel's `LastPostAt` backwards, but it *does* drag it forwards to now, so editing an old
    /// post reorders the sidebar. The root post's `UpdateAt` moves under the same guard, which is
    /// what makes a reply's edit visible to a client polling the thread by `UpdateAt`.
    ///
    /// **The `time` here is a second `GetMillis()`, not the post's `UpdateAt`.** They differ by a
    /// millisecond often enough to matter to a test that asserts equality.
    ///
    /// Go mutates the caller's `newPost` and `oldPost` in place; this takes them by reference and
    /// returns the saved post, so the two clones inside are the price of not lying about what the
    /// caller still holds. Nothing observable depends on the mutation — the only Go reader of the
    /// mutated `oldPost` is the plugin hook, which sees an old post carrying the *history row's*
    /// fresh id.
    fn update(
        &self,
        new_post: &Post,
        old_post: &Post,
    ) -> impl std::future::Future<Output = Result<Post, StoreError>> + Send;

    /// Port of `SqlPostStore.Overwrite` (post_store.go:513), which is `OverwriteMultiple` of
    /// one — the write `App.attachFilesToPost` makes when fewer files attached than the client
    /// listed, so the row's `FileIds` says what is actually attached.
    ///
    /// Not [`PostStore::update`]: no history row, no `LastPostAt` bump, no root bump, and no
    /// `PreCommit` — `FileIds` is written as handed over, so a **nil** list reaches the column
    /// as the JSON `null`, and `UpdateAt` is a fresh clock read the caller sees on the returned
    /// post. `IsValid` runs first, with the same unwrapped error as `update`.
    ///
    /// # The `Threads` statement matches no row
    ///
    /// For a reply Go runs `UPDATE Threads SET LastReplyAt = ? WHERE PostId = ?` with the
    /// **reply's** id — and `Threads.PostId` is the root's. So the statement is executed, in the
    /// same transaction, and changes nothing; a reply whose files only partly attached keeps a
    /// `LastReplyAt` equal to its `CreateAt`. Reproduced as written, because "fixing" it would
    /// move a timestamp Go leaves alone.
    fn overwrite(
        &self,
        post: &Post,
    ) -> impl std::future::Future<Output = Result<Post, StoreError>> + Send;

    /// Port of `SqlPostStore.Delete` (post_store.go:972) — the **soft** delete behind
    /// `DELETE /api/v4/posts/{post_id}`, narrowed to a **root** post.
    ///
    /// # It deletes a thread, not a post
    ///
    /// `WHERE Id = $4 OR RootId = $4`: one statement takes the post and every reply to it, so
    /// deleting a root ends the conversation. The rows stay — `DeleteAt` and `UpdateAt` are stamped
    /// and `props.deleteBy` is written with `jsonb_set`. **A NULL `props` column stays NULL**,
    /// because `jsonb_set(NULL, …)` is NULL, so a post written before that column was populated
    /// comes back with `"props":{}` and no `deleteBy` at all.
    ///
    /// # Two branches on `RootId`
    ///
    /// A root marks its thread and its replies' files (below). A reply runs
    /// `updateThreadAfterReplyDeletion` — the thread's `ReplyCount` and `LastReplyAt` recomputed
    /// from the live replies, the author dropped from `Participants` when this was their last —
    /// and moves the root's `UpdateAt` to the delete time. Both inside the one transaction.
    ///
    /// # `Threads` and `FileInfo` are marked, not removed
    ///
    /// `deleteThread` stamps `Threads.ThreadDeleteAt`, and `deleteThreadFiles` stamps
    /// `FileInfo.DeleteAt` for the files of the **replies** — joined through `Posts.RootId`, so the
    /// root's own files are not among them. Those are soft-deleted separately by the app layer,
    /// from a goroutine in Go.
    fn delete(
        &self,
        post_id: &str,
        time: i64,
        delete_by_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPostStore.PermanentDelete` (post_store.go:1023) for one id.
    ///
    /// One transaction: `permanentDeleteAssociatedData` first — the `Threads` and
    /// `ThreadMemberships` rows keyed on the post, its `Reactions`, `TemporaryPosts` and
    /// `ReadReceipts`, then every reply (`Posts.RootId = id`) — and the post itself last. A hard
    /// delete, so nothing is stamped and nothing can be restored; the app layer has already
    /// dealt with the files, which Go removes through the file backend before calling this.
    fn permanent_delete(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPostPersistentNotificationStore.Delete`
    /// (post_persistent_notification_store.go:88) for the one id its reachable caller passes.
    ///
    /// A **soft** delete, stamping `DeleteAt` from its own clock rather than the caller's delete
    /// timestamp — which is what [`PostStore::has_persistent_notification`] reads back.
    fn delete_persistent_notification(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPostPersistentNotificationStore.DeleteByChannel`
    /// (post_persistent_notification_store.go:124) for the single channel its only reachable
    /// caller passes — `App.DeleteChannel`, which archives a channel.
    ///
    /// Go's `len(channelIds) == 0` early return is not reachable from one id, so it is not here.
    ///
    /// The `DeleteAt` is this statement's own `GetMillis()`, **not** the channel's — Go takes a
    /// fresh clock read inside the store, so the retired notification is stamped later than the
    /// archive it followed.
    ///
    /// Unlike every other cleanup on the archive path, its failure is **not** swallowed: Go
    /// answers 500 `app.post_persistent_notification.delete_by_channel.app_error`. See
    /// [`mm_app::App::delete_channel`].
    fn delete_persistent_notifications_by_channel(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPostPersistentNotificationStore.DeleteByTeam`
    /// (post_persistent_notification_store.go) for the single team its only reachable caller —
    /// `App.SoftDeleteTeam` — passes.
    ///
    /// **A soft delete two joins away from its predicate.** The rows live in
    /// `PersistentNotifications`, the team id lives on `Channels`, and the two are connected only
    /// through `Posts`: `Posts.Id = PersistentNotifications.PostId AND Posts.ChannelId =
    /// Channels.Id AND Channels.TeamId = ?`. A port that reached for a `TeamId` column would find
    /// none.
    ///
    /// `DeleteAt` is this statement's own clock read, not the team's — same as the channel
    /// sibling — and like it, the failure is **not** swallowed: archiving a team answers 500
    /// `app.post_persistent_notification.delete_by_team.app_error`. It is also the **first** thing
    /// `SoftDeleteTeam` does, before the team row is touched, so that failure leaves the team
    /// alive.
    fn delete_persistent_notifications_by_team(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPostStore.Save` (post_store.go:341) and the `SaveMultiple` (:159) it delegates
    /// to, narrowed to **one root post that is not burn-on-read, prioritised or persistent**.
    ///
    /// # `LastPostAt` moves even when the message count does not
    ///
    /// The post-commit `UPDATE Channels` sets `LastPostAt`/`LastRootPostAt` with `GREATEST` and
    /// adds `count` to `TotalMsgCount`/`TotalMsgCountRoot` — and `count` is **zero** for a post
    /// whose [`Post::excludes_from_channel_message_count`] is true. A join or leave post
    /// therefore reorders every member's sidebar without making the channel unread, which is the
    /// single easiest thing to get wrong here: dropping the guard makes every join unread, and
    /// dropping the `UPDATE` entirely leaves the channel sorted where it was.
    ///
    /// `system_guest_join_channel` and `system_add_guest_to_chan` are **not** in
    /// `IsJoinLeaveMessage`, so a guest's join *does* count. That asymmetry is Go's.
    ///
    /// # It is one `GREATEST`, not a `WHERE … <` guard
    ///
    /// [`PostStore::update`] moves `LastPostAt` with `WHERE LastPostAt < $1`; this one uses
    /// `GREATEST` in the `SET`. The two agree on the value and differ on whether a row is
    /// touched, which matters only to a trigger — but they are different statements and a reader
    /// copying one into the other would be writing a third thing.
    ///
    /// # Go logs and swallows the counter update
    ///
    /// `mlog.Warn("Error updating Channel LastPostAt.")` — the post is already committed, so a
    /// failure here leaves a saved post in a channel whose counters did not move. Reproduced: the
    /// error is logged and `Ok` is returned.
    ///
    /// # A reply is one transaction: the row, then `updateThreadsFromPosts`
    ///
    /// Go inserts the post and updates the `Threads` row for its root inside one transaction
    /// (post_store.go:206). A root with no `Threads` row gets one built from the table —
    /// participants in first-reply order, the live reply count, the last reply time and the
    /// channel's team — and an existing row is advanced: `ReplyCount + 1`, the author moved to the
    /// **end** of `Participants`, `LastReplyAt` raised. After the commit the root's `UpdateAt` is
    /// set to the reply's `CreateAt` and the returned reply carries the thread's live reply count
    /// (`populateReplyCount`). The channel counters move differently for a reply: `TotalMsgCount`
    /// and `LastPostAt`, but not the two `Root` columns — `channelNewRootPosts` is only ever
    /// filled by a root.
    ///
    /// # The priority row is written with the post
    ///
    /// `savePostsPriority` runs after the `Threads` update in the same transaction: one
    /// `PostsPriority` row holding the three fields as the client sent them. `Priority` is NOT
    /// NULL while the two booleans are nullable, so a priority with no level fails the whole
    /// save and a `nil` boolean reads back as `null`, not `false`.
    ///
    /// # What is refused rather than half-done
    ///
    /// A persistent notification and `burn_on_read` are each a [`StoreError::Argument`]:
    /// `savePostsPersistentNotifications` writes a row the notification job consumes, and the
    /// burn-on-read post is a `TemporaryPost` write. Neither is reachable from a system post; a
    /// caller that grew one would otherwise get a post with no notification row, silently.
    fn save(
        &self,
        post: &Post,
    ) -> impl std::future::Future<Output = Result<Post, StoreError>> + Send;

    /// Port of `SqlPostStore.SetPostReminder` (post_store.go:3278) — the write behind
    /// `POST /api/v4/users/{user_id}/posts/{post_id}/reminder`.
    ///
    /// # The existence check is the reason this is a transaction
    ///
    /// `PostReminders` has **no foreign key** to `Posts` (see the table: two `varchar(26)`
    /// columns and a bigint, a composite primary key, and one index on `TargetTime`), so the
    /// `SELECT EXISTS` is the only thing stopping a reminder being filed against a post id that
    /// was never written. Go runs it inside the same transaction as the insert, which is what
    /// makes the pair atomic against a concurrent delete.
    ///
    /// # The insert is an upsert, and the conflict target is the composite key
    ///
    /// `ON CONFLICT (postid, userid) DO UPDATE SET TargetTime = ?` — setting a second reminder
    /// on the same post **moves** the first rather than adding to it, so a user has at most one
    /// reminder per post. A plain `INSERT` here would 500 the second request.
    ///
    /// # `TargetTime` is Unix **seconds**, not milliseconds
    ///
    /// The one place in the migrated surface where a timestamp on the wire is not epoch millis.
    /// `SetPostReminder` formats it with `time.Unix(targetTime, 0)` (app/post.go:2866) and
    /// `CheckPostReminders` compares it against `time.Now().UTC().Unix()`, so both ends agree on
    /// seconds. Nothing here validates it: a negative or absurd value is stored as given.
    ///
    /// Returns [`StoreError::NotFound`] for a post that does not exist — Go's
    /// `store.NewErrNotFound("Post", …)`, which the app layer turns into a 500 and **not** a 404;
    /// see [`mm_app::App::set_post_reminder`].
    fn set_post_reminder(
        &self,
        post_id: &str,
        user_id: &str,
        target_time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPostStore.GetPostReminderMetadata` (post_store.go:3341).
    ///
    /// # `Username` is the **author's**, not the reminded user's
    ///
    /// `JOIN Users u ON p.UserId=u.Id` walks the post's author. The ephemeral confirmation says
    /// "You will be reminded about <link> by @<username>", which reads as the person who is
    /// reminding you and is in fact the person who wrote the post. Getting this wrong produces a
    /// sentence that is grammatical, plausible, and names the wrong human.
    ///
    /// # The team join is a `LEFT JOIN` and its `COALESCE` is load-bearing
    ///
    /// A DM or group channel has `Channels.TeamId = ''`, which matches no team, so `t.name` is
    /// SQL `NULL` and `COALESCE(t.name, '')` makes it the empty string. The caller branches on
    /// exactly that emptiness to choose between a `{siteURL}/pl/{postID}` permalink and a
    /// `{siteURL}/{teamName}/pl/{postID}` one — so dropping the `COALESCE` would not merely
    /// change a string, it would send a NULL into the branch that builds the link.
    fn get_post_reminder_metadata(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<PostReminderMetadata, StoreError>> + Send;

    /// Port of `SqlPostStore.SearchPostsForUser` (post_store.go:2909) — the database branch
    /// only. The query and its reshaping are on [`search`].
    ///
    /// `params_list` is taken by value because Go rewrites `params.Terms` in place before the
    /// searches run and the caller never reads it again.
    fn search_posts_for_user(
        &self,
        params_list: Vec<SearchParams>,
        user_id: &str,
        team_id: &str,
        page: i64,
    ) -> impl std::future::Future<Output = Result<PostSearchResults, StoreError>> + Send;
}

/// Port of `store.PostReminderMetadata` (channels/store/store.go:1389).
///
/// Not a wire type — it has no `json:` tags and never leaves the server. `UserLocale` is selected
/// by the Go query and read by nothing on this path (the ephemeral message is deliberately
/// untranslated, app/post.go:2882), and is carried here so that the row and the query stay the
/// same shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostReminderMetadata {
    pub channel_id: String,
    pub team_name: String,
    pub user_locale: String,
    pub username: String,
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
/// A NULL column and the JSON `null` both scan to a nil slice: `StringArray.Scan` returns early
/// on a nil value and otherwise `json.Unmarshal`s, which sets the slice to nil on `null`. The
/// second shape is what `overwrite` writes for a post none of whose files attached
/// (`ArrayToJSON(nil)` is `"null"`), so it is a column this store itself produces.
fn decode_string_array(
    column: &'static str,
    raw: Option<String>,
) -> Result<Option<StringArray>, StoreError> {
    raw.map(|raw| serde_json::from_str::<Option<StringArray>>(&raw))
        .transpose()
        .map(Option::flatten)
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
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn has_persistent_notification(&self, post_id: &str) -> Result<bool, StoreError> {
        // `DeleteAt = 0` bare, not coalesced — the column is NOT NULL here, unlike `Reactions`.
        let row = sqlx::query_scalar!(
            r#"SELECT 1 AS "one!" FROM persistentnotifications WHERE deleteat = 0 AND postid = $1"#,
            post_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get the persistent notification post={post_id}"),
            source,
        })?;

        Ok(row.is_some())
    }

    /// `COUNT(*) FROM Posts p WHERE p.Type = '' AND p.UserId NOT IN (SELECT UserId FROM Bots)
    /// AND p.DeleteAt = 0`.
    ///
    /// # `Type = ''` is half of `UsersPostsOnly`, and it is not the same as "not a system post"
    ///
    /// The option sets **two** predicates (post_store.go:2534-2539): an exact empty `Type` and
    /// the bot exclusion. `ExcludeSystemPosts` — a *different* option, unset here — is
    /// `Type NOT LIKE 'system_%'`. They differ on any post whose type is set but does not start
    /// with `system_`, which the integrations do write, so collapsing one into the other changes
    /// the count on a real server.
    ///
    /// # The bot exclusion is a subquery, not a join
    ///
    /// `NOT IN (SELECT UserId FROM Bots)` — and `Bots.UserId` is `NOT NULL` in the schema, which
    /// is what makes `NOT IN` safe here: a single NULL in that subquery would make the predicate
    /// answer `UNKNOWN` for *every* row and return zero posts.
    #[tracing::instrument(skip_all, fields(count))]
    async fn analytics_posts_usage_count(&self) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "value!"
              FROM posts p
             WHERE p.type = ''
               AND p.userid NOT IN (SELECT userid FROM bots)
               AND p.deleteat = 0
            "#
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count Posts".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip_all, fields(team_id, count))]
    async fn analytics_post_count_by_team(&self, team_id: &str) -> Result<i64, StoreError> {
        // `SUM(num)` is `numeric`; Go scans it into an `int64` and the cast is that scan.
        let count = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(SUM(num), 0)::bigint AS "total!"
              FROM posts_by_team_day
             WHERE ($1 = '' OR teamid = $1)
            "#,
            team_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count Posts by team: teamID: {team_id}"),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip_all, fields(team_id, bots_only, rows))]
    async fn analytics_post_counts_by_day(
        &self,
        team_id: &str,
        bots_only: bool,
        start_day: chrono::NaiveDate,
        end_day: chrono::NaiveDate,
    ) -> Result<mm_model::analytics_row::AnalyticsRows, StoreError> {
        use mm_model::analytics_row::AnalyticsRow;

        // Four statements rather than one with a `CASE` on the view name — a view is not a
        // parameter. `Value` is Go's `float64` scan of `num` / `SUM(num)`; the casts are that
        // scan.
        let rows = match (team_id.is_empty(), bots_only) {
            (false, false) => {
                sqlx::query_as!(
                    AnalyticsRow,
                    r#"
                    SELECT TO_CHAR(day, 'YYYY-MM-DD') AS "name!", num::float8 AS "value!"
                      FROM posts_by_team_day
                     WHERE teamid = $1 AND day >= $2 AND day <= $3
                     ORDER BY 1 DESC
                     LIMIT 30
                    "#,
                    team_id,
                    start_day,
                    end_day,
                )
                .fetch_all(&self.pool)
                .await
            }
            (true, false) => {
                sqlx::query_as!(
                    AnalyticsRow,
                    r#"
                    SELECT TO_CHAR(day, 'YYYY-MM-DD') AS "name!",
                           COALESCE(SUM(num), 0)::float8 AS "value!"
                      FROM posts_by_team_day
                     WHERE day >= $1 AND day <= $2
                     GROUP BY 1
                     ORDER BY 1 DESC
                     LIMIT 30
                    "#,
                    start_day,
                    end_day,
                )
                .fetch_all(&self.pool)
                .await
            }
            (false, true) => {
                sqlx::query_as!(
                    AnalyticsRow,
                    r#"
                    SELECT TO_CHAR(day, 'YYYY-MM-DD') AS "name!", num::float8 AS "value!"
                      FROM bot_posts_by_team_day
                     WHERE teamid = $1 AND day >= $2 AND day <= $3
                     ORDER BY 1 DESC
                     LIMIT 30
                    "#,
                    team_id,
                    start_day,
                    end_day,
                )
                .fetch_all(&self.pool)
                .await
            }
            (true, true) => {
                sqlx::query_as!(
                    AnalyticsRow,
                    r#"
                    SELECT TO_CHAR(day, 'YYYY-MM-DD') AS "name!",
                           COALESCE(SUM(num), 0)::float8 AS "value!"
                      FROM bot_posts_by_team_day
                     WHERE day >= $1 AND day <= $2
                     GROUP BY 1
                     ORDER BY 1 DESC
                     LIMIT 30
                    "#,
                    start_day,
                    end_day,
                )
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|source| StoreError::Db {
            context: if bots_only {
                format!("failed to find bot posts with teamId={team_id}")
            } else {
                format!("failed to find posts with teamId={team_id}")
            },
            source,
        })?;

        tracing::Span::current().record("rows", rows.len());
        Ok(mm_model::analytics_row::AnalyticsRows(rows))
    }

    #[tracing::instrument(skip_all, fields(team_id, rows))]
    async fn analytics_user_counts_with_posts_by_day(
        &self,
        team_id: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<mm_model::analytics_row::AnalyticsRows, StoreError> {
        use mm_model::analytics_row::AnalyticsRow;

        // Go writes the team filter into an `INNER JOIN Channels … AND Channels.TeamId = ?` and
        // no join at all without a team. A `LEFT JOIN` with the team in the `WHERE` is the same
        // set both ways: with a team, a post whose channel is missing fails the comparison; with
        // none, the join contributes nothing.
        let rows = sqlx::query_as!(
            AnalyticsRow,
            r#"
            SELECT TO_CHAR(DATE(TO_TIMESTAMP(p.createat / 1000)), 'YYYY-MM-DD') AS "name!",
                   COUNT(DISTINCT p.userid)::float8 AS "value!"
              FROM posts p
              LEFT JOIN channels c ON p.channelid = c.id
             WHERE ($1 = '' OR c.teamid = $1)
               AND p.createat >= $2 AND p.createat <= $3
             GROUP BY DATE(TO_TIMESTAMP(p.createat / 1000))
             ORDER BY 1 DESC
             LIMIT 30
            "#,
            team_id,
            start_ms,
            end_ms,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Posts with teamId={team_id}"),
            source,
        })?;

        tracing::Span::current().record("rows", rows.len());
        Ok(mm_model::analytics_row::AnalyticsRows(rows))
    }

    /// Port of `SqlPostStore.getFlaggedPosts` (post_store.go:535).
    ///
    /// Go builds this one by string substitution rather than with squirrel, and two of its
    /// habits are on the wire.
    ///
    /// # The team filter is missing its parentheses, and that is not a typo to tidy
    ///
    /// `buildFlaggedPostTeamFilterClause` returns the literal `AND B.TeamId = ? OR B.TeamId = ''`
    /// (post_store.go:609), appended to a `WHERE ChannelId IN (…)`. `AND` binds tighter than
    /// `OR`, so the predicate Go actually runs is
    ///
    /// ```text
    /// (ChannelId IN (members…) AND B.TeamId = ?) OR B.TeamId = ''
    /// ```
    ///
    /// — the second disjunct has **no membership check at all**. Every channel with an empty
    /// `TeamId` is a DM or a GM, so a flagged post in any DM passes the team filter for *every*
    /// team id, and it does so whether or not the caller is still a member of that DM. Measured
    /// against the running server: a flagged DM post comes back under a team it has nothing to
    /// do with. The flagged-by-this-user subquery still applies, which is what keeps it from
    /// being a disclosure bug rather than a filtering one.
    ///
    /// Written here as `(members AND ($4 = '' OR teamid = $4)) OR ($4 <> '' AND teamid = '')`,
    /// which is the same truth table in one statement: with no team id the second disjunct is
    /// dead and the first is the bare membership check, which is Go's no-clause shape.
    ///
    /// # `LIMIT ? OFFSET ?` is fed `perPage` and **`page`**
    ///
    /// The handler passes `c.Params.Page` where the store names `offset` (api4/post.go:493) and
    /// never multiplies by the page size. So `?page=1&per_page=1` skips **one post**, not one
    /// page, and `?page=2` on a three-post list returns the third. That is Go's arithmetic and
    /// it is reproduced; the parameter is named `offset` here for the same reason Go names it
    /// that.
    ///
    /// # Everything else
    ///
    /// `Posts.DeleteAt = 0` inside the subquery — unlike [`Self::get_posts_by_ids`], this one
    /// does filter. The `ReplyCount` correlated subquery is the same as that route's, resolving
    /// each post's thread root first. `ORDER BY CreateAt` is unqualified in Go and resolves to
    /// the select list's own output column, which is `A`'s, not the joined channel's.
    #[tracing::instrument(
        skip(self),
        fields(user_id = %user_id, channel_id = %channel_id, team_id = %team_id, found)
    )]
    async fn get_flagged_posts(
        &self,
        user_id: &str,
        channel_id: &str,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<PostList, StoreError> {
        let rows = sqlx::query_as!(
            PostRow,
            r#"
            SELECT a.id,
                   a.createat     AS "create_at!",
                   a.updateat     AS "update_at!",
                   a.editat       AS "edit_at!",
                   a.deleteat     AS "delete_at!",
                   a.ispinned     AS "is_pinned!",
                   a.userid       AS "user_id!",
                   a.channelid    AS "channel_id!",
                   a.rootid       AS "root_id!",
                   a.originalid   AS "original_id!",
                   a.message      AS "message!",
                   a.type         AS "post_type!",
                   a.props        AS "props?",
                   a.hashtags     AS "hashtags!",
                   a.filenames    AS "filenames?",
                   a.fileids      AS "file_ids?",
                   a.hasreactions AS "has_reactions!",
                   a.remoteid     AS "remote_id?",
                   (SELECT count(*)
                      FROM posts r
                     WHERE r.rootid = (CASE WHEN a.rootid = '' THEN a.id ELSE a.rootid END)
                       AND r.deleteat = 0) AS "reply_count!"
              FROM (SELECT posts.id,
                           posts.createat,
                           posts.updateat,
                           posts.editat,
                           posts.deleteat,
                           posts.ispinned,
                           posts.userid,
                           posts.channelid,
                           posts.rootid,
                           posts.originalid,
                           posts.message,
                           posts.type,
                           posts.props,
                           posts.hashtags,
                           posts.filenames,
                           posts.fileids,
                           posts.hasreactions,
                           posts.remoteid
                      FROM posts
                     WHERE posts.id IN (SELECT preferences.name
                                          FROM preferences
                                         WHERE preferences.userid = $1
                                           AND preferences.category = $2)
                       AND ($3 = '' OR posts.channelid = $3)
                       AND posts.deleteat = 0) AS a
              INNER JOIN channels b ON b.id = a.channelid
             WHERE (a.channelid IN (SELECT channelmembers.channelid
                                      FROM channelmembers
                                     WHERE channelmembers.userid = $1)
                    AND ($4 = '' OR b.teamid = $4))
                OR ($4 <> '' AND b.teamid = '')
             ORDER BY a.createat DESC
             LIMIT $5 OFFSET $6
            "#,
            user_id,
            PREFERENCE_CATEGORY_FLAGGED_POST,
            channel_id,
            team_id,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Posts".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

        // `pl.AddPost(post)` then `pl.AddOrder(post.Id)` per row, in query order — so `order`
        // is `CreateAt DESC` and the map is keyed by id. There is no `ErrNotFound` branch: an
        // empty result is an empty list, not a miss.
        let mut list = PostList::new();
        for row in rows {
            let post = post_from_row(row)?;
            let id = post.id.clone();
            list.add_post(post);
            list.add_order(id);
        }

        Ok(list)
    }

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

    /// # `>=`, where the unread-reply count beside it is `>`
    ///
    /// `sq.GtOrEq{"CreateAt": since}` — the one caller, `countThreadMentions`, then filters
    /// `p.CreateAt >= timestamp` a second time in Go, so the bound is stated twice and both
    /// are inclusive. `GetThreadUnreadReplyCount` is the strict one. A port that unified them
    /// would count a reply at exactly `LastViewed` as a mention but not as unread.
    ///
    /// No `ORDER BY`: Go has none, and the caller only counts.
    #[tracing::instrument(skip(self), fields(thread_id = %thread_id, since, found))]
    async fn get_posts_by_thread(
        &self,
        thread_id: &str,
        since: i64,
    ) -> Result<Vec<Post>, StoreError> {
        let rows = sqlx::query_as!(
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
             WHERE posts.rootid = $1
               AND posts.deleteat = 0
               AND posts.createat >= $2
            "#,
            thread_id,
            since,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to fetch thread posts".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
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

    #[tracing::instrument(skip(self), fields(max_post_size))]
    async fn max_post_size(&self) -> Result<usize, StoreError> {
        // `Get` into an `int32`: a failure is warned about and leaves the value at zero, which the
        // floor below then rescues. No row is `sql.ErrNoRows`, which takes the same branch.
        let bytes: i32 = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(character_maximum_length, 0) AS "length!"
              FROM information_schema.columns
             WHERE table_name = 'posts'
               AND column_name = 'message'
            "#
        )
        .fetch_optional(&self.pool)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "Unable to determine the maximum supported post size");
            None
        })
        .unwrap_or(0);

        // `max(int(maxPostSizeBytes)/4, model.PostMessageMaxRunesV2)` — Go's integer division,
        // truncating. A negative `character_maximum_length` is not representable in Postgres, so
        // the cast cannot lose a sign here.
        let max_post_size =
            (bytes.max(0) as usize / 4).max(mm_model::post::POST_MESSAGE_MAX_RUNES_V2);
        tracing::Span::current().record("max_post_size", max_post_size);
        Ok(max_post_size)
    }

    #[tracing::instrument(skip(self, new_post, old_post), fields(post_id = %new_post.id))]
    async fn update(&self, new_post: &Post, old_post: &Post) -> Result<Post, StoreError> {
        // Owned copies because Go mutates its arguments and the mutation is part of the write:
        // `new_post` gains an `UpdateAt` and a `PreCommit`, and `old_post` becomes the history row.
        let mut new_post = new_post.clone();
        let mut old_post = old_post.clone();

        new_post.update_at = get_millis();
        new_post.pre_commit();

        old_post.delete_at = new_post.update_at;
        old_post.update_at = new_post.update_at;
        old_post.original_id = old_post.id.clone();
        old_post.id = new_id();
        old_post.pre_commit();

        let max_post_size = self.max_post_size().await?;
        new_post
            .is_valid(max_post_size)
            .map_err(|app_error| StoreError::Invalid {
                entity: "Post",
                app_error,
            })?;
        // `ValidateProps` would run here. It only logs — see the trait docs.

        let props = props_for_column(&new_post);
        sqlx::query!(
            r#"
            UPDATE posts
               SET createat     = $1,
                   updateat     = $2,
                   editat       = $3,
                   deleteat     = $4,
                   ispinned     = $5,
                   userid       = $6,
                   channelid    = $7,
                   rootid       = $8,
                   originalid   = $9,
                   message      = $10,
                   type         = $11,
                   props        = $12,
                   hashtags     = $13,
                   filenames    = $14,
                   fileids      = $15,
                   hasreactions = $16,
                   remoteid     = $17
             WHERE id = $18
            "#,
            new_post.create_at,
            new_post.update_at,
            new_post.edit_at,
            new_post.delete_at,
            new_post.is_pinned,
            new_post.user_id,
            new_post.channel_id,
            new_post.root_id,
            new_post.original_id,
            new_post.message,
            new_post.post_type,
            props,
            new_post.hashtags,
            array_to_json(Some(&new_post.filenames)),
            array_to_json(new_post.file_ids.as_deref()),
            new_post.has_reactions,
            new_post.remote_id,
            new_post.id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Post with id={}", new_post.id),
            source,
        })?;

        // A **second** clock read, not `new_post.update_at`.
        let time = get_millis();
        sqlx::query!(
            "UPDATE channels SET lastpostat = $1 WHERE id = $2 AND lastpostat < $1",
            time,
            new_post.channel_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to update lastpostat of channels".to_owned(),
            source,
        })?;

        if !new_post.root_id.is_empty() {
            sqlx::query!(
                "UPDATE posts SET updateat = $1 WHERE id = $2 AND updateat < $1",
                time,
                new_post.root_id,
            )
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to update updateAt of posts".to_owned(),
                source,
            })?;
        }

        insert_post(&self.pool, &old_post)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to insert the old post".to_owned(),
                source,
            })?;

        Ok(new_post)
    }

    #[tracing::instrument(skip(self, post), fields(post_id = %post.id))]
    async fn overwrite(&self, post: &Post) -> Result<Post, StoreError> {
        // Owned because Go writes `UpdateAt` onto the post it was handed and returns that post.
        let mut post = post.clone();
        let update_at = get_millis();
        post.update_at = update_at;

        let max_post_size = self.max_post_size().await?;
        post.is_valid(max_post_size)
            .map_err(|app_error| StoreError::Invalid {
                entity: "Post",
                app_error,
            })?;
        // `ValidateProps` would run here. It only logs — see the trait docs on `update`.

        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        let props = props_for_column(&post);
        sqlx::query!(
            r#"
            UPDATE posts
               SET createat     = $1,
                   updateat     = $2,
                   editat       = $3,
                   deleteat     = $4,
                   ispinned     = $5,
                   userid       = $6,
                   channelid    = $7,
                   rootid       = $8,
                   originalid   = $9,
                   message      = $10,
                   type         = $11,
                   props        = $12,
                   hashtags     = $13,
                   filenames    = $14,
                   fileids      = $15,
                   hasreactions = $16,
                   remoteid     = $17
             WHERE id = $18
            "#,
            post.create_at,
            post.update_at,
            post.edit_at,
            post.delete_at,
            post.is_pinned,
            post.user_id,
            post.channel_id,
            post.root_id,
            post.original_id,
            post.message,
            post.post_type,
            props,
            post.hashtags,
            array_to_json(Some(&post.filenames)),
            array_to_json(post.file_ids.as_deref()),
            post.has_reactions,
            post.remote_id,
            post.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Post with id={}", post.id),
            source,
        })?;

        if !post.root_id.is_empty() {
            // `WHERE PostId = <the reply's id>` — see the trait docs: this matches nothing.
            sqlx::query!(
                "UPDATE threads SET lastreplyat = $1 WHERE postid = $2",
                update_at,
                post.id,
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to update Threads with postid={}", post.id),
                source,
            })?;
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;

        Ok(post)
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn permanent_delete(&self, post_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        // `permanentDeleteAssociatedData`, in Go's order.
        for (statement, context) in [
            (
                "DELETE FROM threads WHERE postid = $1",
                "failed to delete Threads",
            ),
            (
                "DELETE FROM threadmemberships WHERE postid = $1",
                "failed to delete ThreadMemberships",
            ),
            (
                "DELETE FROM reactions WHERE postid = $1",
                "failed to delete Reactions",
            ),
            // Go's own wrap text for this table says "Threads"; kept, so a log line here reads
            // the same as Go's for the same failure.
            (
                "DELETE FROM temporaryposts WHERE postid = $1",
                "failed to delete Threads",
            ),
            (
                "DELETE FROM readreceipts WHERE postid = $1",
                "failed to delete ReadReceipts",
            ),
            (
                "DELETE FROM posts WHERE rootid = $1",
                "failed to delete Posts",
            ),
            ("DELETE FROM posts WHERE id = $1", "failed to delete Posts"),
        ] {
            sqlx::query(statement)
                .bind(post_id)
                .execute(&mut *tx)
                .await
                .map_err(|source| StoreError::Db {
                    context: context.to_owned(),
                    source,
                })?;
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }

    async fn delete(&self, post_id: &str, time: i64, delete_by_id: &str) -> Result<(), StoreError> {
        // Unlike `update`, this one **is** a transaction in Go.
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        // Go selects `RootId, UserId`: the first decides the branch, the second is what the
        // reply branch removes from the thread's participants.
        let row = sqlx::query!(
            r#"SELECT rootid AS "root_id!", userid AS "user_id!" FROM posts WHERE id = $1"#,
            post_id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete Post with id={post_id}"),
            source,
        })?;

        let Some(row) = row else {
            // `sql.ErrNoRows` is the **only** error Go turns into a not-found here; every other
            // failure keeps its wrapped form and becomes a 500 at the app layer.
            return Err(StoreError::NotFound {
                entity: "Post",
                criteria: post_id.to_owned(),
            });
        };

        sqlx::query!(
            r#"
            UPDATE posts
               SET deleteat = $1,
                   updateat = $1,
                   props    = jsonb_set(props, ARRAY[$2::text], $3)
             WHERE id = $4 OR rootid = $4
            "#,
            time,
            mm_model::post::POST_PROPS_DELETE_BY,
            serde_json::Value::String(delete_by_id.to_owned()),
            post_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to update Posts".to_owned(),
            source,
        })?;

        if row.root_id.is_empty() {
            // `deleteThread` — the `Threads` row is marked, not removed, so the thread's reply
            // count and participants survive the delete.
            sqlx::query!(
                "UPDATE threads SET threaddeleteat = $1 WHERE postid = $2",
                time,
                post_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to mark thread for root post {post_id} as deleted"),
                source,
            })?;

            // `deleteThreadFiles` — the **replies'** files, joined through `Posts.RootId`. The
            // root's own attachments are not in this set.
            sqlx::query!(
                r#"
                UPDATE fileinfo
                   SET deleteat = $1
                  FROM posts
                 WHERE fileinfo.postid = posts.id
                   AND posts.rootid = $2
                "#,
                time,
                post_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to mark files of thread post {post_id} as deleted"),
                source,
            })?;
        } else {
            update_thread_after_reply_deletion(&mut tx, &row.root_id, &row.user_id).await?;

            // `UPDATE Posts SET UpdateAt = ? WHERE Id = root` — inside the transaction, and its
            // failure is a **warning** in Go (`Error updating Post UpdateAt.`), not a rollback.
            if let Err(source) = sqlx::query!(
                "UPDATE posts SET updateat = $1 WHERE id = $2",
                time,
                row.root_id
            )
            .execute(&mut *tx)
            .await
            {
                tracing::warn!(error = %source, "Error updating Post UpdateAt.");
            }
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn delete_persistent_notification(&self, post_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE persistentnotifications SET deleteat = $1 WHERE postid = $2",
            get_millis(),
            post_id
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete notifications for posts [{post_id}]"),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(team_id = %team_id))]
    async fn delete_persistent_notifications_by_team(
        &self,
        team_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE persistentnotifications
               SET deleteat = $1
              FROM posts, channels
             WHERE posts.id = persistentnotifications.postid
               AND posts.channelid = channels.id
               AND channels.teamid = $2
            "#,
            mm_model::utils::get_millis(),
            team_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete notifications for teams [{team_id}]"),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(channel_id = %channel_id))]
    async fn delete_persistent_notifications_by_channel(
        &self,
        channel_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE persistentnotifications
               SET deleteat = $1
              FROM posts
             WHERE posts.id = persistentnotifications.postid
               AND posts.channelid = $2
            "#,
            get_millis(),
            channel_id,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete notifications for channels [{channel_id}]"),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id, user_id = %user_id, target_time))]
    async fn set_post_reminder(
        &self,
        post_id: &str,
        user_id: &str,
        target_time: i64,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        // Go's `SELECT EXISTS (SELECT 1 FROM Posts WHERE Id=?)`. Note it does **not** exclude
        // soft-deleted posts: a reminder can be set on a post whose `DeleteAt` is non-zero, and
        // the app layer's `GetSinglePost(…, false)` is what refuses that one request earlier.
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM posts WHERE id = $1) AS "exists!""#,
            post_id
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to check for post".to_owned(),
            source,
        })?;

        if !exists {
            // Go returns here without committing; the deferred `finalizeTransactionX` rolls back.
            // Dropping `tx` does the same thing.
            return Err(StoreError::NotFound {
                entity: "Post",
                criteria: post_id.to_owned(),
            });
        }

        sqlx::query!(
            r#"
            INSERT INTO postreminders (postid, userid, targettime)
                 VALUES ($1, $2, $3)
            ON CONFLICT (postid, userid)
              DO UPDATE SET targettime = $3
            "#,
            post_id,
            user_id,
            target_time,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to insert post reminder".to_owned(),
            source,
        })?;

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn get_post_reminder_metadata(
        &self,
        post_id: &str,
    ) -> Result<PostReminderMetadata, StoreError> {
        // Go hangs `AND p.Id=?` off the final `JOIN Users` ON clause rather than a WHERE. It is
        // an *inner* join, so the two are equivalent and this spells it as the WHERE it is.
        //
        // `Get` into a struct, not `Select`: zero rows is `sql.ErrNoRows`, which Go wraps and
        // returns as an ordinary error — there is no not-found branch on this read, and the app
        // layer turns whatever comes back into the same 500.
        let row = sqlx::query!(
            r#"
            SELECT c.id            AS "channel_id!",
                   COALESCE(t.name, '') AS "team_name!",
                   u.locale        AS "user_locale!",
                   u.username      AS "username!"
              FROM posts p
              JOIN channels c ON p.channelid = c.id
              LEFT JOIN teams t ON c.teamid = t.id
              JOIN users u ON p.userid = u.id
             WHERE p.id = $1
            "#,
            post_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get post reminder metadata: postId {post_id}"),
            source,
        })?;

        Ok(PostReminderMetadata {
            channel_id: row.channel_id,
            team_name: row.team_name,
            user_locale: row.user_locale,
            username: row.username,
        })
    }

    #[tracing::instrument(skip(self, post), fields(post_id, channel_id = %post.channel_id, post_type = %post.post_type))]
    async fn save(&self, post: &Post) -> Result<Post, StoreError> {
        // Owned, because `PreSave` mutates the post Go was handed and the caller reads the id
        // back off it.
        let mut post = post.clone();

        if !post.id.is_empty() && !post.is_remote() {
            return Err(StoreError::Argument {
                entity: "Post",
                detail: "a post that already carries an id is an ErrInvalidInput on Save",
            });
        }
        if post.post_type == mm_model::post::POST_TYPE_BURN_ON_READ {
            return Err(StoreError::Argument {
                entity: "Post",
                detail: "a burn-on-read post is a TemporaryPost write",
            });
        }
        if post.get_persistent_notification() == Some(true) {
            return Err(StoreError::Argument {
                entity: "Post",
                detail: "savePostsPersistentNotifications writes PersistentNotifications",
            });
        }

        post.pre_save();

        let max_post_size = self.max_post_size().await?;
        post.is_valid(max_post_size)
            .map_err(|app_error| StoreError::Invalid {
                entity: "Post",
                app_error,
            })?;
        // `ValidateProps` would run here. It only logs — see the trait docs on `update`.

        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        insert_post(&mut *tx, &post)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to save Post".to_owned(),
                source,
            })?;

        if !post.root_id.is_empty() {
            update_threads_from_post(&mut tx, &post)
                .await
                .map_err(|source| StoreError::Db {
                    context: "update thread from posts failed".to_owned(),
                    source,
                })?;
        }

        // `savePostsPriority` (post_store.go:3115): one row per post that carries a priority,
        // written as the client sent it. `Priority` is a `*string` over a NOT NULL column, so a
        // priority document with no level — `{"requested_ack": true}` alone — fails **here**, in
        // the transaction, and the post is rolled back with it; `CreatePost` answers that with
        // its generic 500. The two booleans are nullable columns, and a `nil` stays NULL: an
        // omitted `requested_ack` reads back as `null`, a sent `false` as `false`.
        if let Some(priority) = post.get_priority() {
            sqlx::query!(
                r#"
                INSERT INTO postspriority (postid, channelid, priority, requestedack, persistentnotifications)
                VALUES ($1, $2, $3, $4, $5)
                "#,
                post.id,
                post.channel_id,
                priority.priority.as_deref(),
                priority.requested_ack,
                priority.persistent_notifications,
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to save PostPriority".to_owned(),
                source,
            })?;
        }
        // `savePostsPersistentNotifications` — refused above; see the trait docs.

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;

        // Go accumulates this per channel across the batch; with one post the count is the
        // post's own contribution and the date is its `CreateAt`. The two `Root` columns move
        // only for a root: a reply leaves `channelNewRootPosts` and `maxDateNewRootPosts` unset,
        // so it adds 0 and compares `GREATEST(0, LastRootPostAt)`.
        let count = i64::from(!post.excludes_from_channel_message_count());
        let is_root = post.root_id.is_empty();
        let root_count = if is_root { count } else { 0 };
        let root_date = if is_root { post.create_at } else { 0 };
        if let Err(source) = sqlx::query!(
            r#"
            UPDATE channels
               SET lastpostat        = GREATEST($1, lastpostat),
                   lastrootpostat    = GREATEST($2, lastrootpostat),
                   totalmsgcount     = totalmsgcount + $3,
                   totalmsgcountroot = totalmsgcountroot + $4
             WHERE id = $5
            "#,
            post.create_at,
            root_date,
            count,
            root_count,
            post.channel_id,
        )
        .execute(&self.pool)
        .await
        {
            tracing::warn!(error = %source, "Error updating Channel LastPostAt.");
        }

        if !is_root {
            // `UPDATE Posts SET UpdateAt = ? WHERE Id = ?` with the batch's latest reply time —
            // the root's `UpdateAt` is what a client's "has this thread changed" check reads.
            // Logged and swallowed, as the channel update above.
            if let Err(source) = sqlx::query!(
                "UPDATE posts SET updateat = $1 WHERE id = $2",
                post.create_at,
                post.root_id,
            )
            .execute(&self.pool)
            .await
            {
                tracing::warn!(error = %source, "Error updating Post UpdateAt.");
            }

            // `populateReplyCount`: the returned reply carries the thread's live reply count,
            // itself included, read back from the table rather than derived.
            match sqlx::query_scalar!(
                r#"SELECT COUNT(id) AS "count!" FROM posts WHERE rootid = $1 AND posts.deleteat = 0"#,
                post.root_id,
            )
            .fetch_one(&self.pool)
            .await
            {
                Ok(count) => post.reply_count = count,
                Err(source) => {
                    tracing::warn!(error = %source, "Unable to populate the reply count in some posts.");
                }
            }
        }

        tracing::Span::current().record("post_id", post.id.as_str());
        Ok(post)
    }

    /// # Paging is a lie, and `perPage` is not read
    ///
    /// "Since we don't support paging for DB search, we just return nothing for later pages":
    /// `page > 0` is an empty result before the params are even validated, and `perPage` is not
    /// a parameter of this method at all — the window is the `LIMIT 100` per element of
    /// `params_list`. A client asking for `per_page: 5` gets up to 100 posts, which is Go's
    /// answer too.
    ///
    /// # Go runs the elements concurrently; this runs them in order
    ///
    /// Each `SearchParams` is one query, fanned out over goroutines and merged through a
    /// channel. The merge is `PostList.Extend` then `SortByCreateAt`: the post map is
    /// order-insensitive, and `Order` after an unstable `sort.Slice` promises nothing for ids
    /// tied on `CreateAt`. Sequential is one of the orders the goroutines can produce.
    ///
    /// # The validation error is Go's `*model.AppError`, passed through
    ///
    /// `IsSearchParamsListValid` returns an `AppError` and the app layer's `errors.As` hands it to
    /// the client unchanged — a 500 carrying
    /// `model.search_params_list.is_valid.include_deleted_channels.app_error`. It is carried as
    /// [`StoreError::Invalid`] for the same reason `Save` carries its `IsValid` failures. It is
    /// unreachable from the API, which sets the flag identically on every element.
    #[tracing::instrument(
        skip(self, params_list),
        fields(user_id = %user_id, team_id = %team_id, page, elements = params_list.len())
    )]
    async fn search_posts_for_user(
        &self,
        mut params_list: Vec<SearchParams>,
        user_id: &str,
        team_id: &str,
        page: i64,
    ) -> Result<PostSearchResults, StoreError> {
        if page > 0 {
            return Ok(PostSearchResults::new(Some(PostList::new()), None));
        }

        is_search_params_list_valid(&params_list).map_err(|app_error| StoreError::Invalid {
            entity: "SearchParams",
            app_error,
        })?;

        let mut posts = PostList::new();
        for params in &mut params_list {
            // "remove any unquoted term that contains only non-alphanumeric chars" — applied to
            // `Terms` only; `ExcludedTerms` is not filtered here or anywhere.
            params.terms = remove_non_alpha_numeric_unquoted_terms(&params.terms, " ");
            let found = search(&self.pool, team_id, user_id, params).await?;
            posts.extend(&found);
        }
        posts.sort_by_create_at();

        Ok(PostSearchResults::new(Some(posts), None))
    }
}

/// Port of `updateThreadAfterReplyDeletion` (post_store.go:3063), inside the delete's
/// transaction.
///
/// # The participant goes only when this was their last live reply
///
/// `COUNT` the author's undeleted replies to the root **after** the delete marked this one; at
/// zero, `Participants - userId` removes them from the jsonb array. `ReplyCount` and
/// `LastReplyAt` are recomputed from the live replies either way — and only on a row whose
/// `ReplyCount > 0`, so a thread row that already reads zero is left alone rather than driven
/// negative or rewritten.
async fn update_thread_after_reply_deletion(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    root_id: &str,
    user_id: &str,
) -> Result<(), StoreError> {
    let count: i64 = sqlx::query_scalar!(
        r#"
        SELECT COUNT(posts.id) AS "count!"
          FROM posts
         WHERE posts.rootid = $1
           AND posts.userid = $2
           AND posts.deleteat = 0
        "#,
        root_id,
        user_id,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to count user's posts in thread".to_owned(),
        source,
    })?;

    // One statement in Go with an optional `Set`; two literal statements here, since sqlx wants
    // each one whole. The predicate and the two subqueries are identical between them.
    let result = if count == 0 {
        sqlx::query!(
            r#"
            UPDATE threads
               SET participants = participants - $2::text,
                   lastreplyat = (SELECT COALESCE(MAX(createat), 0) FROM posts WHERE rootid = $1 AND deleteat = 0),
                   replycount = (SELECT COUNT(*) FROM posts WHERE rootid = $1 AND deleteat = 0)
             WHERE postid = $1
               AND replycount > 0
            "#,
            root_id,
            user_id,
        )
        .execute(&mut **tx)
        .await
    } else {
        sqlx::query!(
            r#"
            UPDATE threads
               SET lastreplyat = (SELECT COALESCE(MAX(createat), 0) FROM posts WHERE rootid = $1 AND deleteat = 0),
                   replycount = (SELECT COUNT(*) FROM posts WHERE rootid = $1 AND deleteat = 0)
             WHERE postid = $1
               AND replycount > 0
            "#,
            root_id,
        )
        .execute(&mut **tx)
        .await
    };
    result.map(|_| ()).map_err(|source| StoreError::Db {
        context: "failed to update Threads".to_owned(),
        source,
    })
}

/// Port of `updateThreadsFromPosts` (post_store.go:270) for the one post `Save` inserts, inside
/// the caller's transaction.
///
/// # Insert or advance, decided by the `Threads` row
///
/// No row: build one from the table — the participants are every live replier in the order of
/// their **latest** reply (`GROUP BY UserId ORDER BY MAX(CreateAt)`), the reply count and last
/// reply time are counted from `Posts`, and `ThreadTeamId` is the channel's team (empty for a
/// DM). The reply just inserted is in the same transaction and is counted. A row: `ReplyCount`
/// goes up by one, the author is removed from `Participants` if present and appended at the end,
/// and `LastReplyAt` is raised only if this reply is later. Both branches write `ChannelId`.
///
/// # `ThreadDeleteAt` is not written
///
/// The insert names six columns and leaves it NULL, which every read in `thread_store.rs`
/// coalesces to zero. Writing `0` here would be a different row from the one Go writes.
async fn update_threads_from_post(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    post: &Post,
) -> Result<(), sqlx::Error> {
    let existing = sqlx::query!(
        r#"
        SELECT postid                             AS "post_id!",
               COALESCE(replycount, 0)            AS "reply_count!",
               COALESCE(lastreplyat, 0)           AS "last_reply_at!",
               participants                       AS "participants?"
          FROM threads
         WHERE postid = $1
        "#,
        post.root_id,
    )
    .fetch_optional(&mut **tx)
    .await?;

    match existing {
        None => {
            let participants: Vec<String> = sqlx::query_scalar!(
                r#"
                SELECT posts.userid AS "user_id!"
                  FROM posts
                 WHERE posts.rootid = $1
                   AND posts.deleteat = 0
                 GROUP BY posts.userid
                 ORDER BY MAX(posts.createat) ASC
                "#,
                post.root_id,
            )
            .fetch_all(&mut **tx)
            .await?;

            let count: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(posts.id) AS "count!" FROM posts WHERE posts.rootid = $1 AND posts.deleteat = 0"#,
                post.root_id,
            )
            .fetch_one(&mut **tx)
            .await?;

            let last_reply_at: i64 = sqlx::query_scalar!(
                r#"SELECT COALESCE(MAX(posts.createat), 0) AS "last_reply_at!" FROM posts WHERE posts.rootid = $1 AND posts.deleteat = 0"#,
                post.root_id,
            )
            .fetch_one(&mut **tx)
            .await?;

            let team_id: String = sqlx::query_scalar!(
                r#"SELECT COALESCE(channels.teamid, '') AS "team_id!" FROM channels WHERE channels.id = $1"#,
                post.channel_id,
            )
            .fetch_one(&mut **tx)
            .await?;

            sqlx::query!(
                r#"
                INSERT INTO threads
                    (postid, channelid, replycount, lastreplyat, participants, threadteamid)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
                post.root_id,
                post.channel_id,
                count,
                last_reply_at,
                serde_json::Value::from(participants),
                team_id,
            )
            .execute(&mut **tx)
            .await?;
        }
        Some(row) => {
            let mut participants: Vec<String> = row
                .participants
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            participants.retain(|id| id != &post.user_id);
            participants.push(post.user_id.clone());
            let reply_count = row.reply_count + 1;
            let last_reply_at = row.last_reply_at.max(post.create_at);

            sqlx::query!(
                r#"
                UPDATE threads
                   SET channelid = $1,
                       replycount = $2,
                       lastreplyat = $3,
                       participants = $4
                 WHERE postid = $5
                "#,
                post.channel_id,
                reply_count,
                last_reply_at,
                serde_json::Value::from(participants),
                post.root_id,
            )
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// `model.StringInterfaceToJSON(post.Props)` for a `jsonb` column.
///
/// Go marshals the map to text and lets the driver hand it to Postgres, so a **nil** map is the
/// four bytes `null` and lands as jsonb `'null'` — not as SQL NULL, and not as `'{}'`. That is
/// the value `post_from_row` reads back as `props: None`, so the round trip is closed. Every post
/// that has been through `PreCommit` carries a materialised map, which is why the `null` arm is
/// hard to reach through a route; it is here because dropping it would silently rewrite the one
/// row shape that can.
fn props_for_column(post: &Post) -> serde_json::Value {
    match post.get_props() {
        Some(props) => serde_json::Value::Object(props.clone()),
        None => serde_json::Value::Null,
    }
}

/// The `INSERT INTO Posts (postSliceColumns()) VALUES (postToSlice(post))` that both
/// `SqlPostStore.Update` (for the edit-history row) and `SaveMultiple` build.
///
/// The column order is `postSliceColumnsWithTypes` (post_store.go:53) exactly. It is not the
/// table's own column order — `EditAt`, `IsPinned` and `RemoteId` were added later and sit at the
/// end of the physical table — so naming the columns is what keeps the two apart.
async fn insert_post<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    post: &Post,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO posts
            (id, createat, updateat, editat, deleteat, ispinned, userid, channelid, rootid,
             originalid, message, type, props, hashtags, filenames, fileids, hasreactions, remoteid)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        "#,
        post.id,
        post.create_at,
        post.update_at,
        post.edit_at,
        post.delete_at,
        post.is_pinned,
        post.user_id,
        post.channel_id,
        post.root_id,
        post.original_id,
        post.message,
        post.post_type,
        props_for_column(post),
        post.hashtags,
        array_to_json(Some(&post.filenames)),
        array_to_json(post.file_ids.as_deref()),
        post.has_reactions,
        post.remote_id,
    )
    .execute(executor)
    .await
    .map(|_| ())
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

/// `specialSearchChars` (sqlstore/store.go:394) — "have special meaning and can be treated as
/// spaces". Replaced in `Terms` unless the search is a hashtag one, and in `ExcludedTerms` always.
const SPECIAL_SEARCH_CHARS: [char; 7] = ['<', '>', '+', '(', ')', '~', ':'];

/// Port of `SqlPostStore.search` (post_store.go:2235) with `channelsByName` and `userByUsername`
/// both false — the only shape `SearchPostsForUser` calls it in. The by-name shape is `Search`,
/// the plugin API's entry point, and is not ported.
///
/// # One statement where Go composes many
///
/// Squirrel appends a `WHERE` per filter that is present; sqlx's compile-time checking wants one
/// fixed statement. So every optional predicate is `($n IS NULL OR <predicate>)` with the argument
/// `None` where Go would have appended nothing — the planner folds the constant arm and the row
/// set is the same. Three places reshape more than that: the `from:` sub-query tests
/// `TeamMembers` through `EXISTS` rather than Go's comma join (the same membership test, minus
/// duplicate ids in the `IN` list); `sq.Eq{"Id": ids}` is `= ANY($n)` and `sq.NotEq` is
/// `<> ALL($n)`; and the `Message`/`Hashtags` column choice is a `CASE` on a boolean rather than
/// a second statement. Everything else — the `system_` prefix with its LIKE-wildcard underscore,
/// the `card` exclusion, `TeamId = ? OR TeamId = ''` so direct and group channels count in a team
/// search, the `ReplyCount` sub-query, the order and the 100-row cap — is Go's text.
///
/// # The text-search config is a parameter
///
/// Go reads `default_text_search_config` once at start-up and pastes it into the SQL;
/// [`default_text_search_config`] reads the same setting and binds it as `regconfig`, the way
/// `channel_store` already does. Same dictionary either way.
///
/// # A query error is an empty result, not an error
///
/// "Don't return the error to the caller as it is of no use to the user" — Go logs the failure
/// and answers with whatever the list holds, which is nothing. This is reachable: a search of
/// only excluded terms (`-foo`) builds the tsquery ` &!(foo)`, which Postgres rejects, and Go's
/// answer is a 200 with an empty `order`. So the error is logged and swallowed here — against
/// the crate's usual rule, deliberately — because the alternative is a 500 where Go shows an
/// empty page. A row that will not decode takes the same path, since Go's `SelectBuilder` fails
/// as one call.
///
/// # The CJK branch is live
///
/// `FeatureFlags.CJKSearch` defaults to `true` (feature_flags.go:198), so a term containing Han,
/// Hiragana, Katakana or Hangul takes the `LIKE` path instead of `to_tsquery` on a stock server
/// as much as here. `SearchWithoutUserId` is never set by this route's caller and is not
/// modelled: every search is scoped to the caller's channel memberships.
#[tracing::instrument(skip_all, fields(team_id = %team_id, hashtag = params.is_hashtag, or_terms = params.or_terms))]
async fn search(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    params: &SearchParams,
) -> Result<PostList, StoreError> {
    let mut list = PostList::new();
    // Note what the list omits: `ExcludedDate`, `ExcludedAfterDate` and `ExcludedBeforeDate`.
    // `-on:2024-01-01` alone is an empty result, not "everything else".
    if params.terms.is_empty()
        && params.excluded_terms.is_empty()
        && params.in_channels.is_empty()
        && params.excluded_channels.is_empty()
        && params.from_users.is_empty()
        && params.excluded_users.is_empty()
        && params.on_date.is_empty()
        && params.after_date.is_empty()
        && params.before_date.is_empty()
    {
        return Ok(list);
    }

    // `buildCreateDateFilterClause` (post_store.go:2050): `on:` returns early, so none of the
    // other five bounds is applied beside it.
    let mut on_date = None;
    let mut excluded_date = None;
    let mut after_date = None;
    let mut before_date = None;
    let mut excluded_after_date = None;
    let mut excluded_before_date = None;
    if !params.on_date.is_empty() {
        on_date = Some(params.get_on_date_millis());
    } else {
        if !params.excluded_date.is_empty() {
            excluded_date = Some(params.get_excluded_date_millis());
        }
        if !params.after_date.is_empty() {
            after_date = Some(params.get_after_date_millis());
        }
        if !params.before_date.is_empty() {
            before_date = Some(params.get_before_date_millis());
        }
        if !params.excluded_after_date.is_empty() {
            excluded_after_date = Some(params.get_excluded_after_date_millis());
        }
        if !params.excluded_before_date.is_empty() {
            excluded_before_date = Some(params.get_excluded_before_date_millis());
        }
    }

    // The hashtag map is built from the terms **before** the special characters are blanked,
    // and uppercased with Go's `strings.ToUpper` on both sides of the comparison.
    let mut term_map = std::collections::HashSet::new();
    if params.is_hashtag {
        for term in params.terms.split(' ') {
            term_map.insert(go_to_upper(term));
        }
    }

    let mut terms = params.terms.as_str().to_owned();
    let mut excluded_terms = params.excluded_terms.as_str().to_owned();
    for c in SPECIAL_SEARCH_CHARS {
        if !params.is_hashtag {
            terms = terms.replace(c, " ");
        }
        excluded_terms = excluded_terms.replace(c, " ");
    }

    let mut ts_query: Option<String> = None;
    let mut like_terms: Option<Vec<String>> = None;
    let mut like_excluded: Option<Vec<String>> = None;
    if terms.is_empty() && excluded_terms.is_empty() {
        // "we've already confirmed that we have a channel or user to search for"
    } else if contains_cjk(&terms) || contains_cjk(&excluded_terms) {
        // `buildCJKSearchClause` (post_store.go:2202): one `LIKE` per term, ANDed or ORed, and
        // one `NOT LIKE` per excluded term. An empty list adds no clause, so it binds as NULL.
        let parsed: Vec<String> = split_cjk_search_terms(&terms)
            .iter()
            .map(|term| like_pattern(term))
            .collect();
        if !parsed.is_empty() {
            like_terms = Some(parsed);
        }
        let parsed: Vec<String> = split_cjk_search_terms(&excluded_terms)
            .iter()
            .map(|term| like_pattern(term))
            .collect();
        if !parsed.is_empty() {
            like_excluded = Some(parsed);
        }
    } else {
        ts_query = Some(build_ts_query(&terms, &excluded_terms, params.or_terms));
    }

    let text_config = crate::channel_store::default_text_search_config(pool).await?;

    fn non_empty(list: &StringArray) -> Option<&[String]> {
        (!list.is_empty()).then_some(list.as_slice())
    }
    let from_users = non_empty(&params.from_users);
    let excluded_users = non_empty(&params.excluded_users);
    let in_channels = non_empty(&params.in_channels);
    let excluded_channels = non_empty(&params.excluded_channels);

    let rows = sqlx::query_as!(
        PostRow,
        r#"
        SELECT q2.id           AS "id!",
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
               (SELECT count(*)
                  FROM posts
                 WHERE posts.rootid = (CASE WHEN q2.rootid = '' THEN q2.id ELSE q2.rootid END)
                   AND posts.deleteat = 0) AS "reply_count!"
          FROM posts q2
         WHERE q2.deleteat = 0
           AND q2.type NOT LIKE 'system_%'
           AND q2.type <> 'card'
           AND (($2::text[] IS NULL AND $3::text[] IS NULL)
                OR q2.userid IN (SELECT u.id
                                   FROM users u
                                  WHERE ($1::text = ''
                                         OR EXISTS (SELECT 1
                                                      FROM teammembers tm
                                                     WHERE tm.teamid = $1
                                                       AND tm.userid = u.id))
                                    AND ($2::text[] IS NULL OR u.id = ANY($2))
                                    AND ($3::text[] IS NULL OR u.id <> ALL($3))))
           AND ($4::bigint IS NULL OR q2.createat BETWEEN $4 AND $5::bigint)
           AND ($6::bigint IS NULL OR q2.createat NOT BETWEEN $6 AND $7::bigint)
           AND ($8::bigint IS NULL OR q2.createat >= $8)
           AND ($9::bigint IS NULL OR q2.createat <= $9)
           AND ($10::bigint IS NULL OR q2.createat < $10)
           AND ($11::bigint IS NULL OR q2.createat > $11)
           AND ($12::text IS NULL
                OR to_tsvector($13::text::regconfig,
                               CASE WHEN $14::bool THEN q2.hashtags ELSE q2.message END)
                   @@ to_tsquery($13::text::regconfig, $12))
           AND ($15::text[] IS NULL
                OR ($16::bool
                    AND (CASE WHEN $14 THEN q2.hashtags ELSE q2.message END) LIKE ANY($15))
                OR (NOT $16
                    AND (CASE WHEN $14 THEN q2.hashtags ELSE q2.message END) LIKE ALL($15)))
           AND ($17::text[] IS NULL
                OR NOT ((CASE WHEN $14 THEN q2.hashtags ELSE q2.message END) LIKE ANY($17)))
           AND q2.channelid IN (SELECT c.id
                                  FROM channels c, channelmembers cm
                                 WHERE c.id = cm.channelid
                                   AND ($18::bool OR c.deleteat = 0)
                                   AND cm.userid = $19
                                   AND ($1 = '' OR c.teamid = $1 OR c.teamid = '')
                                   AND ($20::text[] IS NULL OR c.id = ANY($20))
                                   AND ($21::text[] IS NULL OR c.id <> ALL($21)))
         ORDER BY q2.createat DESC
         LIMIT 100
        "#,
        team_id,
        from_users,
        excluded_users,
        on_date.map(|(start, _)| start),
        on_date.map(|(_, end)| end),
        excluded_date.map(|(start, _)| start),
        excluded_date.map(|(_, end)| end),
        after_date,
        before_date,
        excluded_after_date,
        excluded_before_date,
        ts_query,
        text_config,
        params.is_hashtag,
        like_terms.as_deref(),
        params.or_terms,
        like_excluded.as_deref(),
        params.include_deleted_channels,
        user_id,
        in_channels,
        excluded_channels,
    )
    .fetch_all(pool)
    .await;

    let decoded: Result<Vec<Post>, StoreError> = match rows {
        Ok(rows) => rows.into_iter().map(post_from_row).collect(),
        Err(source) => Err(StoreError::Db {
            context: "failed to search posts".to_owned(),
            source,
        }),
    };
    match decoded {
        Err(err) => {
            // Go: `mlog.Warn("Query error searching posts.", ...)` and an empty list.
            tracing::warn!(error = %err, "query error searching posts");
        }
        Ok(posts) => {
            for post in posts {
                // "exclude burn on read posts from search results"
                if post.post_type == mm_model::post::POST_TYPE_BURN_ON_READ {
                    continue;
                }
                if params.is_hashtag
                    && !post
                        .hashtags
                        .split(' ')
                        .any(|tag| term_map.contains(&go_to_upper(tag)))
                {
                    continue;
                }
                list.add_order(post.id.as_str());
                list.add_post(post);
            }
        }
    }
    list.make_non_nil();
    Ok(list)
}

/// The tsquery text Go builds for the non-CJK branch (post_store.go:2294-2333): hyphens
/// neutralised, `*` at a word end rewritten as `:*`, runs of whitespace joined with `&` (or `|`
/// for an OR search), spaces inside a quoted phrase joined with `<->`, and the excluded terms —
/// always `|`-joined — appended as ` &!(...)`.
///
/// Whatever this produces is bound as a parameter and parsed by Postgres, so its exact bytes are
/// the parity surface: an input Go turns into an unparseable query must become the same
/// unparseable query here, or one server answers an empty page and the other a match.
fn build_ts_query(terms: &str, excluded_terms: &str, or_terms: bool) -> String {
    let terms = mark_wildcards(&neutralize_non_word_hyphens(terms));
    let excluded_terms = mark_wildcards(&neutralize_non_word_hyphens(excluded_terms));

    let mut clause = replace_spaces(&terms, or_terms);
    let excluded_clause = replace_spaces(&excluded_terms, true);
    if !excluded_clause.is_empty() {
        clause.push_str(" &!(");
        clause.push_str(&excluded_clause);
        clause.push(')');
    }
    clause
}

/// The `replaceSpaces` closure (post_store.go:2306): collapse whitespace, join quoted phrases,
/// then turn every remaining space into the operator. `use_or` is `excludedInput || OrTerms`.
fn replace_spaces(input: &str, use_or: bool) -> String {
    if input.is_empty() {
        return String::new();
    }
    // `strings.Join(strings.Fields(input), " ")`.
    let collapsed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let joined = join_quoted_phrases(&collapsed);
    joined.replace(' ', if use_or { "|" } else { "&" })
}

/// `quotedStringsRegex.ReplaceAllStringFunc` (post_store.go:2314) — `("[^"]*")`, each match
/// with its spaces replaced by `<->`. Pairs are taken left to right, so an odd trailing quote
/// matches nothing and the text after it keeps its spaces.
fn join_quoted_phrases(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('"') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('"') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push('"');
        out.push_str(&after_open[..close].replace(' ', "<->"));
        out.push('"');
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `wildCardRegex.ReplaceAllLiteralString(terms, ":* ")` (post_store.go:2301) — the pattern is
/// `\*($| )`, so a `*` is a prefix marker only at the end of a word, and the space it consumes
/// is put back by the replacement. `a**` is `a*:* `: the first star is not followed by a
/// boundary and survives as a character.
fn mark_wildcards(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '*' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            None => out.push_str(":* "),
            Some(' ') => {
                chars.next();
                out.push_str(":* ");
            }
            Some(_) => out.push('*'),
        }
    }
    out
}

/// Port of `neutralizeNonWordHyphens` (sqlstore/utils.go:207): a `-` not flanked by a word rune
/// on both sides becomes a space, so `t-shirt` reaches `to_tsquery` intact and `-`, `--` and
/// `foo-` do not. Go mutates the rune slice as it goes, and a hyphen only ever becomes a space,
/// which is as much a non-word as the hyphen was — so reading the original runes answers the same.
fn neutralize_non_word_hyphens(s: &str) -> String {
    if !s.contains('-') {
        return s.to_owned();
    }
    let runes: Vec<char> = s.chars().collect();
    runes
        .iter()
        .enumerate()
        .map(|(i, &r)| {
            if r != '-' {
                return r;
            }
            let has_left = i > 0 && is_word_rune(runes[i - 1]);
            let has_right = i + 1 < runes.len() && is_word_rune(runes[i + 1]);
            if has_left && has_right { '-' } else { ' ' }
        })
        .collect()
}

/// Port of `isWordRune` (sqlstore/utils.go:227): `unicode.IsLetter || IsDigit || IsMark`. Not
/// `char::is_alphanumeric`, which is a different (and Rust-versioned) set on both counts.
fn is_word_rune(r: char) -> bool {
    is_go_letter(r) || is_go_digit(r) || is_go_mark(r)
}

/// Port of `removeNonAlphaNumericUnquotedTerms` (sqlstore/utils.go:105): split on the
/// separator, keep a word if it is quoted or holds at least one letter or digit, trim each
/// survivor, join again. `abcd "**" && abc` becomes `abcd "**" abc`.
fn remove_non_alpha_numeric_unquoted_terms(line: &str, separator: &str) -> String {
    line.split(separator)
        .filter(|word| is_quoted_word(word) || contains_alpha_numeric_char(word))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(separator)
}

/// Port of `isQuotedWord` (sqlstore/utils.go:131): at least two bytes, a `"` at each end.
fn is_quoted_word(s: &str) -> bool {
    s.len() >= 2 && s.starts_with('"') && s.ends_with('"')
}

/// Port of `containsAlphaNumericChar` (sqlstore/utils.go:118): `unicode.IsLetter || IsDigit`.
fn contains_alpha_numeric_char(s: &str) -> bool {
    s.chars().any(|c| is_go_letter(c) || is_go_digit(c))
}

/// Port of `splitCJKSearchTerms` (post_store.go:2176): every quoted phrase first, in order and
/// without its quotes, then the unquoted words; trailing `*` stripped from each, since
/// `LIKE '%term%'` is already open at both ends. A quoted phrase is kept if it was non-empty
/// **before** the strip — `"**"` yields an empty term, and `LIKE '%%'` matches every row — while
/// an unquoted word is dropped if it is empty **after** it. Both are Go's.
fn split_cjk_search_terms(input: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut remaining = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('"') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('"') else {
            break;
        };
        let phrase = &after_open[..close];
        if !phrase.is_empty() {
            terms.push(phrase.trim_end_matches('*').to_owned());
        }
        remaining.push_str(&rest[..open]);
        remaining.push(' ');
        rest = &after_open[close + 1..];
    }
    remaining.push_str(rest);
    for word in remaining.split_whitespace() {
        let word = word.trim_end_matches('*');
        if !word.is_empty() {
            terms.push(word.to_owned());
        }
    }
    terms
}

/// `"%" + sanitizeSearchTerm(term, "\\") + "%"` — the CJK branch's `LIKE` argument. No `ESCAPE`
/// clause in Go, so Postgres's default backslash is the escape the sanitiser writes.
fn like_pattern(term: &str) -> String {
    format!("%{}%", crate::user_store::sanitize_search_term(term, '\\'))
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

/// The term pipeline, transcribed from the Go source rather than an oracle: every function here
/// is unexported in `sqlstore` and cannot be reached from `reference/dump`. The parity suite
/// (`tests/parity/post_search.rs`) is where the composed result meets Go's own answer; these pin
/// the branches a reader could plausibly get wrong on the way there.
#[cfg(test)]
mod search_term_tests {
    use super::*;

    #[test]
    fn non_alphanumeric_unquoted_terms_are_dropped_and_quoted_ones_kept() {
        assert_eq!(
            remove_non_alpha_numeric_unquoted_terms(r#"abcd "**" && abc"#, " "),
            r#"abcd "**" abc"#
        );
        assert_eq!(remove_non_alpha_numeric_unquoted_terms("", " "), "");
        assert_eq!(remove_non_alpha_numeric_unquoted_terms("&& --", " "), "");
        // A single `"` is not a quoted word: two bytes minimum, one at each end.
        assert_eq!(remove_non_alpha_numeric_unquoted_terms(r#"" a"#, " "), "a");
        assert_eq!(
            remove_non_alpha_numeric_unquoted_terms(r#""" a"#, " "),
            r#""" a"#
        );
        // `IsDigit`, not `IsNumber`: a vulgar fraction is not alphanumeric.
        assert_eq!(remove_non_alpha_numeric_unquoted_terms("½ 3", " "), "3");
        assert_eq!(remove_non_alpha_numeric_unquoted_terms("٣ é", " "), "٣ é");
    }

    #[test]
    fn hyphens_survive_only_between_word_runes() {
        assert_eq!(neutralize_non_word_hyphens("t-shirt"), "t-shirt");
        assert_eq!(neutralize_non_word_hyphens("-shirt"), " shirt");
        assert_eq!(neutralize_non_word_hyphens("shirt-"), "shirt ");
        assert_eq!(neutralize_non_word_hyphens("a--b"), "a  b");
        assert_eq!(neutralize_non_word_hyphens("a - b"), "a   b");
        assert_eq!(neutralize_non_word_hyphens("no hyphen"), "no hyphen");
        assert_eq!(neutralize_non_word_hyphens("1-2"), "1-2");
        // A combining mark counts as part of the word on the left.
        assert_eq!(neutralize_non_word_hyphens("e\u{301}-x"), "e\u{301}-x");
        assert_eq!(neutralize_non_word_hyphens("a-\u{301}"), "a-\u{301}");
        assert_eq!(neutralize_non_word_hyphens("a-*"), "a *");
    }

    #[test]
    fn a_star_is_a_prefix_marker_only_at_a_word_end() {
        assert_eq!(mark_wildcards("foo*"), "foo:* ");
        assert_eq!(mark_wildcards("foo* bar"), "foo:* bar");
        assert_eq!(mark_wildcards("a**"), "a*:* ");
        assert_eq!(mark_wildcards("* *"), ":* :* ");
        assert_eq!(mark_wildcards("f*o"), "f*o");
        assert_eq!(mark_wildcards(""), "");
    }

    #[test]
    fn spaces_become_operators_except_inside_quotes() {
        assert_eq!(replace_spaces("", false), "");
        assert_eq!(replace_spaces("  a   b  ", false), "a&b");
        assert_eq!(replace_spaces("a b", true), "a|b");
        assert_eq!(replace_spaces(r#"a "b c" d"#, false), r#"a&"b<->c"&d"#);
        assert_eq!(replace_spaces(r#"a "b c" d"#, true), r#"a|"b<->c"|d"#);
        // An odd quote opens nothing.
        assert_eq!(replace_spaces(r#"a "b c"#, false), r#"a&"b&c"#);
        assert_eq!(
            replace_spaces(r#""a b" "c d""#, false),
            r#""a<->b"&"c<->d""#
        );
    }

    #[test]
    fn excluded_terms_are_or_joined_and_negated_as_a_group() {
        assert_eq!(build_ts_query("a b", "", false), "a&b");
        assert_eq!(build_ts_query("a b", "", true), "a|b");
        assert_eq!(build_ts_query("a b", "c d", false), "a&b &!(c|d)");
        // The excluded side is `|`-joined even in an AND search.
        assert_eq!(build_ts_query("a", "c d", false), "a &!(c|d)");
        // No terms at all: the clause Postgres will reject, which is Go's empty page.
        assert_eq!(build_ts_query("", "c", false), " &!(c)");
        assert_eq!(
            build_ts_query("t-shirt foo*", "-bar", false),
            "t-shirt&foo:* &!(bar)"
        );
        // The star before a closing quote is followed by `"`, not a boundary, so it stays.
        assert_eq!(build_ts_query(r#""a b*" c"#, "", false), r#""a<->b*"&c"#);
        assert_eq!(build_ts_query(r#""a b" c*"#, "", false), r#""a<->b"&c:*"#);
    }

    #[test]
    fn cjk_terms_split_quoted_phrases_first_then_words() {
        assert_eq!(
            split_cjk_search_terms(r#"日本 "東京 タワー" 韓国*"#),
            vec!["東京 タワー", "日本", "韓国"]
        );
        assert_eq!(split_cjk_search_terms(r#""**" x"#), vec!["", "x"]);
        assert_eq!(split_cjk_search_terms("** x"), vec!["x"]);
        assert_eq!(split_cjk_search_terms(r#"a "b"#), vec!["a", "\"b"]);
        assert!(split_cjk_search_terms("").is_empty());
    }

    #[test]
    fn like_patterns_escape_with_a_backslash() {
        assert_eq!(like_pattern("50%"), "%50\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("a\\b"), "%ab%");
    }
}
