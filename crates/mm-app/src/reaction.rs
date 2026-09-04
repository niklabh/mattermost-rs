//! Port of the read side of `server/channels/app/reaction.go`.

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
}
