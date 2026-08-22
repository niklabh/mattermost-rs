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
//! # NULL is not an error here, it is a zero value
//!
//! Go scans `Props`, `Filenames` and `FileIds` through `StringInterface.Scan` /
//! `StringArray.Scan` (model/utils.go:118, :185), both of which **return nil on a NULL** and
//! leave the field at its zero value. So a NULL `props` column reaches the client as
//! `"props":null`, not as an error and not as `{}`. The `Option` fields below reproduce that.

use mm_model::post::Post;
use mm_model::post_acknowledgement::PostAcknowledgement;
use mm_model::post_metadata::PostPriority;
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
        None | Some(serde_json::Value::Null) => None,
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

    /// A NULL `props` is `"props":null` on the wire, not `{}` — Go's `Scan` returns early and
    /// leaves the nil map in place. A NULL `fileids` is `"file_ids":null` for the same reason.
    #[test]
    fn null_json_columns_stay_null_rather_than_becoming_empty() {
        let post = post_from_row(PostRow {
            props: None,
            file_ids: None,
            filenames: None,
            ..row()
        })
        .expect("decodes");
        assert_eq!(post.props, None);
        assert_eq!(post.file_ids, None);
        assert!(post.filenames.is_empty());

        let json = serde_json::to_value(&post).expect("serialises");
        assert_eq!(json["props"], serde_json::Value::Null);
        assert_eq!(json["file_ids"], serde_json::Value::Null);
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
