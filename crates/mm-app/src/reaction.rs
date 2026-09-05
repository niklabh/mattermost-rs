//! Port of the read side of `server/channels/app/reaction.go`.

use std::collections::BTreeMap;

use mm_model::reaction::Reaction;
use mm_model::utils::{AppError, AppResult};
use mm_store::reaction_store::ReactionStore;

use crate::App;

impl App {
    /// Port of `app.App.GetReactionsForPost` (app/reaction.go:121).
    ///
    /// One store call and one error id. There is **no not-found branch**: a post that does not
    /// exist, or has never been reacted to, is a successful read of zero rows — the 500 is
    /// reserved for a genuine query failure. So `getReactions` cannot 404, which is the
    /// difference between it and every other `/posts/{post_id}/…` read.
    ///
    /// # The empty result is `null` on the wire, not `[]`
    ///
    /// Go declares `var reactions []*model.Reaction` and `SelectBuilder` appends into it, so
    /// zero rows leave the slice **nil**; `json.Marshal` renders a nil slice as `null`. The
    /// handler marshals the app layer's return value directly, with no `if len == 0` in
    /// between, so a post with no reactions answers the four bytes `null`. Preserved by
    /// returning `Vec<Reaction>` here and letting `mm_api::reactions` decide the bytes — see
    /// that module for why the decision lives there and not in this signature.
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    pub async fn get_reactions_for_post(&self, post_id: &str) -> AppResult<Vec<Reaction>> {
        self.store()
            .reaction()
            .get_for_post(post_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reaction lookup failed");
                AppError::boxed(
                    "GetReactionsForPost",
                    "app.reaction.get_for_post.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetBulkReactionsForPosts` (app/reaction.go:129).
    ///
    /// One store call, one grouping pass, and `populateEmptyReactions` (app/reaction.go:148) —
    /// which is why the answer names **every** requested post, including ids that match no post
    /// at all. The handler above it validates no id, so `["abc"]` is a legal request that
    /// answers `{"abc": []}`.
    ///
    /// # `[]` here, `null` next door
    ///
    /// [`Self::get_reactions_for_post`] answers the four bytes `null` for a post with no
    /// reactions, because Go's slice is nil and nothing fills it in. This route's empty value is
    /// `[]`, because `populateEmptyReactions` assigns a literal `[]*model.Reaction{}`. Two
    /// routes, the same absence, two different bytes — a `BTreeMap<String, Vec<Reaction>>`
    /// serialised straight through gives this one the right ones.
    ///
    /// # The map is ordered, and that is wire surface
    ///
    /// `encoding/json` sorts map keys bytewise when it marshals, so Go's response object is in
    /// ascending id order regardless of the request's. `BTreeMap<String>` orders bytewise too,
    /// so serialising it reproduces that without a sort step. (The request list is already
    /// sorted by `SortedArrayFromJSON`, so the two agree twice over.)
    ///
    /// # An empty id list is a 500
    ///
    /// The store refuses it, and that refusal is not a not-found — so it lands in the same 500
    /// as a driver failure, with the same error id. See
    /// `mm_store::reaction_store::SqlReactionStore::bulk_get_for_posts` for why Go fails there.
    #[tracing::instrument(skip(self, post_ids), fields(asked = post_ids.len()))]
    pub async fn get_bulk_reactions_for_posts(
        &self,
        post_ids: &[String],
    ) -> AppResult<BTreeMap<String, Vec<Reaction>>> {
        let all = self
            .store()
            .reaction()
            .bulk_get_for_posts(post_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "bulk reaction lookup failed");
                AppError::boxed(
                    "GetBulkReactionsForPosts",
                    "app.reaction.bulk_get_for_post_ids.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut grouped: BTreeMap<String, Vec<Reaction>> = BTreeMap::new();
        for reaction in all {
            // The id is needed twice — once as the key, once inside the value, where
            // `post_id` is a wire field this route echoes back. Taking it out of the
            // `Reaction` would blank it on the response, so the copy is the cheap half.
            grouped
                .entry(reaction.post_id.clone())
                .or_default()
                .push(reaction);
        }

        // `populateEmptyReactions` (app/reaction.go:148). A post the query matched nothing for
        // still gets a key, and one the query answered for is left alone.
        for post_id in post_ids {
            grouped.entry(post_id.clone()).or_default();
        }

        Ok(grouped)
    }
}
