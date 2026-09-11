//! Port of `server/channels/app/view.go` — the whole file, all seven functions plus
//! `publishViewEvent`.
//!
//! Not to be confused with [`crate::channel_view`], which is `channel_view.go`: marking a channel
//! read. Same word, different feature.
//!
//! # Every error id here is distinct, and three of them are the *only* thing a client can see
//!
//! Go raises eleven different ids across seven functions and the handler layer adds five more.
//! They carry the whole semantics: `GetView` answers `app.view.get.not_found.app_error` for a
//! miss and `app.view.get.app_error` for a broken query, and the status codes (404 against 500)
//! are the only other difference. Collapsing any pair would be invisible in a happy-path test and
//! wrong for every client that branches on `id` — which the webapp does.
//!
//! # The websocket events are not symmetric, and that asymmetry is deliberate in Go
//!
//! `view_created` and `view_updated` carry the **whole view** as a JSON *string* under `view`;
//! `view_deleted` carries only `view_id`; `view_sorted` carries the **whole reordered list** as a
//! JSON string under `views`. Go's own comment says why the delete is different ("consumers only
//! need the ID to remove the view from state"). All four are `Add(key, string)` — a JSON document
//! nested inside a JSON string, which is Mattermost's usual websocket habit and not a mistake.
//!
//! # A marshal failure is logged and swallowed, twice, in two different shapes
//!
//! `publishViewEvent` returns without publishing at all; `UpdateViewSortOrder` logs and then
//! carries on to return the views. So a failure to encode loses the event but never the response.
//! `View` and `Vec<View>` serialise infallibly here — every field is a string, an `i64` or a
//! `serde_json` value — so neither branch is reachable; both are reproduced rather than
//! `unwrap`ped.
//!
//! # The create limit is checked, not enforced by the database
//!
//! `MaxViewsPerChannel` is 50, and `CreateView` counts first and refuses at `>=`. There is no
//! unique constraint or trigger behind it, so two concurrent creates can both see 49 and both
//! succeed. That race is Go's; it is reproduced rather than fixed, because fixing it would make
//! this server refuse a create Go accepts.

use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::view::{MAX_VIEWS_PER_CHANNEL, View, ViewPatch, ViewQueryOpts};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_VIEW_CREATED, WEBSOCKET_EVENT_VIEW_DELETED, WEBSOCKET_EVENT_VIEW_SORTED,
    WEBSOCKET_EVENT_VIEW_UPDATED, WebSocketEvent,
};
use mm_store::view_store::ViewStore;

use crate::App;

/// `count >= model.MaxViewsPerChannel` (app/view.go:26).
///
/// **`>=`, not `>`**: the check is made *before* the insert, so a channel already holding fifty
/// views refuses the fifty-first. Using `>` would allow fifty-one, which no test of a channel
/// with three views can see.
fn channel_is_full(count: i64) -> bool {
    count >= MAX_VIEWS_PER_CHANNEL as i64
}

impl App {
    /// Port of `app.App.CreateView` (app/view.go:17).
    ///
    /// # The nil guard is unreachable and is still here
    ///
    /// `createView` refuses a `null` body with `SetInvalidParamWithErr` before this is called, so
    /// `app.view.create.nil_view.app_error` cannot be produced through the API. Rust's type
    /// system makes it unrepresentable rather than unreachable, which is why there is no branch
    /// for it below — the id is named here so a reader looking for it knows where it went.
    ///
    /// # The count is taken with **zero** options, not the caller's
    ///
    /// `model.ViewQueryOpts{}` — so the limit is measured against every live view in the channel,
    /// never against a page of them. Passing the caller's paging here would let a client past the
    /// limit by asking for `per_page=1`.
    ///
    /// # `IsValid` runs in the store, not here
    ///
    /// So a validation failure arrives as `StoreError::Invalid` carrying `model.view.is_valid.*`
    /// with its own 400 — and `errors.As(err, &appErr)` in Go passes it through **unwrapped**.
    /// That is why a bad title answers `model.view.is_valid.title.app_error` and not
    /// `app.view.create.app_error`; measured against the Go server.
    #[tracing::instrument(skip(self, view), fields(channel_id = %view.channel_id, view_id))]
    pub async fn create_view(&self, view: &mut View, connection_id: &str) -> AppResult<()> {
        let count = self
            .store()
            .view()
            .count_for_channel(&view.channel_id, &ViewQueryOpts::default())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "view count failed");
                AppError::boxed(
                    "CreateView",
                    "app.view.create.count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if channel_is_full(count) {
            return Err(AppError::boxed(
                "CreateView",
                "app.view.create.limit.app_error",
                None,
                "channel has reached the maximum number of views".to_owned(),
                400,
            ));
        }

        self.store().view().save(view).await.map_err(|err| {
            // `errors.As(err, &appErr)` — a validation failure keeps its own id and status.
            if let mm_store::StoreError::Invalid { app_error, .. } = err {
                return app_error;
            }
            tracing::error!(error = %err, "view save failed");
            AppError::boxed(
                "CreateView",
                "app.view.create.app_error",
                None,
                String::new(),
                500,
            )
        })?;
        tracing::Span::current().record("view_id", &view.id);

        self.publish_view_event(WEBSOCKET_EVENT_VIEW_CREATED, view, connection_id)
            .await;

        Ok(())
    }

    /// Port of `app.App.GetView` (app/view.go:43).
    #[tracing::instrument(skip(self), fields(view_id = %view_id))]
    pub async fn get_view(&self, view_id: &str) -> AppResult<View> {
        self.store().view().get(view_id).await.map_err(|err| {
            if err.is_not_found() {
                return AppError::boxed(
                    "GetView",
                    "app.view.get.not_found.app_error",
                    None,
                    String::new(),
                    404,
                );
            }
            tracing::error!(error = %err, "view lookup failed");
            AppError::boxed(
                "GetView",
                "app.view.get.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `app.App.GetViewsForChannel` (app/view.go:58).
    ///
    /// The nil-to-empty normalisation at the end of Go's function is what makes an empty channel
    /// answer `[]` rather than `null` — `SelectBuilder` leaves the slice nil, and without that
    /// line the wire shape would differ between "no views" and "no views, after a delete".
    /// `Vec::new()` is already `[]`, so there is nothing to normalise here; the *handler* is
    /// where it becomes visible.
    #[tracing::instrument(skip(self, opts), fields(channel_id = %channel_id, views))]
    pub async fn get_views_for_channel(
        &self,
        channel_id: &str,
        opts: &ViewQueryOpts,
    ) -> AppResult<Vec<View>> {
        let views = self
            .store()
            .view()
            .get_for_channel(channel_id, opts)
            .await
            .map_err(|err| {
                // `ErrInvalidInput` is a **400** and everything else a 500.
                if err.is_invalid_input() {
                    return AppError::boxed(
                        "GetViewsForChannel",
                        "app.view.get_for_channel.invalid_input.app_error",
                        None,
                        String::new(),
                        400,
                    );
                }
                tracing::error!(error = %err, "views lookup failed");
                AppError::boxed(
                    "GetViewsForChannel",
                    "app.view.get_for_channel.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("views", views.len());
        Ok(views)
    }

    /// Port of `app.App.GetViewsCountForChannel` (app/view.go:75).
    ///
    /// **One branch, not two.** Unlike its sibling above, this one folds `ErrInvalidInput` into
    /// the same 500 as a driver error — so an empty channel id is a 400 from the list and a 500
    /// from the count. Go's asymmetry, reproduced.
    #[tracing::instrument(skip(self, opts), fields(channel_id = %channel_id, count))]
    pub async fn get_views_count_for_channel(
        &self,
        channel_id: &str,
        opts: &ViewQueryOpts,
    ) -> AppResult<i64> {
        let count = self
            .store()
            .view()
            .count_for_channel(channel_id, opts)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "views count failed");
                AppError::boxed(
                    "GetViewsCountForChannel",
                    "app.view.count_for_channel.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    /// Port of `app.App.UpdateView` (app/view.go:83).
    ///
    /// # It clones before patching, and the clone is what gets written
    ///
    /// Go's `view = view.Clone()` exists so the handler's `AddEventPriorState(view.Clone())` — a
    /// clone it took *before* this call — still describes the pre-patch state. The caller here
    /// owns the value, so the clone is the caller's business; this takes the view by value and
    /// returns the patched one.
    ///
    /// # Three error branches and their order
    ///
    /// `errors.As(err, &appErr)` is tried **first**, so a validation failure from `IsValid`
    /// inside the store keeps its own `model.view.is_valid.*` id and 400 — the same passthrough
    /// [`App::create_view`] documents. A not-found is 404, anything else 500.
    #[tracing::instrument(skip(self, view, patch), fields(view_id = %view.id))]
    pub async fn update_view(
        &self,
        mut view: View,
        patch: Option<&ViewPatch>,
        connection_id: &str,
    ) -> AppResult<View> {
        view.patch(patch);

        self.store().view().update(&mut view).await.map_err(|err| {
            if let mm_store::StoreError::Invalid { app_error, .. } = err {
                return app_error;
            }
            if err.is_not_found() {
                return AppError::boxed(
                    "UpdateView",
                    "app.view.update.not_found.app_error",
                    None,
                    String::new(),
                    404,
                );
            }
            tracing::error!(error = %err, "view update failed");
            AppError::boxed(
                "UpdateView",
                "app.view.update.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        self.publish_view_event(WEBSOCKET_EVENT_VIEW_UPDATED, &view, connection_id)
            .await;

        Ok(view)
    }

    /// Port of `app.App.DeleteView` (app/view.go:110).
    ///
    /// `model.GetMillis()` is read **here**, not in the store, so the delete instant is the app
    /// layer's clock — which matters only in that the store cannot be handed a different one.
    ///
    /// The event is built inline rather than through [`App::publish_view_event`] because it
    /// carries `view_id` instead of `view`; see the module docs.
    #[tracing::instrument(skip(self, view), fields(view_id = %view.id, channel_id = %view.channel_id))]
    pub async fn delete_view(&self, view: &View, connection_id: &str) -> AppResult<()> {
        self.store()
            .view()
            .delete(&view.id, get_millis())
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    return AppError::boxed(
                        "DeleteView",
                        "app.view.delete.not_found.app_error",
                        None,
                        String::new(),
                        404,
                    );
                }
                tracing::error!(error = %err, "view delete failed");
                AppError::boxed(
                    "DeleteView",
                    "app.view.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_VIEW_DELETED,
            "",
            &view.channel_id,
            "",
            None,
            connection_id,
        );
        message.add("view_id", serde_json::Value::String(view.id.clone()));
        self.publish(message).await;

        Ok(())
    }

    /// Port of `app.App.UpdateViewSortOrder` (app/view.go:135).
    ///
    /// # The event is published even though the encode can fail, and the response is not
    ///
    /// Go marshals the list, logs a warning on failure and **skips only the publish**, then
    /// returns the views regardless. So a caller always gets its answer; only the broadcast can
    /// be lost. Reproduced, including which of the two is sacrificed.
    #[tracing::instrument(skip(self), fields(view_id = %view_id, channel_id = %channel_id, new_index))]
    pub async fn update_view_sort_order(
        &self,
        view_id: &str,
        channel_id: &str,
        new_index: i64,
        connection_id: &str,
    ) -> AppResult<Vec<View>> {
        let views = self
            .store()
            .view()
            .update_sort_order(view_id, channel_id, new_index)
            .await
            .map_err(|err| {
                // `ErrInvalidInput` is tried **before** `ErrNotFound`: an index past the end of
                // the list is a 400, a view that is not in the list is a 404.
                if err.is_invalid_input() {
                    return AppError::boxed(
                        "UpdateViewSortOrder",
                        "app.view.update_sort_order.invalid_input.app_error",
                        None,
                        String::new(),
                        400,
                    );
                }
                if err.is_not_found() {
                    return AppError::boxed(
                        "UpdateViewSortOrder",
                        "app.view.update_sort_order.not_found.app_error",
                        None,
                        String::new(),
                        404,
                    );
                }
                tracing::error!(error = %err, "view sort order update failed");
                AppError::boxed(
                    "UpdateViewSortOrder",
                    "app.view.update_sort_order.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        match serde_json::to_string(&views) {
            Ok(views_json) => {
                let mut message = WebSocketEvent::new(
                    WEBSOCKET_EVENT_VIEW_SORTED,
                    "",
                    channel_id,
                    "",
                    None,
                    connection_id,
                );
                message.add("views", serde_json::Value::String(views_json));
                self.publish(message).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "Failed to encode views to JSON for websocket");
            }
        }

        Ok(views)
    }

    /// Port of `app.App.GetPostsForView` (app/post.go:1367).
    ///
    /// **It lives in `post.go`, not `view.go`**, and it is the only one of the seven that does.
    /// Ported here anyway because `getPostsForView` is the only caller and `post.rs` belongs to
    /// another migration unit.
    ///
    /// # It is `GetPostsPage` with two ids changed
    ///
    /// The store call is identical — `Post().GetPosts(options, false, sanitizeOptions)` — and so
    /// are the four stages after it, every one of which [`App::get_posts_page`] documents as inert
    /// or unreachable here. What differs is the error vocabulary: `ErrInvalidInput` is
    /// `app.post.get_posts.app_error` at **400** and everything else
    /// `app.post.get_posts_for_view.app_error` at 500, where `GetPostsPage` says
    /// `app.post.get_root_posts.app_error`. A client branching on `id` can tell the two routes
    /// apart, so reusing the neighbour would be wrong on the wire.
    ///
    /// Go's own comment says the view's configuration is not consulted yet: "For now, it returns
    /// all posts in the channel." So `view_id` reaches this route, is validated, and then changes
    /// nothing about the answer — a `TODO` that is part of the wire contract until it is not.
    #[tracing::instrument(skip(self, opts), fields(channel_id = %opts.channel_id))]
    pub async fn get_posts_for_view(
        &self,
        opts: mm_store::post_store::GetPostsOptions<'_>,
    ) -> AppResult<mm_model::post_list::PostList> {
        use mm_store::post_store::PostStore;
        self.store().post().get_posts(opts).await.map_err(|err| {
            if err.is_invalid_input() {
                return AppError::boxed(
                    "GetPostsForView",
                    "app.post.get_posts.app_error",
                    None,
                    String::new(),
                    400,
                );
            }
            tracing::error!(error = %err, "post page lookup for a view failed");
            AppError::boxed(
                "GetPostsForView",
                "app.post.get_posts_for_view.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `app.App.publishViewEvent` (app/view.go:161) — unexported in Go.
    ///
    /// The broadcast names the **channel** and neither a team nor a user, so every connected
    /// member of the channel is told and nobody else is. `connection_id` becomes
    /// `omit_connection_id`, which is how the client that made the change avoids being told about
    /// its own write twice.
    async fn publish_view_event(&self, event_type: &str, view: &View, connection_id: &str) {
        let Ok(view_json) = serde_json::to_string(view).inspect_err(|err| {
            tracing::warn!(error = %err, "Failed to encode view to JSON");
        }) else {
            // Go returns without publishing. Unreachable for this type; see the module docs.
            return;
        };

        let mut message =
            WebSocketEvent::new(event_type, "", &view.channel_id, "", None, connection_id);
        message.add("view", serde_json::Value::String(view_json));
        self.publish(message).await;
    }
}

#[cfg(test)]
mod tests {
    use mm_model::view::{View, ViewPatch};

    fn kanban_props(field_id: &str) -> mm_model::utils::StringInterface {
        let mut props = mm_model::utils::StringInterface::new();
        props.insert(
            "group_by".into(),
            serde_json::json!({
                "field_id": field_id,
                "columns": [{"id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "name": "Todo", "option_ids": ["o1"]}]
            }),
        );
        props
    }

    fn a_view() -> View {
        View {
            id: "bbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            channel_id: "cccccccccccccccccccccccccc".into(),
            view_type: "kanban".into(),
            creator_id: "dddddddddddddddddddddddddd".into(),
            title: "Board".into(),
            description: String::new(),
            sort_order: 0,
            props: Some(kanban_props("eeeeeeeeeeeeeeeeeeeeeeeeee")),
            create_at: 1,
            update_at: 1,
            delete_at: 0,
        }
    }

    /// The websocket payload is a JSON document inside a JSON **string** — the shape a client
    /// has to `JSON.parse` a second time. Asserted because "just nest the object" is the obvious
    /// wrong simplification.
    #[test]
    fn the_view_event_payload_is_a_json_string_not_an_object() {
        let view = a_view();
        let encoded = serde_json::to_string(&view).expect("a view serialises");
        let value = serde_json::Value::String(encoded.clone());
        assert!(value.is_string(), "the payload must be a string");
        let reparsed: View = serde_json::from_str(&encoded).expect("it round-trips");
        assert_eq!(reparsed, view);
    }

    /// `MaxViewsPerChannel` is refused at `>=`, so fifty existing views blocks the fifty-first.
    ///
    /// The boundary is the whole content: `>` instead of `>=` differs only at exactly 50, and
    /// every end-to-end fixture in this repo has three views.
    #[test]
    fn the_create_limit_refuses_at_the_maximum_not_past_it() {
        assert_eq!(mm_model::view::MAX_VIEWS_PER_CHANNEL, 50);
        assert!(!super::channel_is_full(0));
        assert!(!super::channel_is_full(48));
        assert!(!super::channel_is_full(49), "the 50th view still fits");
        assert!(super::channel_is_full(50), "the 51st does not");
        assert!(super::channel_is_full(51));
    }

    /// `UpdateView` patches then writes; a patch with every field null must leave the view
    /// untouched except for `UpdateAt`, which the store moves.
    #[test]
    fn an_all_null_patch_changes_nothing() {
        let mut view = a_view();
        let before = view.clone();
        view.patch(Some(&ViewPatch::default()));
        assert_eq!(view, before);
    }

    /// A patch carrying an empty string is applied — the guard is on the pointer, not the value —
    /// which is how `{"title":""}` reaches `IsValid` and becomes a 400 rather than a no-op.
    #[test]
    fn a_patch_carrying_an_empty_title_is_applied_and_then_fails_validation() {
        let mut view = a_view();
        view.patch(Some(&ViewPatch {
            title: Some(String::new()),
            ..ViewPatch::default()
        }));
        assert_eq!(view.title, "");
        let err = view.is_valid().expect_err("an empty title is invalid");
        assert_eq!(err.id, "model.view.is_valid.title.app_error");
        assert_eq!(err.status_code, 400);
    }
}
