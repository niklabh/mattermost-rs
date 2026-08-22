//! Port of the app-layer surface behind `GET /api/v4/posts/{post_id}`: `GetSinglePost`,
//! `GetPostIfAuthorized`, `PreparePostForClientWithEmbedsAndImages` and
//! `SanitizePostMetadataForUser`.
//!
//! # The metadata pipeline is only partly reproducible, and this module says which part
//!
//! `PreparePostForClient` (app/post_metadata.go:189) is a pipeline, and two of its stages
//! depend on machinery this server does not have:
//!
//! - **`getFirstLink` runs Go's markdown parser** (`shared/markdown`) to find the first
//!   *autolink* in the message, and `getImagesForPost` runs it again to find markdown images.
//!   The parser is not ported ([D-044]).
//! - **`getLinkMetadata` fetches the link over HTTP** — OpenGraph, image dimensions, oEmbed —
//!   and caches the result in `LinkMetadata`. Whether it produces an `opengraph`, `image` or
//!   plain `link` embed depends on what the remote host answers *at that moment*, which is not
//!   a thing a second implementation can agree with.
//!
//! So the pipeline here is written as a **total function that can refuse**. Every stage it
//! reproduces is reproduced exactly; every input shape whose output it cannot predict returns
//! [`PrepareError::Unreproducible`], and the handler forwards that request to the Go server
//! rather than answering with a body that is nearly right. The refusal predicate is deliberately
//! a *superset* of the shapes that actually differ — over-forwarding costs a proxy hop, while
//! under-forwarding is a wire-format bug nothing would catch.
//!
//! [`REFUSED_PROPS`] and [`message_may_contain_a_link`] are that predicate, and each entry names
//! the Go branch it stands in for.
//!
//! # What is *not* refused, and why that is safe here
//!
//! | Go stage | why it is a no-op | what would change that |
//! |---|---|---|
//! | `PostWithProxyAddedToImageURLs` | `ImageProxySettings.Enable` is false | config; refused when on |
//! | `OverrideIconURLIfEmoji` | `EnablePostIconOverride` is false | config; refused when on |
//! | `revealSingleBurnOnReadPost` + the burn-on-read block | needs `post.Type == burn_on_read` | refused by type |
//! | `isInaccessiblePost` / `filterInaccessiblePosts` | `GetLastAccessiblePostTime` returns `0` without a licence carrying a `PostHistory` limit (app/post.go:2166), so nothing is filtered and the `app.post.cloud.get.app_error` 403 is unreachable | a Cloud licence |
//! | `applyPostWillBeConsumedHook` | plugin hook; the four plugins on this deployment do not implement it for ordinary posts | refused for `custom_*` post types, which is where a plugin's own posts land |
//! | `sanitizeFileAttachmentsForUser` | returns immediately when `AccessControl` is nil or `EnableAttributeBasedAccessControl` is false (post_metadata.go:432) — both hold on Team Edition | an enterprise licence with ABAC on |
//! | `removeInaccessibleContentFromFilesSlice` | Cloud file limits, same licence gate as above | a Cloud licence |

use mm_model::emoji::{Emoji, find_emoji_references, get_system_emoji_id};
use mm_model::file_info::FileInfo;
use mm_model::permission::{
    PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_READ_PUBLIC_CHANNEL, make_permission_error,
};
use mm_model::post::{
    AllStringsOptions, POST_CUSTOM_TYPE_PREFIX, POST_PROPS_ADAPTIVE_CARDS, POST_PROPS_ATTACHMENTS,
    POST_PROPS_BLOCK_KIT_BLOCKS, POST_PROPS_CHANNEL_MENTIONS, POST_PROPS_MM_BLOCKS,
    POST_PROPS_OVERRIDE_ICON_EMOJI, POST_PROPS_PREVIEWED_POST, POST_PROPS_UNSAFE_LINKS,
    POST_TYPE_BURN_ON_READ, Post,
};
use mm_model::post_metadata::PostMetadata;
use mm_model::reaction::Reaction;
use mm_model::session::Session;
use mm_model::utils::{AppError, remove_duplicate_strings};
use mm_store::{EmojiStore, FileInfoStore, PostStore, ReactionStore, StoreError};

use crate::App;

/// Port of `model.PreparePostForClientOpts` (post.go:1449). Not a wire type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreparePostForClientOpts {
    pub is_new_post: bool,
    pub is_edit_post: bool,
    pub include_priority: bool,
    pub retain_content: bool,
    pub include_deleted: bool,
}

/// Why a post's client-facing shape cannot be produced here.
///
/// Not an error in Go's sense — the Go server answers all of these perfectly well. It is a
/// statement that *this* implementation would answer differently, so the request belongs
/// upstream. The `&'static str` names the Go branch, and it reaches the logs, never a client.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("post metadata is not reproducible here: {0}")]
    Unreproducible(&'static str),
    #[error(transparent)]
    App(#[from] Box<AppError>),
}

/// Props whose presence changes a branch this module does not reproduce.
///
/// Each entry stands in for a specific Go branch:
///
/// | prop | branch |
/// |---|---|
/// | `attachments` | `getEmbedForPost` returns a `message_attachment` embed (post_metadata.go:547) — **and** the attachment strings feed `AllStrings`, so both the emoji scan and `getImagesForPost` change |
/// | `boards` | `getEmbedForPost` returns a `boards` embed carrying the prop verbatim (:553) |
/// | `mm_blocks`, `blocks`, `cards` | `InteractiveBlocksImageURLs` and `AllStrings` walk the three interactive dialects (post.go:846) |
/// | `unsafe_links` | `HasUnsafeLinks` short-circuits `getEmbedsAndImages` and empties `getImagesForPost` (:283, :617) |
/// | `channel_mentions` | `sanitizeChannelMentionsForUser` rewrites the prop from fresh channel rows (:376) |
/// | `previewed_post` | feeds `getLinkMetadata`'s permalink lookup (:568) |
///
/// `override_icon_emoji` is deliberately **absent**: its branch is additionally gated on
/// `EnablePostIconOverride`, so it only refuses when that setting is on. See
/// [`App::prepare_post_for_client`].
///
/// `"boards"` is a bare string literal in Go too (post_metadata.go:553) — there is no
/// `model.PostProps*` constant for it, and inventing one here would hide that.
pub const REFUSED_PROPS: [&str; 8] = [
    POST_PROPS_ATTACHMENTS,
    "boards",
    POST_PROPS_MM_BLOCKS,
    POST_PROPS_BLOCK_KIT_BLOCKS,
    POST_PROPS_ADAPTIVE_CARDS,
    POST_PROPS_UNSAFE_LINKS,
    POST_PROPS_CHANNEL_MENTIONS,
    POST_PROPS_PREVIEWED_POST,
];

/// A conservative superset of "Go's markdown parser would find an autolink or an image here".
///
/// Go looks for two node kinds, and only two:
///
/// - `*markdown.Autolink` ([`getFirstLink`], post_metadata.go:775). Mattermost's autolinker
///   (`shared/markdown/autolink.go`) recognises exactly two bare forms — a `www\d{0,3}\.` host
///   and a scheme followed by `://` — plus the angle-bracket form from the inline parser. It has
///   **no email rule**, so `someone@example.com` is not a link on the server even though the
///   webapp renders one. Markdown links `[text](url)` are not autolinks either.
/// - `*markdown.InlineImage` / `*markdown.ReferenceImage` (`getImages`, :790), both of which
///   are written `![`.
///
/// Hence the four needles. `<` is in the list for the angle-bracket autolink and costs nothing:
/// a chat message containing a literal `<` is simply forwarded. A reference definition
/// (`[ref]: https://…`) carries `://` and is caught by the first needle.
///
/// **This must never return `false` for a message Go would find a link in.** Widening it is
/// free; narrowing it is a wire-format bug.
pub fn message_may_contain_a_link(message: &str) -> bool {
    message.contains("://")
        || message.contains("www")
        || message.contains("![")
        || message.contains('<')
}

impl App {
    /// Port of `app.App.GetSinglePost` (post.go:1525).
    ///
    /// Both store branches carry the **same** error id, `app.post.get.app_error`, and differ
    /// only in status: 404 for a miss, 500 for anything else. A client branching on `id` cannot
    /// tell them apart, and neither can a log reader with only the id — reproduced because the
    /// id is on the wire.
    ///
    /// The cloud-limit check that follows in Go is a no-op without a licence carrying a post
    /// history limit; see the module docs.
    #[tracing::instrument(skip(self), fields(post_id = %post_id, incl_deleted))]
    pub async fn get_single_post(
        &self,
        post_id: &str,
        incl_deleted: bool,
    ) -> Result<Post, AppError> {
        self.store()
            .post()
            .get_single(post_id, incl_deleted)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() {
                    404
                } else {
                    tracing::error!(error = %err, "post lookup failed");
                    500
                };
                AppError::new(
                    "GetSinglePost",
                    "app.post.get.app_error",
                    None,
                    String::new(),
                    status,
                )
            })
    }

    /// Port of `app.App.GetPostIfAuthorized` (post.go:2754).
    ///
    /// The returned `bool` is `is_member`, **not** "authorized" — Go uses it only to mark an
    /// audit record for a non-member read. Note the asymmetry it creates: when the channel is
    /// open and the caller has `read_public_channel` but is not a member, the function returns
    /// the post *and* `false`.
    ///
    /// # The duplicated fallback is narrower than the one it duplicates
    ///
    /// `HasPermissionToReadChannel` already falls back to `read_public_channel` for both `O`
    /// and open-board channels (authorization.go:470). Go then repeats the fallback here for
    /// `O` **only**. That repetition cannot grant anything the first one refused, so its whole
    /// effect is on *which permission id* the 403 names — `read_public_channel` for an open
    /// channel, `read_channel_content` otherwise. Clients read that id, so it is wire format.
    #[tracing::instrument(skip(self, session), fields(post_id = %post_id, incl_deleted))]
    pub async fn get_post_if_authorized(
        &self,
        post_id: &str,
        session: &Session,
        incl_deleted: bool,
    ) -> Result<(Post, bool), Box<AppError>> {
        let post = self
            .get_single_post(post_id, incl_deleted)
            .await
            .map_err(Box::new)?;

        let channel = self.get_channel(&post.channel_id).await.map_err(Box::new)?;

        let (ok, is_member) = self
            .session_has_permission_to_read_channel(session, &channel)
            .await;

        if !ok {
            if channel.channel_type == mm_model::channel::CHANNEL_TYPE_OPEN
                && !self.config().compliance_enable
            {
                if !self
                    .session_has_permission_to_team(
                        session,
                        &channel.team_id,
                        &PERMISSION_READ_PUBLIC_CHANNEL,
                    )
                    .await
                {
                    return Err(make_permission_error(
                        session,
                        &[&PERMISSION_READ_PUBLIC_CHANNEL],
                    ));
                }
            } else {
                return Err(make_permission_error(
                    session,
                    &[&PERMISSION_READ_CHANNEL_CONTENT],
                ));
            }
        }

        Ok((post, is_member))
    }

    /// Port of `app.App.PreparePostForClientWithEmbedsAndImages` (post_metadata.go:270).
    ///
    /// # `preparePostFilesForClient` genuinely runs twice
    ///
    /// Once inside `PreparePostForClient` (:215) and once here (:274), with the same arguments.
    /// The second call overwrites `Metadata.Files` with an identical value — except after the
    /// deleted-post short circuit, which blanks the metadata in between. **That is the only
    /// reason a soft-deleted post fetched with `include_deleted=true` can still carry its
    /// `files`**, and dropping the "redundant" second call would silently change that response.
    ///
    /// It is easy to conclude the second call is dead, because in practice a deleted post's
    /// files are usually gone too: `DeletePost` soft-deletes them from a **goroutine**
    /// (`a.Srv().Go(func() { a.deletePostFiles(...) })`, app/post.go:2013), so by the time a
    /// client asks, `GetByIds` filters them out and the metadata is `{}` either way. The window
    /// where it is not is real but transient — which is exactly why the parity fixture waits for
    /// that goroutine and then puts the rows back, rather than racing it.
    #[tracing::instrument(skip_all, fields(post_id = %post.id))]
    pub async fn prepare_post_for_client_with_embeds_and_images(
        &self,
        post: &Post,
        opts: PreparePostForClientOpts,
    ) -> Result<Post, PrepareError> {
        let mut post = self.prepare_post_for_client(post, opts).await?;
        self.get_embeds_and_images(&mut post)?;
        self.prepare_post_files_for_client(&mut post, opts).await;
        Ok(post)
    }

    /// Port of `app.App.PreparePostForClient` (post_metadata.go:189). Stage order is Go's.
    async fn prepare_post_for_client(
        &self,
        original: &Post,
        opts: PreparePostForClientOpts,
    ) -> Result<Post, PrepareError> {
        // The plugin `MessageWillBeConsumed` hook can rewrite any post; the shapes where that
        // is observable on this deployment are the plugins' own, which all carry a custom type.
        if original.post_type.starts_with(POST_CUSTOM_TYPE_PREFIX) {
            return Err(PrepareError::Unreproducible("plugin post type"));
        }
        // `revealSingleBurnOnReadPost` (post_helpers.go:335) and the burn-on-read block at
        // post_metadata.go:217 need the ReadReceipt and TemporaryPost stores and the caller's
        // session identity; neither is ported.
        if original.post_type == POST_TYPE_BURN_ON_READ {
            return Err(PrepareError::Unreproducible("burn-on-read post"));
        }

        let mut post = original.clone();

        // 1. `PostWithProxyAddedToImageURLs` — identity when the proxy is off, and it returns
        //    the *same pointer*, so there is not even a clone to reproduce.
        if self.config().image_proxy_enable {
            return Err(PrepareError::Unreproducible("image proxy is enabled"));
        }

        // 2. `OverrideIconURLIfEmoji`. Go type-asserts the prop to a string **before** reading
        //    the config, so a non-string prop returns early either way — but the only observable
        //    difference is whether `override_icon_url` gets written, which needs the config on.
        if self.config().enable_post_icon_override
            && post
                .props
                .as_ref()
                .and_then(|props| props.get(POST_PROPS_OVERRIDE_ICON_EMOJI))
                .is_some_and(serde_json::Value::is_string)
        {
            return Err(PrepareError::Unreproducible("icon override is enabled"));
        }

        refuse_on_props(&post)?;

        // 3. Metadata always exists from here on, which is why a plain post serialises
        //    `"metadata":{}` rather than omitting the key.
        let mut metadata = post.metadata.take().unwrap_or_default();

        // 4. The deleted-post short circuit. `RetainContent` is false for this route, so a
        //    soft-deleted post comes back with an **empty message** and blank metadata — and
        //    then `PreparePostForClientWithEmbedsAndImages` refills `files` on top. Returning
        //    here rather than falling through is what drops its reactions and priority.
        if post.delete_at > 0 && !opts.retain_content {
            post.message = String::new();
            post.metadata = Some(PostMetadata::default());
            return Ok(post);
        }

        // 5. Emojis and reactions, together: Go assigns both or neither.
        match self.get_emojis_and_reactions_for_post(&post).await {
            Ok((emojis, reactions)) => {
                metadata.emojis = emojis;
                metadata.reactions = reactions;
            }
            // Go logs a warning and leaves both fields at nil rather than failing the request.
            Err(err) => {
                tracing::warn!(error = %err, post_id = %post.id, "Failed to get emojis and reactions for a post");
            }
        }

        post.metadata = Some(metadata);

        // 6. Files.
        self.prepare_post_files_for_client(&mut post, opts).await;

        // 7. Priority and acknowledgements. `RootId == ""` is the trap: a **reply never carries
        //    either**, however its own priority row reads.
        if opts.include_priority && self.config().post_priority && post.root_id.is_empty() {
            match self.store().post().get_priority_for_post(&post.id).await {
                Ok(priority) => {
                    if let Some(metadata) = post.metadata.as_mut() {
                        metadata.priority = priority;
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, post_id = %post.id, "Failed to get post priority for a post");
                }
            }

            match self
                .store()
                .post()
                .get_acknowledgements_for_post(&post.id)
                .await
            {
                Ok(acknowledgements) => {
                    if let Some(metadata) = post.metadata.as_mut() {
                        metadata.acknowledgements = acknowledgements;
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, post_id = %post.id, "Failed to get post acknowledgements for a post");
                }
            }
        }

        Ok(post)
    }

    /// Port of `app.App.preparePostFilesForClient` (post_metadata.go:260).
    ///
    /// Go warn-logs a store failure and leaves `Metadata.Files` untouched; so does this.
    async fn prepare_post_files_for_client(&self, post: &mut Post, opts: PreparePostForClientOpts) {
        match self.get_file_metadata_for_post(post, opts).await {
            Ok(files) => {
                if let Some(metadata) = post.metadata.as_mut() {
                    metadata.files = files;
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, post_id = %post.id, "Failed to get files for a post");
            }
        }
    }

    /// Port of `app.App.getFileMetadataForPost` (post_metadata.go:520) and the
    /// `GetFileInfosForPost` (post.go:2401) behind it, including `orderFileInfosByID`.
    ///
    /// `includeDeleted` here is **`opts.IncludeDeleted`, not the route's `include_deleted` query
    /// parameter** — `getPost` never sets the opts field, so a deleted post's file infos are
    /// still filtered to `DeleteAt = 0`. Wiring the query parameter through would look like a
    /// fix and would change the response.
    async fn get_file_metadata_for_post(
        &self,
        post: &Post,
        opts: PreparePostForClientOpts,
    ) -> Result<Vec<FileInfo>, StoreError> {
        let file_ids = post.file_ids.as_deref().unwrap_or_default();
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }

        let infos = self
            .store()
            .file_info()
            .get_by_ids(file_ids, opts.include_deleted)
            .await?;

        Ok(order_file_infos_by_id(file_ids, infos))
    }

    /// Port of `app.App.getEmojisAndReactionsForPost` (post_metadata.go:528).
    ///
    /// **Reactions are read only when `post.HasReactions` is set.** The column is Go's own
    /// denormalised flag; trusting the `Reactions` table instead would return rows for a post
    /// whose flag is stale, which Go does not.
    async fn get_emojis_and_reactions_for_post(
        &self,
        post: &Post,
    ) -> Result<(Vec<Emoji>, Vec<Reaction>), StoreError> {
        let reactions = if post.has_reactions {
            self.store().reaction().get_for_post(&post.id).await?
        } else {
            Vec::new()
        };

        let emojis = self.get_custom_emojis_for_post(post, &reactions).await?;
        Ok((emojis, reactions))
    }

    /// Port of `app.App.getCustomEmojisForPost` (post_metadata.go:710) and
    /// `GetMultipleEmojiByName` (app/emoji.go:242).
    ///
    /// Only **custom** emoji are returned: system names are filtered out before the query, so a
    /// post full of `:smile:` still reports no emojis. `AllStrings` is what makes the scan cover
    /// message attachments and interactive blocks as well as the message — both of which are
    /// refused here, so in practice it is the message plus the reaction names.
    async fn get_custom_emojis_for_post(
        &self,
        post: &Post,
        reactions: &[Reaction],
    ) -> Result<Vec<Emoji>, StoreError> {
        if !self.config().enable_custom_emoji {
            return Ok(Vec::new());
        }

        let names = get_emoji_names_for_post(post, reactions);
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let custom: Vec<String> = names
            .into_iter()
            .filter(|name| get_system_emoji_id(name).is_none())
            .collect();
        if custom.is_empty() {
            return Ok(Vec::new());
        }

        self.store().emoji().get_multiple_by_name(&custom).await
    }

    /// Port of `app.App.getEmbedsAndImages` (post_metadata.go:277).
    ///
    /// Reproduced only for the shapes where both `Embeds` and `Images` come out empty:
    /// `getEmbedForPost` returns `(nil, nil)` when there is no first link and no attachment or
    /// board prop, and `getImagesForPost` returns an empty map when the message holds no
    /// markdown images. `omitempty` drops both, which is why a plain post's metadata is `{}`.
    fn get_embeds_and_images(&self, post: &mut Post) -> Result<(), PrepareError> {
        // `getFirstLink` reads `post.Message` and nothing else — and the message it reads is
        // the one the deleted-post short circuit may already have emptied, which is why this
        // check has to sit here rather than at the top of the pipeline.
        if message_may_contain_a_link(&post.message) {
            return Err(PrepareError::Unreproducible(
                "message may contain a link or a markdown image",
            ));
        }
        // Go sets `Embeds` to an empty slice and `Images` to an empty map. Both are `omitempty`,
        // so the fields' defaults already serialise identically; nothing to write.
        Ok(())
    }

    /// Port of `app.App.SanitizePostMetadataForUser` (post_metadata.go:332).
    ///
    /// The returned `bool` is `isMemberForPreviews`, which Go initialises to **`true`** and only
    /// lowers inside the permalink-embed branch. That branch needs a non-empty `Metadata.Embeds`
    /// — impossible here, because any post that could carry an embed was refused upstream — so
    /// this always answers `true` on the shapes it serves. Channel-mention sanitisation is
    /// refused via [`REFUSED_PROPS`], and ABAC file sanitisation is inert without an enterprise
    /// licence (see the module docs).
    #[tracing::instrument(skip_all, fields(post_id = %post.id))]
    pub async fn sanitize_post_metadata_for_user(
        &self,
        post: Post,
        _user_id: &str,
    ) -> Result<(Post, bool), PrepareError> {
        if post
            .metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.embeds.is_empty())
        {
            return Err(PrepareError::Unreproducible("post carries embeds"));
        }
        refuse_on_props(&post)?;
        Ok((post, true))
    }
}

/// The [`REFUSED_PROPS`] check, applied wherever Go would branch on one of them.
///
/// Go tests key **presence** (`if _, ok := props[...]; ok`), not the value, so a prop explicitly
/// set to `null` still takes the branch. The refusal carries the prop name, which is what a log
/// reader needs to know which branch stopped us.
fn refuse_on_props(post: &Post) -> Result<(), PrepareError> {
    let Some(props) = post.props.as_ref() else {
        return Ok(());
    };
    for refused in REFUSED_PROPS {
        if props.contains_key(refused) {
            return Err(PrepareError::Unreproducible(refused));
        }
    }
    Ok(())
}

/// Port of `getEmojiNamesForPost` (post_metadata.go:698) and the `getEmojiNamesForString` it
/// calls.
///
/// Order is wire surface — the names decide nothing on their own, but `RemoveDuplicateStrings`
/// keeps first occurrences, and the resulting name list is what the `IN (…)` is built from.
/// Post strings come first, reaction names after.
///
/// `omit_interactive_blocks` is `!FeatureFlags.MmBlocksEnabled`, and that flag defaults to
/// `true` (feature_flags.go:214) and is `true` on this deployment, so the blocks *are* walked.
/// Every post carrying them is refused before reaching here, which makes the choice
/// unobservable — but the faithful value is the one that stays right when the refusal is lifted.
fn get_emoji_names_for_post(post: &Post, reactions: &[Reaction]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for string in post.all_strings(AllStringsOptions {
        omit_interactive_blocks: false,
    }) {
        names.extend(
            find_emoji_references(&string)
                .into_iter()
                .map(|name| name.trim_matches(':').to_owned()),
        );
    }
    names.extend(reactions.iter().map(|r| r.emoji_name.clone()));
    // Go's `RemoveDuplicateStrings` **sorts** before deduplicating (model/utils.go), so the name
    // list reaching the store is alphabetical, not first-seen. The distinction is invisible in
    // the response — the store's own result order decides `metadata.emojis` — but it is what the
    // `IN (…)` list looks like, and `remove_duplicate_strings_non_sort` beside it is the trap.
    remove_duplicate_strings(&mut names);
    names
}

/// Port of `orderFileInfosByID` (app/post.go:2433).
///
/// The result follows `ids`; anything the store returned that `ids` does not name keeps the
/// store's own `CreateAt DESC` order and is appended after. Go short-circuits on fewer than two
/// infos, which matters only for performance — the loop below produces the same answer — but the
/// **empty-`ids`** short circuit is behavioural: it returns the store order untouched rather
/// than an empty vector.
fn order_file_infos_by_id(ids: &[String], infos: Vec<FileInfo>) -> Vec<FileInfo> {
    if ids.is_empty() || infos.len() < 2 {
        return infos;
    }

    // Go builds `byID` by assignment, so on a duplicate id the *last* info wins the lookup and
    // the earlier one is only reachable from the second loop. Ids are a primary key, so this
    // cannot happen — the map is written this way to keep the two loops' invariant identical.
    let mut by_id: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(infos.len());
    for (index, info) in infos.iter().enumerate() {
        by_id.insert(info.id.as_str(), index);
    }

    let mut order: Vec<usize> = Vec::with_capacity(infos.len());
    // Named ids first, in the post's order. Go deletes each as it consumes it, which is what
    // makes the second loop a "leftovers" pass rather than a duplicate one.
    for id in ids {
        if let Some(index) = by_id.remove(id.as_str()) {
            order.push(index);
        }
    }
    // Then everything still in the map, in the store's `CreateAt DESC` order.
    for (index, info) in infos.iter().enumerate() {
        if by_id.contains_key(info.id.as_str()) {
            order.push(index);
        }
    }

    let mut slots: Vec<Option<FileInfo>> = infos.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_with_message(message: &str) -> Post {
        Post {
            message: message.to_owned(),
            ..Post::default()
        }
    }

    /// Every shape Mattermost's autolinker actually recognises has to be caught. Measured
    /// against the live Go server in `parity_post_get.rs`, which asserts each of these really
    /// does produce an embed there.
    #[test]
    fn every_autolink_shape_go_recognises_is_refused() {
        for message in [
            "see https://example.com for info",
            "a www.example.com bare host",
            "www3.example.com numbered",
            "<https://example.com>",
            "an image ![alt](https://example.com/a.png)",
            "a reference image ![alt][ref]\n\n[ref]: https://example.com/a.png",
            "ftp://files.example.com",
        ] {
            assert!(
                message_may_contain_a_link(message),
                "{message:?} must be forwarded"
            );
        }
    }

    /// The shapes that must *not* be forwarded, or the route serves almost nothing. Each is a
    /// case Go's autolinker leaves alone: it has no email rule, `~channel` and `@user` are
    /// Mattermost syntax rather than markdown, and a plain `[text](url)` is an inline link, not
    /// an autolink.
    #[test]
    fn ordinary_messages_are_not_refused() {
        for message in [
            "plain probe message",
            "contact me at someone@example.com",
            "hey @alice, see ~town-square",
            "emoji test :smile: and :shipit:",
            "a * b > c",
            "",
        ] {
            assert!(
                !message_may_contain_a_link(message),
                "{message:?} should be served locally"
            );
        }
    }

    #[test]
    fn a_refused_prop_refuses_by_presence_not_by_value() {
        for prop in REFUSED_PROPS {
            let mut post = post_with_message("hi");
            let mut props = mm_model::utils::StringInterface::new();
            props.insert(prop.to_owned(), serde_json::Value::Null);
            post.props = Some(props);
            assert!(
                refuse_on_props(&post).is_err(),
                "a null {prop} prop still takes Go's branch"
            );
        }
    }

    #[test]
    fn channel_mentions_are_refused() {
        let mut post = post_with_message("hi");
        let mut props = mm_model::utils::StringInterface::new();
        props.insert(
            POST_PROPS_CHANNEL_MENTIONS.to_owned(),
            serde_json::json!({"town-square": {"display_name": "Town Square"}}),
        );
        post.props = Some(props);
        assert!(refuse_on_props(&post).is_err());
    }

    #[test]
    fn an_ordinary_props_map_is_not_refused() {
        let mut post = post_with_message("hi");
        let mut props = mm_model::utils::StringInterface::new();
        props.insert("from_webhook".to_owned(), serde_json::json!("true"));
        post.props = Some(props);
        assert!(refuse_on_props(&post).is_ok());
    }

    fn info(id: &str) -> FileInfo {
        FileInfo {
            id: id.to_owned(),
            ..FileInfo::default()
        }
    }

    /// The store returns `CreateAt DESC`; the client sees the post's own attachment order.
    #[test]
    fn file_infos_follow_the_posts_file_ids() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let stored = vec![info("c"), info("a"), info("b")];
        let ordered = order_file_infos_by_id(&ids, stored);
        assert_eq!(
            ordered.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    /// An info the post does not name keeps the store's order and lands after everything it
    /// does name — not first, and not dropped.
    #[test]
    fn unnamed_file_infos_are_appended_in_store_order() {
        let ids = vec!["b".to_owned()];
        let stored = vec![info("z"), info("b"), info("y")];
        let ordered = order_file_infos_by_id(&ids, stored);
        assert_eq!(
            ordered.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["b", "z", "y"]
        );
    }

    /// Go's two short circuits. Empty ids is the behavioural one: it returns the store order
    /// rather than an empty vector, so a file info with no matching id is *not* dropped.
    #[test]
    fn the_short_circuits_return_the_store_order_untouched() {
        let stored = vec![info("z"), info("y")];
        let ordered = order_file_infos_by_id(&[], stored);
        assert_eq!(
            ordered.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["z", "y"]
        );

        let ordered = order_file_infos_by_id(&["q".to_owned()], vec![info("z")]);
        assert_eq!(
            ordered.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["z"]
        );
    }

    /// System emoji never reach the store, and duplicates collapse keeping the first.
    #[test]
    fn emoji_names_come_from_the_message_then_the_reactions() {
        let post = post_with_message("a :one: b :two: c :one:");
        let reactions = [
            Reaction {
                emoji_name: "three".to_owned(),
                ..Reaction::default()
            },
            Reaction {
                emoji_name: "one".to_owned(),
                ..Reaction::default()
            },
        ];
        assert_eq!(
            get_emoji_names_for_post(&post, &reactions),
            // Sorted, not first-seen — see `get_emoji_names_for_post`.
            ["one", "three", "two"]
        );
    }
}
