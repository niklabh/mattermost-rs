//! Port of the `model.Config` settings something ported actually reads — and nothing else.
//!
//! `config.go` is 5,795 lines across 53 structs and this models sixteen settings from six
//! sections. That is not a staging post on the way to translating the rest: the document is read
//! whole from the database and the unmodelled sections are simply ignored, so a field appears here
//! when a reader needs it and its doc comment names that reader. **A config field with no reader
//! is a guess about a wire format nothing can falsify** — the same rule the rest of the project
//! applies to model files.
//!
//! Growing it costs one `Option<T>` in [`Document`], one line in [`Config::from_document`], one
//! key in `scripts/dump-config-fixture.sh`, and the count in
//! `the_fixture_covers_every_document_sourced_setting`.
//!
//! # Where these values come from
//!
//! From the **shared database**, then overlaid with the environment — in that order, which is Go's
//! own order and not an arbitrary one.
//!
//! This used to be environment-only, because Go's default backing store is `config.FileStore`
//! over `config.json` on a Docker volume this process cannot see: the two servers shared a
//! database but not a configuration, and every setting read here was an assumption. That was
//! [D-156]. `docker-compose.yml` now sets `MM_CONFIG` to the shared Postgres DSN, which makes Go
//! select `config.DatabaseStore` (config/store.go:91) and keep the whole `model.Config` as one
//! JSON document in `Configurations.Value`. [`mm_store::ConfigStore`] reads that document and
//! [`Config::load`] layers the environment on top of it.
//!
//! **The document is the configuration Go persists, not the one it runs on.** `Store.Load` builds
//! two configs and writes back the one *without* the environment applied (store.go:321), so a
//! reader that stops at the document disagrees with the running server on precisely the settings
//! an operator bothered to change. Measured against the live stack: the row says
//! `ServiceSettings.SiteURL == ""` while the server beside it is running on
//! `MM_SERVICESETTINGS_SITEURL=http://localhost:8065`. Hence [`Config::apply_env`], and hence its
//! being applied after the document rather than as a fallback for it.
//!
//! **`FeatureFlags` is not in the document at all.** Go clears the section before persisting when
//! `readOnlyFF` is set, which is the default (store.go:306-310) — confirmed against the live row,
//! which has no `FeatureFlags` key. So [`Config::feature_flag_burn_on_read`] can only ever come
//! from the environment or from the compiled-in default, and a future flag must not be given a
//! database source it does not have.
//!
//! # Which direction each default fails
//!
//! This matters more than the defaults themselves, because the two settings fail in *opposite*
//! directions when we are wrong about them:
//!
//! - `restrict_system_admin = false` makes `SessionHasPermissionToAndNotRestrictedAdmin`
//!   behave exactly like `SessionHasPermissionTo`. Being wrong here **over-grants**: we would
//!   admit a restricted system admin that Go denies.
//! - `compliance_enable = false` takes the public-channel fallback branch in
//!   `HasPermissionToReadChannel`, which is the *permissive* one. Being wrong here
//!   **over-grants** too: we would let a non-member read a public channel that Go, with
//!   compliance on, confines to members.
//!
//! Both are `false` in Go and both over-grant if that is wrong, so neither is a safe assumption
//! to bury. Note also that `authorization.go:475` reads `ComplianceSettings.Enable` **without**
//! consulting the licence, even though every compliance *feature* is licence-gated
//! (`app/compliance.go:18`). So "Team Edition cannot enable compliance" is not a proof that this
//! branch is unreachable — the setting alone moves it.

/// Port of `model.Config` (config.go), restricted to the fields a migrated code path reads.
///
/// Deliberately not a lazily-grown mirror of the whole struct: a field appears here when
/// something ported consults it, and its doc comment names the caller. A config field with no
/// reader is a guess about the wire format that nothing can falsify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `ExperimentalSettings.RestrictSystemAdmin` (config.go:1247).
    ///
    /// When true, Go's `SessionHasPermissionToAndNotRestrictedAdmin` denies **every** caller
    /// that is not unrestricted — it does not fall through to a role check. Read by
    /// [`crate::App::session_has_permission_to_and_not_restricted_admin`].
    pub restrict_system_admin: bool,

    /// `ComplianceSettings.Enable` (config.go:2874).
    ///
    /// When true, `HasPermissionToReadChannel` stops falling back to `read_public_channel` for
    /// open channels, confining reads to members so the compliance export sees every access.
    /// Read by [`crate::App::has_permission_to_read_channel`].
    pub compliance_enable: bool,

    /// `ImageProxySettings.Enable` (config.go:3996). Go default `false`.
    ///
    /// When true, `PostWithProxyAddedToImageURLs` rewrites every markdown image destination in
    /// the message and may set `message_source`. That needs the markdown parser ([D-044]), so
    /// [`crate::post::prepare_post_for_client_with_embeds_and_images`] refuses the post and the
    /// handler forwards to Go instead.
    pub image_proxy_enable: bool,

    /// `ServiceSettings.EnablePostIconOverride` (config.go:848). Go default `false`.
    ///
    /// When true, `OverrideIconURLIfEmoji` resolves an `override_icon_emoji` prop to a static
    /// emoji URL and writes it back into the post's props. Same treatment: a post carrying that
    /// prop is refused, and forwarded, while this is on.
    pub enable_post_icon_override: bool,

    /// `ServiceSettings.EnablePostUsernameOverride` (config.go, defaulted **`false`**).
    ///
    /// The username half of the pair above, and the two are **not** applied the same way by the
    /// webhook paths: on *create* a disabled override blanks `Username` to `""`, while on
    /// *update* it restores the **old hook's** value. So an administrator who turns the setting
    /// off does not lose the usernames already configured — they simply become unchangeable.
    pub enable_post_username_override: bool,

    /// `ServiceSettings.EnableCustomEmoji` (config.go:849). Go default **`true`**.
    ///
    /// Gates `metadata.emojis` entirely — `getCustomEmojisForPost` returns an empty slice
    /// without touching the store when this is off.
    pub enable_custom_emoji: bool,

    /// `ServiceSettings.EnableBotAccountCreation` (config.go:462, defaulted **`false`** at
    /// :917).
    ///
    /// The whole of `POST /api/v4/bots` on a stock server: closed, `createBot` answers
    /// `api.bot.create_disabled` at **403** — and it does so *after* the `create_bot` permission
    /// check, so a caller without rights still gets the 403 naming the permission instead. This
    /// deployment leaves it at the default, which is why `scripts/stack.sh` writes its seeded
    /// bots straight to the tables. Read by `mm_api::bots::create_bot`.
    pub enable_bot_account_creation: bool,

    /// `ServiceSettings.PostPriority` (config.go:992). Go default **`true`**.
    ///
    /// Gates `metadata.priority` *and* `metadata.acknowledgements`. `IsPostPriorityEnabled`
    /// (app/post_priority.go:46) reads this and nothing else — there is **no licence check**,
    /// so the branch is live on Team Edition.
    pub post_priority: bool,

    /// `ServiceSettings.AllowPersistentNotifications` (config.go:467, defaulted **`true`** at
    /// :997).
    ///
    /// The other half of `IsPersistentNotificationsEnabled()`
    /// (post_persistent_notification.go:430), which is `IsPostPriorityEnabled() && this`. Both
    /// default to `true`, so **the feature is on by default** — and `SaveReactionForPost` calls
    /// `ResolvePersistentNotification` on every reaction to a root post when it is. See
    /// `App::save_reaction_for_post` for what this server does about that.
    pub allow_persistent_notifications: bool,

    /// `ServiceSettings.UniqueEmojiReactionLimitPerPost` (config.go:489).
    ///
    /// Defaulted to **50** and *clamped* to 500 at :1025 — the clamp is a `SetDefaults` step, so
    /// a document holding 900 is read back as 500 by a running server and this port applies the
    /// same ceiling. Read once per reaction, and only when the emoji is not already on the post:
    /// an existing emoji never counts against the limit.
    pub unique_emoji_reaction_limit_per_post: i64,

    /// `TeamSettings.RestrictDirectMessage` (config.go:2553, defaulted **`"any"`** at :2620).
    ///
    /// Two accepted values, `any` and `team` (config.go:4552). Only `team` does anything:
    /// `CheckIfChannelIsRestrictedDM` returns early on anything else, so a DM between two users
    /// with no team in common is refused only on a server that has been configured for it.
    pub restrict_direct_message: String,

    /// `TeamSettings.RestrictCreationToDomains` (config.go:2548, defaulted **`""`** at :2592).
    ///
    /// A free-form list of email domains, normalised by `normalizeDomains`: `@` and `,` become
    /// spaces, the whole string is lower-cased, and the result is split on whitespace. So
    /// `"@corp.example.com, example.com example.org"` is three domains.
    ///
    /// `checkValidDomains` refuses a team whose `AllowedDomains` names anything outside this list
    /// — and **only when the list is non-empty**, which the default makes it. So on a stock server
    /// the check is a no-op, and a server that sets it refuses updates Go would refuse too.
    pub restrict_creation_to_domains: String,

    /// `TeamSettings.EnableOpenServer` (config.go:2546, defaulted **`false`** at :2588).
    ///
    /// Read by exactly one route, and read **backwards** from what the name suggests:
    /// `getUsersWithInvalidEmails` answers **400** when it is *enabled*. An open server lets
    /// anyone sign up, so "which accounts have emails outside the allowed domains" has no
    /// meaning; the handler refuses rather than returning an empty list.
    pub enable_open_server: bool,

    /// `ServiceSettings.EnableUserStatuses` (config.go:445, defaulted **`true`** at :711).
    ///
    /// **`ServiceSettings`, not `TeamSettings`** — its sibling `UserStatusAwayTimeout` *is* in
    /// `TeamSettings`, and reading either from the wrong section would silently fall back to the
    /// default on every real server's document.
    ///
    /// **Every status read and write returns early when this is off** — and the writes return
    /// *silently*, with no error, so a client gets a 200 for a status that was never stored. The
    /// read `GetStatus` answers an empty `Status{}` rather than a 404, which is a different shape
    /// again.
    pub enable_user_statuses: bool,

    /// `TeamSettings.UserStatusAwayTimeout` (config.go, defaulted **`300`** — seconds).
    ///
    /// `isUserAway` is `GetMillis() - lastActivityAt >= timeout * 1000`, so the field is seconds
    /// and the comparison is milliseconds. It decides whether a *non-manual* away request is
    /// honoured at all: a user who has been active more recently than the timeout stays online.
    pub user_status_away_timeout: i64,

    /// `TeamSettings.EnableCustomUserStatuses` (config.go:2549, defaulted **`true`** at :2596).
    ///
    /// Gates all four custom-status routes, which answer **501** `api.custom_status.disabled`
    /// when it is off — a status code, not a silent success like [`Config::enable_user_statuses`]
    /// gives the plain status writes. Two adjacent features, two different shapes of "off".
    pub enable_custom_user_statuses: bool,

    /// `EmailSettings.RequireEmailVerification` (config.go:2145, defaulted **`false`** at :2193).
    ///
    /// Read by `App.UpdateUser`, where it inverts what an email change *means*: with it on, the
    /// submitted email is stashed as `newEmail` and the stored one is **kept**, so the row does
    /// not change until the user follows a verification link. A port that ignored the flag would
    /// let a client take an address it has not proved it owns.
    pub require_email_verification: bool,

    /// `GuestAccountsSettings.RestrictCreationToDomains` (config.go:3948, defaulted **`""`**).
    ///
    /// The guest-account twin of [`Config::restrict_creation_to_domains`]. `App.UpdateUser`
    /// checks **both**, and which one refuses depends on whether the *stored* user is a guest —
    /// so the two are not interchangeable and a port that read one for both would let a guest
    /// take a member-only domain.
    pub guest_restrict_creation_to_domains: String,

    /// `ServiceSettings.AllowSyncedDrafts` (config.go:488). Go default **`true`**.
    ///
    /// Gates the whole drafts feature. Every one of `getDrafts`, `upsertDraft` and `deleteDraft`
    /// checks it *first* and answers **501** `api.drafts.disabled.app_error` when it is off —
    /// before any permission check, so a caller with no rights at all still gets the 501 rather
    /// than a 403.
    pub allow_synced_drafts: bool,

    /// `ServiceSettings.EnableAPIChannelDeletion` (config.go:476). Go default **`false`**.
    ///
    /// Gates `DELETE /api/v4/channels/{id}?permanent=true`. When it is off the request is refused
    /// with **401** — not 403 — and the id depends on who asked: a system admin gets
    /// `api.user.delete_channel.not_enabled.for_admin.app_error`, everybody else
    /// `api.user.delete_channel.not_enabled.app_error`. See
    /// [`crate::channel_write`]'s delete path.
    pub enable_api_channel_deletion: bool,

    /// `ServiceSettings.EnableAPITeamDeletion` (config.go:452). Go default **`false`**
    /// (config.go:885).
    ///
    /// The team twin of the setting above, with the same 401-not-403 refusal and the same
    /// admin/non-admin id split — `api.user.delete_team.not_enabled.for_admin.app_error` versus
    /// `api.user.delete_team.not_enabled.app_error`. Read by `mm_api::teams::delete_team`.
    pub enable_api_team_deletion: bool,

    /// `TeamSettings.EnableChannelCategorySorting` (config.go:2558). Go default **`true`**.
    ///
    /// Read only as the second half of `addChannelToDefaultCategory`'s gate
    /// (app/channel.go:4708), which `PatchChannel` calls after the channel is written. The first
    /// half is `channel.DefaultCategoryName != ""`, so on a stock server a patch that leaves that
    /// field empty never reaches the sidebar at all — which is what makes the common patch
    /// serviceable here while a `default_category_name` patch is forwarded.
    pub enable_channel_category_sorting: bool,

    /// `TeamSettings.MaxChannelsPerTeam` (config.go:2557), Go default **2000**
    /// (config.go:2629).
    ///
    /// Read twice on the create path and with two *different* predicates, which is why the port
    /// keeps both: `CreateChannelWithUser` (app/channel.go:180) compares it against
    /// `GetNumberOfChannelsOnTeam`, which counts `O`, `P` and `G` including archived ones, while
    /// `saveChannelT` (channel_store.go:813) compares it against a count of live `O` and `P`
    /// only. A team can therefore be refused by the first check and accepted by the second.
    ///
    /// A **negative** value switches the store's check off entirely (`maxChannelsPerTeam >= 0`);
    /// the app-layer check has no such escape and would still refuse.
    pub max_channels_per_team: i64,
    /// `TeamSettings.MaxUsersPerTeam` (config.go:2543). Go default **`50`**
    /// (`TeamSettingsDefaultMaxUsersPerTeam`, config.go:144).
    ///
    /// Passed straight into `TeamStore::save_member`, which refuses the write with
    /// `ErrLimitExceeded` when `existing + new > max`. Two things a reader gets wrong:
    /// the count excludes **deleted memberships and deleted users** (both `DeleteAt = 0`
    /// predicates), and the guard on the whole check is `maxUsersPerTeam >= 0` — so a document
    /// holding `0` caps a team at zero members rather than disabling the limit.
    pub max_users_per_team: i64,

    /// `TeamSettings.ExperimentalDefaultChannels` (config.go:2568). Go default **`[]`**.
    ///
    /// When empty, `DefaultChannelNames` is `["town-square", "off-topic"]`; when set, the list
    /// **replaces `off-topic` only** — `town-square` is always first and is de-duplicated out of
    /// the configured list if it names itself. Read on the team-join path to decide which
    /// channels a new member is put in.
    pub experimental_default_channels: Vec<String>,

    /// `ServiceSettings.EnableBurnOnRead` (config.go:472). Go default **`true`**.
    pub enable_burn_on_read: bool,

    /// `ServiceSettings.ExperimentalEnableDefaultChannelLeaveJoinMessages` (config.go:450). Go
    /// default **`true`** (config.go:873), which is the trap: the word "Experimental" reads like
    /// an opt-in and it is on out of the box.
    ///
    /// Read by [`crate::App::leave_team`], where it gates a `town-square` lookup **whose failure
    /// fails the whole removal** as well as the system post itself. So turning it off does not
    /// merely silence a message; it removes a 404 branch from `DELETE /teams/{id}/members/{id}`.
    pub experimental_enable_default_channel_leave_join_messages: bool,

    /// `ServiceSettings.PostEditTimeLimit` (config.go:437). Go default **`-1`**, which means
    /// "no limit" and is checked for explicitly rather than compared.
    ///
    /// Read by [`crate::App::post_edit_time_limit_expired`], which every write that changes a
    /// stored post consults: `updatePost`, `patchPost` and both pin routes. Seconds, not
    /// milliseconds — `post.CreateAt + int64(limit)*1000` is the deadline, so a limit of `0` is
    /// not "no limit" but "expired the moment the post was created".
    pub post_edit_time_limit: i64,

    /// `ServiceSettings.ExperimentalEnableHardenedMode` (config.go:458). Go default **`false`**.
    ///
    /// When on, a non-integration session may not set the props reserved for integrations
    /// (`Post::contains_integrations_reserved_props`) — a **400**
    /// `api.context.invalid_body_param.app_error` naming `props`. Off, the check is a no-op, which
    /// is why being wrong about the default would be silent: every post would be accepted.
    pub experimental_enable_hardened_mode: bool,

    /// `FeatureFlags.BurnOnRead` (feature_flags.go:90). Go default **`true`**.
    ///
    /// Kept apart from the setting above because `isBurnOnReadEnabled` (app/post_helpers.go:270)
    /// ands the two, and either one alone turns the feature off. Folding them into a single
    /// field here would make a deployment that disables only the flag indistinguishable from one
    /// that disables only the setting — the same value, reached two ways, is exactly the sort of
    /// coincidence that hides a wrong read.
    pub feature_flag_burn_on_read: bool,

    /// `FileSettings.DriverName` (config.go:1814). Go default **`"local"`**
    /// (`model.ImageDriverLocal`, config.go:1900).
    ///
    /// Read by [`crate::App::get_emoji`] and [`crate::App::get_emoji_by_name`], which refuse
    /// with `api.emoji.storage.app_error` (403) when it is the **empty string** — not when it
    /// is some driver we do not implement. A `String` rather than a `bool` because the value is
    /// what Go compares, and because `FileSettings.isValid` (config.go:4645) restricts it to
    /// `local`/`amazons3`/`azure`: an empty driver only ever arrives through a config that
    /// would fail Go's own validation, which is why the branch it gates is close to
    /// unreachable and is ported for fidelity rather than for coverage.
    pub file_driver_name: String,

    /// `FileSettings.Directory` (config.go:1815). Go default **`"./data/"`**
    /// (`FileSettingsDefaultDirectory`, config.go:155), and `SetDefaults` replaces an **empty**
    /// value with it too — so a document holding `""` still reads `./data/`.
    ///
    /// The root of every path [`crate::filestore::LocalFileBackend`] resolves. Relative, and
    /// relative to the **process working directory** — which is the Go server's, not
    /// necessarily ours. A deployment where the two processes start in different directories has
    /// two different file backends and no error to say so; see [D-201].
    pub file_directory: String,

    /// `FileSettings.MaxFileSize` (config.go:1811). Go default **`100 * 1024 * 1024`**
    /// (config.go:1888) — 100 MiB, not 100 MB.
    ///
    /// Read by `setProfileImage` and `uploadBrandImage` as `r.ContentLength > MaxFileSize`, and
    /// again by `web.Handler.ServeHTTP` (web/handlers.go:220) as the `MaxBytesReader` cap on
    /// every `handlerParamFileAPI` route — where the cap is `MaxFileSize + bytes.MinRead`, 512
    /// bytes *more*. The two are not the same number and the difference is observable: a body
    /// between the two lengths with no `Content-Length` header passes the first check and fails
    /// the read.
    pub file_max_file_size: i64,

    /// `LdapSettings.PictureAttribute` (config.go:2712, defaulted **`""`** at :2831).
    ///
    /// One of the two halves of `setProfileImage`'s 409: an LDAP user — or a SAML user on a
    /// server syncing with LDAP — may not replace a picture that LDAP owns, and *this being
    /// non-empty* is what makes LDAP own it. Empty on a stock server, so the 409 is unreachable
    /// until an administrator names an attribute.
    pub ldap_picture_attribute: String,

    /// `SamlSettings.EnableSyncWithLdap` (config.go:2992, defaulted **`false`** at :3037).
    ///
    /// The other half: it is what brings a SAML user under
    /// [`Config::ldap_picture_attribute`]'s rule. An LDAP user is covered whatever this says.
    pub saml_enable_sync_with_ldap: bool,

    /// `TeamSettings.LockProfileFieldsForEmailUsers` (config.go:2566, defaulted
    /// **`"none"`** at :2668).
    ///
    /// `IsProfileImageLockedForUser` (app/user.go:1465) compares it against `"all"` and nothing
    /// else, so `"name_and_username"` — the third legal value — does **not** lock the picture.
    /// See [`crate::App::is_profile_image_locked_for_user`] for the other three conjuncts.
    pub lock_profile_fields_for_email_users: String,

    /// `FileSettings.PublicLinkSalt` (config.go:1832). Go's default is
    /// **`NewRandomString(32)`** — generated, not constant.
    ///
    /// So there is no default worth writing: the empty string here means "the document did not
    /// say", and `getPublicFile` forwards rather than validating a hash against a salt it had to
    /// invent. Every server that has ever persisted a configuration has a real value here.
    pub public_link_salt: String,

    /// `FileSettings.DedicatedExportStore` (config.go:1845, defaulted **`false`** at :2028).
    ///
    /// False — the stock value — means the export backend **is** the file backend, the same
    /// object, not a second one configured the same way (platform/service.go:397). True switches
    /// exports to [`Config::file_export_driver_name`] and
    /// [`Config::file_export_directory`], and is also the second gate in
    /// `GeneratePresignURLForExport`.
    pub dedicated_export_store: bool,

    /// `FileSettings.ExportDriverName` (config.go:1846, defaulted **`"local"`** at :2032).
    /// Read only when [`Config::dedicated_export_store`] is set.
    pub file_export_driver_name: String,

    /// `FileSettings.ExportDirectory` (config.go:1847, defaulted **`"./data/"`** at :2036 — the
    /// *file* settings default, not the export one). Read only when
    /// [`Config::dedicated_export_store`] is set.
    pub file_export_directory: String,

    /// `ExportSettings.Directory` (config.go:4047, defaulted **`"./export"`** at :4067).
    ///
    /// **Not** [`Config::file_export_directory`], and the two are easy to confuse: this is the
    /// path *within* the export backend that `listExports`, `downloadExport` and `deleteExport`
    /// operate on, while that one is the backend's own root. On a stock server the export
    /// backend's root is `./data/` and this is `./export`, so an export lives at
    /// `./data/export/<name>`.
    pub export_directory: String,

    /// `ImportSettings.Directory` (config.go:4030, defaulted **`"./import"`** at :4036).
    ///
    /// The counterpart of [`Config::export_directory`] for `listImports` and `deleteImport`,
    /// against the **file** backend rather than the export one — imports have no dedicated store.
    pub import_directory: String,

    /// `ServiceSettings.WebserverMode` (config.go:432, defaulted **`"gzip"`** at :843).
    ///
    /// Read by `web.WriteFileResponse` and by nothing else: in `gzip` mode the pre-computed
    /// content length is written as `X-Uncompressed-Content-Length` instead of `Content-Length`.
    /// **`"regular"` is rewritten to `"gzip"` on load** (config.go:845) — a mutation of the
    /// caller's config rather than a read-time fold — so a document saying `regular` behaves as
    /// `gzip` and this field never holds that value.
    pub webserver_mode: String,

    /// `ServiceSettings.EnableIncomingWebhooks` (config.go:388, defaulted at :607). Go default
    /// **`true`**.
    ///
    /// Gates all three of `GetIncomingWebhooksForTeamPageByUser`,
    /// `GetIncomingWebhooksPageByUser` and `GetIncomingWebhooksCount` (app/webhook.go:646, :659,
    /// :676), each answering **501** `api.incoming_webhook.disabled.app_error` — checked *after*
    /// the handler's permission gate, so a caller with no rights gets the 403 and only an
    /// authorised one ever sees the 501. Read by [`crate::App::get_incoming_webhooks_count`] and
    /// its two neighbours.
    pub enable_incoming_webhooks: bool,

    /// `ServiceSettings.EnableOutgoingWebhooks` (config.go:389, defaulted at :611). Go default
    /// **`true`**.
    ///
    /// The outgoing counterpart of [`Config::enable_incoming_webhooks`], gating the three
    /// functions behind `getOutgoingHooks` with **its own** error id,
    /// `api.outgoing_webhook.disabled.app_error`. Two settings, two ids, one status.
    pub enable_outgoing_webhooks: bool,

    /// `ServiceSettings.EnableOAuthServiceProvider` (config.go, defaulted at :595). Go default
    /// **`true`**.
    ///
    /// Gates all three OAuth **app** reads with `api.oauth.allow_oauth.turn_off.app_error` at
    /// **501** (app/oauth.go:75, :138, :151) — a third feature toggle with a third error id at the
    /// same status. Read by [`crate::App::get_oauth_apps`] and its two neighbours.
    pub enable_oauth_service_provider: bool,

    /// `ServiceSettings.EnableOutgoingOAuthConnections` (config.go:390, defaulted **`false`** at
    /// :615).
    ///
    /// The **first** of the two gates in `ensureOutgoingOAuthConnectionInterface`
    /// (api4/outgoing_oauth_connection.go:60), and on a stock server the one that fires: closed,
    /// it answers `api.context.outgoing_oauth_connection.not_available.configuration_disabled` at
    /// 501. *Open*, the next line asks for an Enterprise licence and answers
    /// `api.license.upgrade_needed.app_error` — same status, different id — so the setting selects
    /// which refusal a Team Edition server gives, never whether it refuses. Read by
    /// `mm_api::outgoing_oauth`.
    pub enable_outgoing_oauth_connections: bool,

    /// `ServiceSettings.EnableCommands` (config.go:391, defaulted **`true`** at :803).
    ///
    /// The first statement of `App.GetCommand` and `App.ListTeamCommandsByUser`, and closed it is
    /// `api.command.disabled.app_error` at **501** — which `getCommand` then *discards*:
    /// its handler answers `SetCommandNotFoundError` for **any** error from `GetCommand`
    /// (api4/command.go:329), so a disabled installation 404s rather than 501s on that route while
    /// `listCommands` shows the 501. One setting, two visible answers. Read by
    /// `mm_api::commands`.
    pub enable_commands: bool,

    /// `FileSettings.EnablePublicLink` (config.go, defaulted **`false`**).
    ///
    /// The whole of `getFileLink` and `getPublicFile` on a stock server: closed, both answer
    /// `api.file.get_public_link.disabled.app_error` at **403** — and `getPublicFile` does so to an
    /// **unauthenticated** caller, since it is an `APIHandler`. Checked *after* `RequireFileId`, so
    /// a malformed id is still a 400. Read by `mm_api::file_links`.
    pub enable_public_link: bool,

    /// `CloudSettings.PreviewModalBucketURL` (config.go:3562, defaulted **`""`** at :3593).
    ///
    /// `App.GetPreviewModalData` answers `app.cloud.preview_modal_bucket_url_not_configured` at
    /// **404** when it is empty, and otherwise fetches JSON over HTTP from that bucket. Empty is
    /// the stock value, so the 404 is the whole route here. Modelled as a `String` rather than an
    /// `Option` because Go's own test is `nil || ""` — the two are the same answer.
    pub cloud_preview_modal_bucket_url: String,

    /// `ServiceSettings.MaximumPersonalAccessTokenLifetimeDays` (config.go:409, defaulted **`0`**
    /// at :583).
    ///
    /// **Zero disables the whole policy**, and that is not the same as "no maximum in days":
    /// `App.maxUserAccessTokenExpiry` returns `(0, false)` for anything `<= 0`, and its caller
    /// returns `0` *without querying the database at all* — so on a stock server
    /// `GET /api/v4/users/tokens/non_compliant/count` answers `{"count":0}` having read nothing.
    /// Read by [`crate::App::max_user_access_token_expiry`].
    ///
    /// `i64` rather than Go's `int` because the value is multiplied into milliseconds
    /// (`maxDays*24*60*60*1000`) before it is used, and that product is an `int64` in Go too.
    pub maximum_personal_access_token_lifetime_days: i64,

    /// `ServiceSettings.EnableUserAccessTokens` (config.go:408, defaulted **`false`** at :579).
    ///
    /// Gates `CreateUserAccessToken` and `RotateUserAccessToken` with a **501**
    /// (`app.user_access_token.disabled`), not a 403 — and the check is
    /// `!enabled && !user.IsBot`, so **bot accounts mint tokens whatever this says**. That
    /// exemption is the only reason the two write routes are reachable on a stock installation,
    /// and it is what their parity suite exercises. Read by
    /// [`crate::App::create_user_access_token`] and [`crate::App::rotate_user_access_token`].
    pub enable_user_access_tokens: bool,

    /// `MessageExportSettings.DownloadExportResults` (config.go:3880, defaulted **`false`** at
    /// :3893).
    ///
    /// The whole content of `downloadJob` (api4/job.go:59) on a stock server: closed, the route is
    /// `app.job.download_export_results_not_enabled` at **501**, checked after `RequireJobId` and
    /// before the job is fetched — so a nonexistent id is the 501 and not a 404. Read by
    /// `mm_api::jobs::download_job`.
    pub message_export_download_export_results: bool,

    /// `FeatureFlags.SessionAttributes` (feature_flags.go:116, defaulted **`false`** at :204).
    ///
    /// Gates `GET /api/v4/users/sessions/attributes/manifest` through
    /// `App.sessionAttributesEnabled` (app/session_attributes.go:24), which is this flag **and**
    /// an Enterprise Advanced licence. Closed, the route is
    /// `api.user.session_attributes.disabled.app_error` at 501.
    ///
    /// Like [`Config::feature_flag_burn_on_read`], it can only come from the environment or the
    /// compiled-in default — `FeatureFlags` is cleared before the document is persisted, so there
    /// is no database source to read and giving it one would be inventing a value.
    pub feature_flag_session_attributes: bool,

    /// The four other flags of the five-way `if` that registers `api4/properties.go`
    /// (properties.go:23), alongside [`Config::feature_flag_session_attributes`]:
    /// `IntegratedBoards`, `ManagedChannelCategories`, `ClassificationMarkings` and
    /// `PostAttributes`.
    ///
    /// **`ClassificationMarkings` defaults to `true`** (feature_flags.go:185) and the other three
    /// to `false`, so on a stock server the nine property routes *are* registered and the `if` is
    /// satisfied by that one flag alone. A port that assumed the whole family was dark — which
    /// every other flag in this block would suggest — would forward nine live routes.
    ///
    /// All four are environment-or-default like [`Config::feature_flag_burn_on_read`].
    pub feature_flag_integrated_boards: bool,
    /// See [`Config::feature_flag_integrated_boards`].
    pub feature_flag_managed_channel_categories: bool,
    /// See [`Config::feature_flag_integrated_boards`]. **Defaults to `true`.**
    pub feature_flag_classification_markings: bool,
    /// See [`Config::feature_flag_integrated_boards`].
    pub feature_flag_post_attributes: bool,

    /// `FeatureFlags.DiscoverableChannels` (feature_flags.go:208, defaulted **`false`** at :208).
    ///
    /// The registration `if` of `initChannelJoinRequestRoutes` (api4/channel_join_request.go:18):
    /// with it off, gorilla/mux has never heard of any of the seven join-request routes and
    /// answers `api.context.404.app_error`. It also turns on `serveDiscoverableNonMember` in
    /// `getChannel` and the `discoverable` arms of `createChannel`/`patchChannel`, which are
    /// [D-153]'s pin — so a deployment that sets it needs those three ported too.
    ///
    /// Environment-or-default like [`Config::feature_flag_burn_on_read`]: `FeatureFlags` never
    /// reaches the persisted document, which is exactly what [D-153] records.
    pub feature_flag_discoverable_channels: bool,

    /// `ServiceSettings.CollapsedThreads` (config.go:485, defaulted **`"always_on"`** at :982).
    ///
    /// **The default short-circuits the preference lookup entirely.**
    /// `App.IsCRTEnabledForUser` (app/channel.go:3183) reads the user's
    /// `display_settings/collapsed_reply_threads` preference only for `default_on` and
    /// `default_off`; `disabled` is always false and `always_on` — the shipped default — is
    /// always true, with no query. Treating this as a plain "CRT on/off" boolean would send a
    /// per-user query the server never makes.
    pub collapsed_threads: String,

    /// `ServiceSettings.ThreadAutoFollow` (config.go:484, defaulted **`true`** at :978).
    ///
    /// In `MarkChannelsAsViewed` this gates the *thread* half of marking a channel read:
    /// `ThreadAutoFollow && (!collapsedThreadsSupported || !isCRTEnabled)`. With the shipped
    /// defaults `isCRTEnabled` is true, so the whole expression reduces to
    /// `!collapsedThreadsSupported` — a client that says it renders threads itself gets no
    /// thread write, and a client that does not gets one.
    pub thread_auto_follow: bool,

    /// `ServiceSettings.EnableChannelViewedMessages` (config.go:444, defaulted **`true`** at
    /// :707).
    ///
    /// Gates only the `multiple_channels_viewed` websocket event, never the write — so with it
    /// off a channel is still marked read and the client is simply not told. Nothing on the HTTP
    /// response body changes either way.
    pub enable_channel_viewed_messages: bool,

    /// `FeatureFlags.EnableShiftEscapeToMarkAllRead` (feature_flags.go:77, defaulted **`false`**
    /// at :181).
    ///
    /// Gates `PUT /channels/members/{user_id}/direct/read` and
    /// `PUT /users/{user_id}/teams/{team_id}/read`, both of which answer **501**
    /// `api.mark_all_as_read.disabled.app_error` when it is off — and the check is the *first*
    /// line of each handler, ahead of `RequireUserId`, so a malformed id gets the 501 too.
    ///
    /// Environment-or-default only, like [`Config::feature_flag_burn_on_read`].
    pub feature_flag_enable_shift_escape_to_mark_all_read: bool,

    /// `ServiceSettings.EnableDynamicClientRegistration` (config.go:386, defaulted **`false`** at
    /// :599).
    ///
    /// The second of the two gates in front of `registerOAuthClient`, and the one that is closed
    /// on a stock server — so the whole Dynamic Client Registration route is a `400` carrying a
    /// **DCR error envelope**, not an `AppError`. Go's own check is
    /// `cfg == nil || !*cfg`, so an absent setting is `false` here as everywhere.
    pub enable_dynamic_client_registration: bool,

    /// `PrivacySettings.ShowFullName` (config.go, defaulted `true`).
    ///
    /// Read by `getUser` and every route that sanitizes another user, and folded into
    /// `User.Etag`. Hardcoded in `mm-api`'s `AppState` until this module had a source of truth —
    /// see [D-085], which this closes: an admin turning it off now moves both servers together.
    pub show_full_name: bool,

    /// `PrivacySettings.ShowEmailAddress` (config.go, defaulted `true`).
    ///
    /// The companion of [`Config::show_full_name`], with the same readers and the same history.
    pub show_email_address: bool,

    /// `ServiceSettings.SiteURL` (config.go, defaulted to `""` by `Store.Load` at store.go:280).
    ///
    /// Two readers, and they want different things from it:
    ///
    /// - [`Config::from_document`] reads its **presence** as Go's `isUpdate` discriminator
    ///   (config.go:4289), which decides two defaults.
    /// - [`Config::subpath`] reads its **value**, to find the path a session cookie is scoped to.
    ///
    /// So an absent key and an empty string are genuinely different inputs to the first reader and
    /// identical to the second. Modelled as `Option<String>` for exactly that reason: collapsing
    /// it to `String` would silently make every document look like a fresh install.
    pub site_url: Option<String>,

    /// `ServiceSettings.SessionIdleTimeoutInMinutes` (config.go:429, defaulted at :799). Go
    /// default **43200** — thirty days, not zero.
    ///
    /// Read by [`crate::App::get_session`]: a positive value arms the idle-timeout check that
    /// revokes a session whose `LastActivityAt` is older than it. Zero disarms it entirely, which
    /// is why the default's *value* matters here in a way the boolean settings' does not — a port
    /// that guessed `0` would never revoke anything and would accept every session Go rejects.
    pub session_idle_timeout_in_minutes: i64,

    /// `ServiceSettings.SessionLengthMobileInHours` (config.go:423, defaulted at :767).
    ///
    /// **Its default is a two-step cascade, not a constant.** Go fills
    /// `SessionLengthMobileInDays` first — `180` on an update, `30` on a fresh config — and then
    /// derives hours as `days * 24`. So the reachable defaults are **4320** and **720**, and
    /// which one applies turns on the same `isUpdate` discriminator as
    /// [`Config::extend_session_length_with_activity`]. Reading the hours field alone and
    /// assuming a single constant is the mistake this comment exists to prevent.
    ///
    /// Read only by `attachDeviceIds` (api4/user.go:2787), where it sets both the session's new
    /// `ExpiresAt` and the `Max-Age` of the session cookie — so a client that registers a device
    /// id has its session length **replaced**, not extended.
    pub session_length_mobile_in_hours: i64,

    /// `ServiceSettings.AllowCookiesForSubdomains` (config.go:414, defaulted **`false`** at
    /// :839).
    ///
    /// The whole of `App.GetCookieDomain` (app/config.go:191): when off — the default — the
    /// cookie carries **no `Domain` attribute at all**, which scopes it to the exact host that
    /// served it. See [`Config::cookie_domain`].
    pub allow_cookies_for_subdomains: bool,

    /// `ServiceSettings.MaximumLoginAttempts` (config.go:383, defaulted **`10`** at :671).
    ///
    /// The lockout cap, and the predicate is strictly `<` in
    /// [`mm_store::UserStore::try_increment_failed_password_attempts`] — so this is the number of
    /// failures *allowed*, and the `maxAttempts`-th one is refused. Read by every password check,
    /// including `DoubleCheckPassword` behind `PUT /users/{id}/password`: an already-logged-in
    /// user who keeps mistyping their current password locks themselves out of *login* too,
    /// because both paths share one counter column.
    ///
    /// `LdapSettings.MaximumLoginAttempts` is a **different** setting with its own default of 10;
    /// only the LDAP path reads it, and that path is forwarded.
    pub maximum_login_attempts: i64,

    /// `ServiceSettings.TerminateSessionsOnPasswordChange` (config.go:416, defaulted
    /// **`!isUpdate`** at :733).
    ///
    /// Computed, not constant — like [`Config::extend_session_length_with_activity`]. Every
    /// document a running server persists carries a `SiteURL`, so `isUpdate` is true there and
    /// this is **off** on any upgraded install and **on** on a fresh one.
    ///
    /// When on, `App.UpdatePassword` revokes every session of the user except the caller's own.
    pub terminate_sessions_on_password_change: bool,

    /// `PasswordSettings.MinimumLength` (config.go:1778). Go default **`8`** — or
    /// `PasswordFIPSMinimumLength` under a FIPS build, which this port does not model.
    ///
    /// Counted in **bytes** (`len(password)`), not runes, so a four-character emoji password is
    /// sixteen and passes a minimum of eight.
    pub password_minimum_length: i64,

    /// `PasswordSettings.Lowercase` (config.go:1786). Go default **`false`**.
    pub password_lowercase: bool,

    /// `PasswordSettings.Number` (config.go:1790). Go default **`false`**.
    pub password_number: bool,

    /// `PasswordSettings.Uppercase` (config.go:1794). Go default **`false`**.
    pub password_uppercase: bool,

    /// `PasswordSettings.Symbol` (config.go:1798). Go default **`false`**. The symbol set is
    /// [`mm_model::utils::SYMBOLS`], which **includes a space**.
    pub password_symbol: bool,

    /// `ServiceSettings.ExtendSessionLengthWithActivity` (config.go:415, defaulted at :728).
    ///
    /// **Its default is not a constant.** Go writes `new(!isUpdate)`, and `isUpdate` is
    /// `ServiceSettings.SiteURL != nil` (config.go:4289) — so a fresh config defaults it `true`
    /// and a pre-existing one `false`. See [`Config::from_document`], which is where that
    /// distinction is actually made; [`Config::default`] models the fresh case.
    ///
    /// Read by [`crate::App::get_session`], where it **disarms** the idle-timeout check: Go
    /// treats sliding expiry and idle revocation as alternatives, never both.
    pub extend_session_length_with_activity: bool,

    /// `ServiceSettings.EnableTesting` (config.go, defaulted **`false`** at :543).
    ///
    /// Gates the three `/system/e2e/ai_bridge` routes, which answer
    /// `api.ai_bridge_test_helper.disabled.app_error` at 501 when it is off. It also decides
    /// whether Go registers `/manualtest` at all (api.go:415) — a *route*, not a handler branch,
    /// so that one is a difference in the router rather than in an answer and is left to the
    /// proxy.
    pub enable_testing: bool,

    /// `ServiceSettings.ScheduledPosts` (config.go, defaulted **`true`** at :1057).
    ///
    /// The first half of `requireScheduledPostsEnabled`, whose second half is a licence check. The
    /// two arms have **different error ids at the same status** — `api.scheduled_posts.feature_disabled`
    /// and `api.scheduled_posts.license_error`, both 400 — so which one this server produces
    /// depends on a setting an operator can change, and the default (`true`) means the licence arm
    /// is the reachable one.
    pub scheduled_posts: bool,

    /// `FeatureFlags.EnableAIRecaps` (feature_flags.go:96, defaulted **`false`** at :192).
    ///
    /// Half of `Config.AIRecapsEnabled()` (ai_recap_settings.go:143), which gates all fifteen
    /// `/recaps` and `/scheduled_recaps` routes. Like [`Config::feature_flag_burn_on_read`] it is
    /// deliberately **not** read from the persisted document: Go strips `FeatureFlags` before
    /// writing (config/store.go:306), so the environment is its only source.
    ///
    /// The Go comment beside it reads `FEATURE_FLAG_REMOVAL: EnableAIRecaps — Remove this when GA
    /// is released`, so this field has a shelf life; when it goes, the gate becomes the setting
    /// below alone.
    pub feature_flag_enable_ai_recaps: bool,

    /// `AIRecapSettings.Enable` (ai_recap_settings.go:88).
    ///
    /// The other half of `AIRecapsEnabled()`, and it is `Option<bool>` for a reason that changes
    /// the answer: `AIRecapSettings.IsEnabled()` is
    /// `s == nil || s.Enable == nil || *s.Enable` — so an **absent** setting means *enabled*, not
    /// disabled. Collapsing this to `bool` with `unwrap_or(false)` would disable recaps on every
    /// server that has never configured them, which is every server.
    pub ai_recap_settings_enable: Option<bool>,

    /// `ClientRequirements.AndroidLatestVersion` and its three siblings (config.go:2674-2677).
    ///
    /// The four version strings `GET /api/v4/system/ping` echoes back verbatim. They have **no**
    /// `SetDefaults` — `ClientRequirements` is one of the few sections Go never defaults — so the
    /// zero value is the empty string and that is what a stock server puts on the wire.
    pub android_latest_version: String,
    /// See [`Config::android_latest_version`].
    pub android_min_version: String,
    /// See [`Config::android_latest_version`].
    pub ios_latest_version: String,
    /// See [`Config::android_latest_version`].
    pub ios_min_version: String,

    /// `ServiceSettings.GoroutineHealthThreshold` (config.go:384, defaulted at :587). Go default
    /// **-1**, not 0.
    ///
    /// `getSystemPing` downgrades `status` to `UNHEALTHY` when the live goroutine count reaches
    /// it. The count is the *Go process's*, which this process cannot observe and whose own
    /// number would mean nothing — so a positive threshold is a boundary, not a setting to read:
    /// see `mm_api::system::get_system_ping`. Modelled so the boundary can be evaluated rather
    /// than assumed, and the default's sign is the whole reason it is `i64` and not `u32`.
    pub goroutine_health_threshold: i64,

    /// `SqlSettings.DisableDatabaseSearch` (config.go, defaulted at :1592). Go default `false`.
    ///
    /// Half of `ActiveSearchBackend`: with no search engine registered, the broker answers
    /// `"none"` when this is set and `"database"` when it is not
    /// (searchengine/searchengine.go:47). Read by `getSystemPing`.
    pub disable_database_search: bool,

    /// `ElasticsearchSettings.EnableSearching`.
    ///
    /// **A boundary, not a behaviour.** The search broker only ever holds an Elasticsearch engine
    /// if the *enterprise* build registered one (`platform/service.go:548`), so on the Team
    /// Edition binary beside us `ActiveSearchBackend` cannot be anything but `"database"` or
    /// `"none"`. This is read so that a deployment which does have the enterprise binary hands
    /// `/system/ping` back to Go rather than confidently reporting the wrong backend.
    pub elasticsearch_enable_searching: bool,

    /// `FeatureFlags.TestFeature` (feature_flags.go:14, defaulted `"off"` at :160).
    ///
    /// `getSystemPing` adds a `TestFeatureFlag` key **only** when this is not `"off"`, so it is a
    /// key that appears and disappears rather than a value that changes. Like
    /// [`Config::feature_flag_burn_on_read`] it is deliberately not read from the persisted
    /// document — Go strips `FeatureFlags` before writing (config/store.go:306) — so the
    /// environment is its only source.
    pub feature_flag_test_feature: String,

    /// The `MM_LICENSE` environment variable (`platform.LicenseEnv`, platform/license.go:26).
    ///
    /// Not an `MM_<SECTION>_<SETTING>` config overlay — it is its own variable, holding a whole
    /// signed licence rather than a setting, and Go reads it **before** the database
    /// (`LoadLicense`, platform/license.go:52). Kept here as the raw string because the only
    /// question anything ported asks of it is whether it is empty: validating a licence needs the
    /// signing key, which is not ported. Read by [`crate::App::license_state`].
    pub license: String,
}

impl Config {
    /// Port of `Config.AIRecapsEnabled` (model/ai_recap_settings.go:143).
    ///
    /// `o.FeatureFlags.EnableAIRecaps && o.AIRecapSettings.IsEnabled()`, and the second half is
    /// `s == nil || s.Enable == nil || *s.Enable` — **absent means enabled**. So the whole gate is
    /// off by default only because the feature flag is, and turning the flag on enables recaps on
    /// every server that has not explicitly disabled them.
    ///
    /// Fifteen routes are gated on this. It is a *configuration* gate, not a licence one: an
    /// operator can turn it on, at which point this server must stop answering and forward.
    pub fn ai_recaps_enabled(&self) -> bool {
        self.feature_flag_enable_ai_recaps && self.ai_recap_settings_enable.unwrap_or(true)
    }

    /// Port of `utils.GetSubpathFromConfig` (channels/utils/subpath.go:242).
    ///
    /// The path a session cookie is scoped to. Go throws the error away at the one call site this
    /// server reproduces — `RemoveSessionCookie` writes `subpath, _ :=` (context.go:181) — so a
    /// SiteURL that will not parse yields the **empty string**, and `net/http` then omits `Path`
    /// from the `Set-Cookie` header entirely rather than sending `Path=/`. That distinction is the
    /// reason this returns a plain `String` with `""` for failure instead of a `Result`: there is
    /// no caller that could do anything else with the error, and typing it would invite one to.
    ///
    /// Three inputs reach `"/"` by three different routes — an absent SiteURL, one whose parsed
    /// path is empty (`http://host`), and one whose path cleans to nothing (`http://host/..`) —
    /// and `fixtures/behaviour_subpath.json` records all of them separately, so a port that
    /// collapsed the branches would still be caught when any one of them moved.
    ///
    /// `path.Clean` runs on the **decoded** path, so `https://host/mattermost%2Fx` is a subpath
    /// two segments deep. Go's, not ours.
    pub fn subpath(&self) -> String {
        let Some(site_url) = self.site_url.as_deref() else {
            return "/".to_owned();
        };

        let Ok(url) = mm_model::go_url::go_parse(site_url) else {
            // Go: `return "", errors.Wrap(err, ...)`, and the caller drops the error.
            return String::new();
        };

        if url.path.is_empty() {
            return "/".to_owned();
        }

        // `u.Path` is `[]byte` here because Go's decoded path can hold bytes no `str` can — see
        // [`mm_model::go_url::GoUrl`]. A cookie `Path` is a header value, so anything non-UTF-8
        // could not be sent anyway; the lossy conversion keeps the failure inside this function
        // instead of at the header builder.
        mm_model::go_path::clean(&String::from_utf8_lossy(&url.path))
    }

    /// Port of `app.App.GetCookieDomain` (app/config.go:191).
    ///
    /// Returns the **hostname** of `SiteURL` when `AllowCookiesForSubdomains` is on, and `""`
    /// otherwise — including when `SiteURL` will not parse, because Go's `if ... err == nil`
    /// falls through to the same empty return. An empty string means the caller must omit the
    /// `Domain` attribute entirely rather than send `Domain=`; see
    /// [`crate::App::attach_device_ids_cookie`].
    ///
    /// `url.Hostname()` strips the port **and** the brackets around an IPv6 literal, which is why
    /// this reaches for [`mm_model::go_url`]'s host splitting rather than taking the authority
    /// verbatim.
    pub fn cookie_domain(&self) -> String {
        if !self.allow_cookies_for_subdomains {
            return String::new();
        }

        let Some(site_url) = self.site_url.as_deref() else {
            // Go dereferences a `*string` here, so a nil `SiteURL` would panic rather than
            // return — unreachable in practice because `SetDefaults` plants `""`, which parses
            // to an empty hostname. Empty is that same answer.
            return String::new();
        };

        let Ok(url) = mm_model::go_url::go_parse(site_url) else {
            return String::new();
        };

        go_hostname(&url.host)
    }

    /// Port of `app.App.isBurnOnReadEnabled` (post_helpers.go:270).
    ///
    /// **Both halves default to true**, so on a stock server this is on — which is why
    /// `getCursorPostId` reaches the read-receipt-aware cursor query rather than the plain one.
    pub fn burn_on_read(&self) -> bool {
        self.feature_flag_burn_on_read && self.enable_burn_on_read
    }

    /// `InitProperties`' five-way registration `if` (api4/properties.go:23).
    ///
    /// When every one of the five is off, gorilla/mux has never heard of the nine property paths
    /// and the answer is the mux's own 404 — not a 501 and not a 403. It is **not** off on a stock
    /// server: `ClassificationMarkings` defaults to `true`, so this is `true` unless an operator
    /// explicitly turns that one off.
    pub fn properties_api_enabled(&self) -> bool {
        self.feature_flag_integrated_boards
            || self.feature_flag_managed_channel_categories
            || self.feature_flag_classification_markings
            || self.feature_flag_session_attributes
            || self.feature_flag_post_attributes
    }
}

impl Default for Config {
    /// Go's `SetDefaults` for exactly these fields. **Two of them default to `true`** — copying
    /// the `false` of the two above would silently drop `metadata.emojis` and
    /// `metadata.priority` from every response.
    fn default() -> Self {
        Self {
            restrict_system_admin: false,
            compliance_enable: false,
            image_proxy_enable: false,
            enable_post_icon_override: false,
            enable_custom_emoji: true,
            // config.go:918 — `new(false)`.
            enable_bot_account_creation: false,
            // config.go:599 — `new(false)`.
            enable_dynamic_client_registration: false,
            enable_post_username_override: false,
            post_priority: true,
            // config.go:997 — `new(true)`.
            allow_persistent_notifications: true,
            // config.go:1021 — `ServiceSettingsDefaultUniqueReactionsPerPost` is 50.
            unique_emoji_reaction_limit_per_post: 50,
            // config.go:2620 — `new(DirectMessageAny)`.
            restrict_direct_message: DIRECT_MESSAGE_ANY.to_owned(),
            // config.go:2592 — `new("")`.
            restrict_creation_to_domains: String::new(),
            // config.go:2588 — `new(false)`.
            enable_open_server: false,
            enable_user_statuses: true,
            user_status_away_timeout: 300,
            enable_custom_user_statuses: true,
            require_email_verification: false,
            guest_restrict_creation_to_domains: String::new(),
            allow_synced_drafts: true,
            enable_api_channel_deletion: false,
            // config.go:885 — `new(false)`.
            enable_api_team_deletion: false,
            enable_channel_category_sorting: true,
            // config.go:2629 — `new(int64(2000))`.
            max_channels_per_team: 2000,
            // config.go:2577 — `TeamSettingsDefaultMaxUsersPerTeam`.
            max_users_per_team: 50,
            // config.go:2653 — `[]string{}`.
            experimental_default_channels: Vec::new(),
            enable_burn_on_read: true,
            // config.go:873 — `new(true)`.
            experimental_enable_default_channel_leave_join_messages: true,
            // config.go:870 — `new(-1)`.
            post_edit_time_limit: -1,
            // config.go:906 — `new(false)`.
            experimental_enable_hardened_mode: false,
            feature_flag_burn_on_read: true,
            file_driver_name: "local".to_owned(),
            // config.go:1904 — `FileSettingsDefaultDirectory`.
            file_directory: "./data/".to_owned(),
            // config.go:1888 — 100 MiB.
            file_max_file_size: 100 * 1024 * 1024,
            // config.go:2832 — `LdapSettingsDefaultPictureAttribute`, the empty string.
            ldap_picture_attribute: String::new(),
            saml_enable_sync_with_ldap: false,
            // config.go:2669 — `TeamSettingsLockProfileFieldsNone`.
            lock_profile_fields_for_email_users: "none".to_owned(),
            // No constant default exists; see the field's documentation.
            public_link_salt: String::new(),
            dedicated_export_store: false,
            file_export_driver_name: "local".to_owned(),
            file_export_directory: "./data/".to_owned(),
            export_directory: "./export".to_owned(),
            import_directory: "./import".to_owned(),
            webserver_mode: "gzip".to_owned(),
            enable_incoming_webhooks: true,
            enable_outgoing_webhooks: true,
            enable_oauth_service_provider: true,
            enable_outgoing_oauth_connections: false,
            enable_commands: true,
            enable_public_link: false,
            cloud_preview_modal_bucket_url: String::new(),
            maximum_personal_access_token_lifetime_days: 0,
            enable_user_access_tokens: false,
            message_export_download_export_results: false,
            feature_flag_session_attributes: false,
            // feature_flags.go:194, :202, :206 — all three `false`.
            feature_flag_integrated_boards: false,
            feature_flag_managed_channel_categories: false,
            feature_flag_post_attributes: false,
            // feature_flags.go:208 — `false`, like the other four.
            feature_flag_discoverable_channels: false,
            // feature_flags.go:185 — **`true`**, and the only one of the five that is.
            feature_flag_classification_markings: true,
            // config.go:982 — `new(CollapsedThreadsAlwaysOn)`.
            collapsed_threads: mm_model::config::COLLAPSED_THREADS_ALWAYS_ON.to_owned(),
            // config.go:978 — `new(true)`.
            thread_auto_follow: true,
            // config.go:708 — `new(true)`.
            enable_channel_viewed_messages: true,
            // feature_flags.go:181 — `false`.
            feature_flag_enable_shift_escape_to_mark_all_read: false,
            show_full_name: true,
            show_email_address: true,
            // Absent, not empty: `SetDefaults` never fills `SiteURL`, and this constructor
            // models a config that has not been through `Store.Load` — which is the only thing
            // that plants the `""`.
            site_url: None,
            session_idle_timeout_in_minutes: 43200,
            // `30 * 24`. The fresh-install arm, for the same reason as
            // `extend_session_length_with_activity` below: this constructor models `SetDefaults`
            // over an empty config, which has no `SiteURL`.
            session_length_mobile_in_hours: 720,
            allow_cookies_for_subdomains: false,
            maximum_login_attempts: 10,
            // `new(!isUpdate)` with `isUpdate == false`, the same reasoning as
            // `extend_session_length_with_activity` below: an empty config is a fresh install.
            terminate_sessions_on_password_change: true,
            password_minimum_length: 8,
            password_lowercase: false,
            password_number: false,
            password_uppercase: false,
            password_symbol: false,
            // `new(!isUpdate)` with `isUpdate == false`: this constructor models `SetDefaults` on
            // an **empty** config, which has no `SiteURL` and is therefore a fresh install. Every
            // document a running Go server persists takes the other branch — see
            // [`Config::from_document`].
            extend_session_length_with_activity: true,
            enable_testing: false,
            // **`true`** — one of the few settings here whose default is on, which is what makes
            // the licence arm of the scheduled-post gate the reachable one.
            scheduled_posts: true,
            // `f.EnableAIRecaps = false` (feature_flags.go:192).
            feature_flag_enable_ai_recaps: false,
            // Absent, and absent means **enabled** — see the field's note.
            ai_recap_settings_enable: None,
            // `ClientRequirements` has no `SetDefaults`; the zero value is the default.
            android_latest_version: String::new(),
            android_min_version: String::new(),
            ios_latest_version: String::new(),
            ios_min_version: String::new(),
            // `new(-1)` (config.go:588) — negative, so the health check is off rather than
            // triggering on the first goroutine.
            goroutine_health_threshold: -1,
            disable_database_search: false,
            elasticsearch_enable_searching: false,
            feature_flag_test_feature: "off".to_owned(),
            license: String::new(),
        }
    }
}

impl Config {
    /// Go's defaults with the environment overlaid — i.e. [`Config::load`] with no document.
    ///
    /// Kept as the constructor for tests and for a deployment whose Go server has never written a
    /// configuration row.
    pub fn from_env() -> Self {
        Self::default().apply_env()
    }

    /// Apply the `MM_<SECTION>_<SETTING>` overlay on top of `self`.
    ///
    /// Port of `applyEnvironmentMap` (config/store.go:292), restricted to the settings modelled
    /// here. Go applies this **after** unmarshalling the document and running `SetDefaults`, and
    /// deliberately persists the pre-overlay config (store.go:321) — so this is a layer over the
    /// stored values, never a fallback for them. An operator who sets a variable has overridden
    /// the database on the Go server, and must override it here too.
    ///
    /// The variable names are Mattermost's own convention, so this agrees with the neighbouring
    /// Go server for free whenever that server is configured by environment.
    #[must_use]
    pub fn apply_env(self) -> Self {
        self.apply_env_from(&|key| std::env::var(key).ok())
    }

    /// [`Config::apply_env`] against an arbitrary lookup.
    ///
    /// The indirection exists so the overlay can be tested at all. The process environment is
    /// global and `std::env::set_var` races every other test in the binary, so an overlay that
    /// read `std::env` directly could only ever be exercised with **nothing set** — under which
    /// it is indistinguishable from doing nothing, and a mutation deleting it survives. Measured:
    /// two did, before this existed.
    #[must_use]
    fn apply_env_from(self, lookup: &impl Fn(&str) -> Option<String>) -> Self {
        let default = self;
        Self {
            restrict_system_admin: lookup_bool(
                lookup,
                "MM_EXPERIMENTALSETTINGS_RESTRICTSYSTEMADMIN",
                default.restrict_system_admin,
            ),
            compliance_enable: lookup_bool(
                lookup,
                "MM_COMPLIANCESETTINGS_ENABLE",
                default.compliance_enable,
            ),
            image_proxy_enable: lookup_bool(
                lookup,
                "MM_IMAGEPROXYSETTINGS_ENABLE",
                default.image_proxy_enable,
            ),
            enable_post_icon_override: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEPOSTICONOVERRIDE",
                default.enable_post_icon_override,
            ),
            enable_dynamic_client_registration: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEDYNAMICCLIENTREGISTRATION",
                default.enable_dynamic_client_registration,
            ),
            enable_post_username_override: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEPOSTUSERNAMEOVERRIDE",
                default.enable_post_username_override,
            ),
            enable_custom_emoji: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLECUSTOMEMOJI",
                default.enable_custom_emoji,
            ),
            enable_bot_account_creation: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEBOTACCOUNTCREATION",
                default.enable_bot_account_creation,
            ),
            allow_persistent_notifications: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ALLOWPERSISTENTNOTIFICATIONS",
                default.allow_persistent_notifications,
            ),
            unique_emoji_reaction_limit_per_post: clamp_unique_reactions(lookup_int(
                lookup,
                "MM_SERVICESETTINGS_UNIQUEEMOJIREACTIONLIMITPERPOST",
                default.unique_emoji_reaction_limit_per_post,
            )),
            restrict_direct_message: lookup("MM_TEAMSETTINGS_RESTRICTDIRECTMESSAGE")
                .unwrap_or(default.restrict_direct_message),
            restrict_creation_to_domains: lookup("MM_TEAMSETTINGS_RESTRICTCREATIONTODOMAINS")
                .unwrap_or(default.restrict_creation_to_domains),
            enable_open_server: lookup_bool(
                lookup,
                "MM_TEAMSETTINGS_ENABLEOPENSERVER",
                default.enable_open_server,
            ),
            enable_user_statuses: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEUSERSTATUSES",
                default.enable_user_statuses,
            ),
            user_status_away_timeout: lookup_int(
                lookup,
                "MM_TEAMSETTINGS_USERSTATUSAWAYTIMEOUT",
                default.user_status_away_timeout,
            ),
            enable_custom_user_statuses: lookup_bool(
                lookup,
                "MM_TEAMSETTINGS_ENABLECUSTOMUSERSTATUSES",
                default.enable_custom_user_statuses,
            ),
            require_email_verification: lookup_bool(
                lookup,
                "MM_EMAILSETTINGS_REQUIREEMAILVERIFICATION",
                default.require_email_verification,
            ),
            guest_restrict_creation_to_domains: lookup(
                "MM_GUESTACCOUNTSSETTINGS_RESTRICTCREATIONTODOMAINS",
            )
            .unwrap_or(default.guest_restrict_creation_to_domains),
            post_priority: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_POSTPRIORITY",
                default.post_priority,
            ),
            allow_synced_drafts: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ALLOWSYNCEDDRAFTS",
                default.allow_synced_drafts,
            ),
            enable_api_channel_deletion: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEAPICHANNELDELETION",
                default.enable_api_channel_deletion,
            ),
            enable_api_team_deletion: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEAPITEAMDELETION",
                default.enable_api_team_deletion,
            ),
            enable_channel_category_sorting: lookup_bool(
                lookup,
                "MM_TEAMSETTINGS_ENABLECHANNELCATEGORYSORTING",
                default.enable_channel_category_sorting,
            ),
            max_channels_per_team: lookup("MM_TEAMSETTINGS_MAXCHANNELSPERTEAM")
                .and_then(|value| value.parse().ok())
                .unwrap_or(default.max_channels_per_team),
            max_users_per_team: lookup_int(
                lookup,
                "MM_TEAMSETTINGS_MAXUSERSPERTEAM",
                default.max_users_per_team,
            ),
            // Go's env decoder splits a `[]string` setting on commas, so the environment form of
            // this is `town-square,welcome`. An unset variable and an empty one are different:
            // unset keeps the default, and `""` is an empty list — which is also the default, so
            // the distinction is invisible here and would not be for a non-empty default.
            experimental_default_channels: lookup("MM_TEAMSETTINGS_EXPERIMENTALDEFAULTCHANNELS")
                .map(|raw| split_list(&raw))
                .unwrap_or(default.experimental_default_channels),
            enable_burn_on_read: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEBURNONREAD",
                default.enable_burn_on_read,
            ),
            experimental_enable_default_channel_leave_join_messages: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_EXPERIMENTALENABLEDEFAULTCHANNELLEAVEJOINMESSAGES",
                default.experimental_enable_default_channel_leave_join_messages,
            ),
            post_edit_time_limit: lookup_int(
                lookup,
                "MM_SERVICESETTINGS_POSTEDITTIMELIMIT",
                default.post_edit_time_limit,
            ),
            experimental_enable_hardened_mode: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_EXPERIMENTALENABLEHARDENEDMODE",
                default.experimental_enable_hardened_mode,
            ),
            feature_flag_burn_on_read: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_BURNONREAD",
                default.feature_flag_burn_on_read,
            ),
            // Not `env_bool`'s fallback rule: a string setting has no unparseable value, so an
            // override of `""` is a deliberate empty driver and must survive as one.
            file_driver_name: lookup("MM_FILESETTINGS_DRIVERNAME")
                .unwrap_or(default.file_driver_name),
            file_directory: lookup("MM_FILESETTINGS_DIRECTORY").unwrap_or(default.file_directory),
            file_max_file_size: lookup_int(
                lookup,
                "MM_FILESETTINGS_MAXFILESIZE",
                default.file_max_file_size,
            ),
            ldap_picture_attribute: lookup("MM_LDAPSETTINGS_PICTUREATTRIBUTE")
                .unwrap_or(default.ldap_picture_attribute),
            saml_enable_sync_with_ldap: lookup_bool(
                lookup,
                "MM_SAMLSETTINGS_ENABLESYNCWITHLDAP",
                default.saml_enable_sync_with_ldap,
            ),
            lock_profile_fields_for_email_users: lookup(
                "MM_TEAMSETTINGS_LOCKPROFILEFIELDSFOREMAILUSERS",
            )
            .unwrap_or(default.lock_profile_fields_for_email_users),
            public_link_salt: lookup("MM_FILESETTINGS_PUBLICLINKSALT")
                .unwrap_or(default.public_link_salt),
            dedicated_export_store: lookup_bool(
                lookup,
                "MM_FILESETTINGS_DEDICATEDEXPORTSTORE",
                default.dedicated_export_store,
            ),
            file_export_driver_name: lookup("MM_FILESETTINGS_EXPORTDRIVERNAME")
                .unwrap_or(default.file_export_driver_name),
            file_export_directory: lookup("MM_FILESETTINGS_EXPORTDIRECTORY")
                .unwrap_or(default.file_export_directory),
            export_directory: lookup("MM_EXPORTSETTINGS_DIRECTORY")
                .unwrap_or(default.export_directory),
            import_directory: lookup("MM_IMPORTSETTINGS_DIRECTORY")
                .unwrap_or(default.import_directory),
            // `SetDefaults` folds `regular` into `gzip` whatever the source, so the overlay is
            // normalised on the way in exactly as the document is.
            webserver_mode: normalise_webserver_mode(
                lookup("MM_SERVICESETTINGS_WEBSERVERMODE").unwrap_or(default.webserver_mode),
            ),
            android_latest_version: lookup("MM_CLIENTREQUIREMENTS_ANDROIDLATESTVERSION")
                .unwrap_or(default.android_latest_version),
            android_min_version: lookup("MM_CLIENTREQUIREMENTS_ANDROIDMINVERSION")
                .unwrap_or(default.android_min_version),
            ios_latest_version: lookup("MM_CLIENTREQUIREMENTS_IOSLATESTVERSION")
                .unwrap_or(default.ios_latest_version),
            ios_min_version: lookup("MM_CLIENTREQUIREMENTS_IOSMINVERSION")
                .unwrap_or(default.ios_min_version),
            feature_flag_test_feature: lookup("MM_FEATUREFLAGS_TESTFEATURE")
                .unwrap_or(default.feature_flag_test_feature),
            enable_testing: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLETESTING",
                default.enable_testing,
            ),
            scheduled_posts: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_SCHEDULEDPOSTS",
                default.scheduled_posts,
            ),
            feature_flag_enable_ai_recaps: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_ENABLEAIRECAPS",
                default.feature_flag_enable_ai_recaps,
            ),
            // An overlay can only ever *set* this, never restore it to absent — which matches
            // Go, whose environment layer writes a pointer to the parsed value.
            ai_recap_settings_enable: lookup("MM_AIRECAPSETTINGS_ENABLE")
                .and_then(|raw| parse_bool(&raw))
                .or(default.ai_recap_settings_enable),
            goroutine_health_threshold: lookup_int(
                lookup,
                "MM_SERVICESETTINGS_GOROUTINEHEALTHTHRESHOLD",
                default.goroutine_health_threshold,
            ),
            disable_database_search: lookup_bool(
                lookup,
                "MM_SQLSETTINGS_DISABLEDATABASESEARCH",
                default.disable_database_search,
            ),
            elasticsearch_enable_searching: lookup_bool(
                lookup,
                "MM_ELASTICSEARCHSETTINGS_ENABLESEARCHING",
                default.elasticsearch_enable_searching,
            ),
            enable_incoming_webhooks: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEINCOMINGWEBHOOKS",
                default.enable_incoming_webhooks,
            ),
            enable_outgoing_webhooks: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEOUTGOINGWEBHOOKS",
                default.enable_outgoing_webhooks,
            ),
            enable_oauth_service_provider: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEOAUTHSERVICEPROVIDER",
                default.enable_oauth_service_provider,
            ),
            enable_outgoing_oauth_connections: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEOUTGOINGOAUTHCONNECTIONS",
                default.enable_outgoing_oauth_connections,
            ),
            enable_commands: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLECOMMANDS",
                default.enable_commands,
            ),
            enable_public_link: lookup_bool(
                lookup,
                "MM_FILESETTINGS_ENABLEPUBLICLINK",
                default.enable_public_link,
            ),
            cloud_preview_modal_bucket_url: lookup("MM_CLOUDSETTINGS_PREVIEWMODALBUCKETURL")
                .unwrap_or(default.cloud_preview_modal_bucket_url),
            maximum_personal_access_token_lifetime_days: lookup_int(
                lookup,
                "MM_SERVICESETTINGS_MAXIMUMPERSONALACCESSTOKENLIFETIMEDAYS",
                default.maximum_personal_access_token_lifetime_days,
            ),
            enable_user_access_tokens: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEUSERACCESSTOKENS",
                default.enable_user_access_tokens,
            ),
            message_export_download_export_results: lookup_bool(
                lookup,
                "MM_MESSAGEEXPORTSETTINGS_DOWNLOADEXPORTRESULTS",
                default.message_export_download_export_results,
            ),
            feature_flag_session_attributes: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_SESSIONATTRIBUTES",
                default.feature_flag_session_attributes,
            ),
            feature_flag_integrated_boards: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_INTEGRATEDBOARDS",
                default.feature_flag_integrated_boards,
            ),
            feature_flag_managed_channel_categories: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_MANAGEDCHANNELCATEGORIES",
                default.feature_flag_managed_channel_categories,
            ),
            feature_flag_classification_markings: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_CLASSIFICATIONMARKINGS",
                default.feature_flag_classification_markings,
            ),
            feature_flag_post_attributes: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_POSTATTRIBUTES",
                default.feature_flag_post_attributes,
            ),
            feature_flag_discoverable_channels: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_DISCOVERABLECHANNELS",
                default.feature_flag_discoverable_channels,
            ),
            collapsed_threads: lookup("MM_SERVICESETTINGS_COLLAPSEDTHREADS")
                .unwrap_or(default.collapsed_threads),
            thread_auto_follow: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_THREADAUTOFOLLOW",
                default.thread_auto_follow,
            ),
            enable_channel_viewed_messages: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLECHANNELVIEWEDMESSAGES",
                default.enable_channel_viewed_messages,
            ),
            feature_flag_enable_shift_escape_to_mark_all_read: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD",
                default.feature_flag_enable_shift_escape_to_mark_all_read,
            ),
            show_full_name: lookup_bool(
                lookup,
                "MM_PRIVACYSETTINGS_SHOWFULLNAME",
                default.show_full_name,
            ),
            show_email_address: lookup_bool(
                lookup,
                "MM_PRIVACYSETTINGS_SHOWEMAILADDRESS",
                default.show_email_address,
            ),
            // `MM_SERVICESETTINGS_SITEURL` is the variable the running Go server beside us is
            // configured with on this stack, and it is the one setting where the document and the
            // process genuinely disagree — see the module doc. An override always *sets* the
            // value, so it also makes `isUpdate` true, matching `applyEnvKey`, which dereferences
            // the pointer `SetDefaults` already planted.
            site_url: lookup("MM_SERVICESETTINGS_SITEURL").or(default.site_url),
            session_idle_timeout_in_minutes: lookup_int(
                lookup,
                "MM_SERVICESETTINGS_SESSIONIDLETIMEOUTINMINUTES",
                default.session_idle_timeout_in_minutes,
            ),
            session_length_mobile_in_hours: lookup_int(
                lookup,
                "MM_SERVICESETTINGS_SESSIONLENGTHMOBILEINHOURS",
                default.session_length_mobile_in_hours,
            ),
            allow_cookies_for_subdomains: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ALLOWCOOKIESFORSUBDOMAINS",
                default.allow_cookies_for_subdomains,
            ),
            maximum_login_attempts: lookup_int(
                lookup,
                "MM_SERVICESETTINGS_MAXIMUMLOGINATTEMPTS",
                default.maximum_login_attempts,
            ),
            terminate_sessions_on_password_change: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_TERMINATESESSIONSONPASSWORDCHANGE",
                default.terminate_sessions_on_password_change,
            ),
            password_minimum_length: lookup_int(
                lookup,
                "MM_PASSWORDSETTINGS_MINIMUMLENGTH",
                default.password_minimum_length,
            ),
            password_lowercase: lookup_bool(
                lookup,
                "MM_PASSWORDSETTINGS_LOWERCASE",
                default.password_lowercase,
            ),
            password_number: lookup_bool(
                lookup,
                "MM_PASSWORDSETTINGS_NUMBER",
                default.password_number,
            ),
            password_uppercase: lookup_bool(
                lookup,
                "MM_PASSWORDSETTINGS_UPPERCASE",
                default.password_uppercase,
            ),
            password_symbol: lookup_bool(
                lookup,
                "MM_PASSWORDSETTINGS_SYMBOL",
                default.password_symbol,
            ),
            extend_session_length_with_activity: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_EXTENDSESSIONLENGTHWITHACTIVITY",
                default.extend_session_length_with_activity,
            ),
            // Its own variable, not part of the `MM_<SECTION>_<SETTING>` overlay, and Go treats
            // any non-empty value as "a licence was supplied" before it ever tries to parse it.
            license: lookup("MM_LICENSE").unwrap_or(default.license),
        }
    }

    /// Parse a persisted `model.Config` document into the settings modelled here.
    ///
    /// Port of the `json.Unmarshal` plus `SetDefaults` pair in `Store.Load` (config/store.go:260
    /// and :285), for these fields only. Unknown sections and unknown keys are ignored, which is
    /// what makes growing this struct one field at a time safe: the document already holds all 47
    /// sections, and a field appears here when something ported reads it.
    ///
    /// # Absent means Go's default, which is not the zero value
    ///
    /// Every setting in `config.go` is a **pointer**, and `SetDefaults` fills the nil ones — so an
    /// absent key takes Go's default, and for `EnableCustomEmoji`, `PostPriority`,
    /// `AllowSyncedDrafts`, the two webhook toggles, the OAuth toggle and both privacy settings
    /// that default is `true`. This is why every field below is an `Option<T>` resolved against
    /// [`Config::default`] rather than a `#[serde(default)]`: the derived default for a `bool` is
    /// `false`, so the tidier-looking spelling would silently turn eight features off whenever the
    /// document omitted them. A JSON `null` lands in the same place as an absent key, which is
    /// also Go's behaviour — it unmarshals to a nil pointer that `SetDefaults` then fills.
    pub fn from_document(document: &str) -> Result<Self, ConfigError> {
        let parsed: Document =
            serde_json::from_str(document).map_err(|source| ConfigError::Malformed { source })?;
        let default = Self::default();

        let service = parsed.service_settings.unwrap_or_default();
        // Port of `Config.isUpdate` (config.go:4289): "a pre-existing config" is detected by
        // `ServiceSettings.SiteURL != nil` and nothing else. `Store.Load` plants a `SiteURL` of
        // `""` before calling `SetDefaults` when the document has none (config/store.go:280), so
        // every document a running server persists is an *update* — a JSON `null` and an absent
        // key both land here as `false`, matching Go's nil pointer.
        let is_update = service.site_url.is_some();
        let client_requirements = parsed.client_requirements.unwrap_or_default();
        let team_settings = parsed.team_settings.unwrap_or_default();
        let email_settings = parsed.email_settings.unwrap_or_default();
        let guest_accounts = parsed.guest_accounts_settings.unwrap_or_default();
        let file_settings = parsed.file_settings.unwrap_or_default();
        let password_settings = parsed.password_settings.unwrap_or_default();
        Ok(Self {
            // Moved, not cloned: `is_update` above already took the only other thing anything
            // wants from this field, and the remaining `service` reads are all `Option<bool>`.
            site_url: service.site_url,
            restrict_system_admin: parsed
                .experimental_settings
                .unwrap_or_default()
                .restrict_system_admin
                .unwrap_or(default.restrict_system_admin),
            compliance_enable: parsed
                .compliance_settings
                .unwrap_or_default()
                .enable
                .unwrap_or(default.compliance_enable),
            image_proxy_enable: parsed
                .image_proxy_settings
                .unwrap_or_default()
                .enable
                .unwrap_or(default.image_proxy_enable),
            enable_post_icon_override: service
                .enable_post_icon_override
                .unwrap_or(default.enable_post_icon_override),
            enable_dynamic_client_registration: service
                .enable_dynamic_client_registration
                .unwrap_or(default.enable_dynamic_client_registration),
            enable_outgoing_oauth_connections: service
                .enable_outgoing_oauth_connections
                .unwrap_or(default.enable_outgoing_oauth_connections),
            maximum_personal_access_token_lifetime_days: service
                .maximum_personal_access_token_lifetime_days
                .unwrap_or(default.maximum_personal_access_token_lifetime_days),
            enable_user_access_tokens: service
                .enable_user_access_tokens
                .unwrap_or(default.enable_user_access_tokens),
            enable_commands: service.enable_commands.unwrap_or(default.enable_commands),
            enable_public_link: file_settings
                .enable_public_link
                .unwrap_or(default.enable_public_link),
            cloud_preview_modal_bucket_url: parsed
                .cloud_settings
                .clone()
                .unwrap_or_default()
                .preview_modal_bucket_url
                .unwrap_or(default.cloud_preview_modal_bucket_url),
            message_export_download_export_results: parsed
                .message_export_settings
                .unwrap_or_default()
                .download_export_results
                .unwrap_or(default.message_export_download_export_results),
            // The same rule as `feature_flag_burn_on_read` above: `FeatureFlags` never reaches the
            // persisted document, so there is nothing here to read.
            feature_flag_session_attributes: default.feature_flag_session_attributes,
            feature_flag_integrated_boards: default.feature_flag_integrated_boards,
            feature_flag_managed_channel_categories: default
                .feature_flag_managed_channel_categories,
            feature_flag_classification_markings: default.feature_flag_classification_markings,
            feature_flag_post_attributes: default.feature_flag_post_attributes,
            feature_flag_discoverable_channels: default.feature_flag_discoverable_channels,
            collapsed_threads: service
                .collapsed_threads
                .unwrap_or(default.collapsed_threads),
            thread_auto_follow: service
                .thread_auto_follow
                .unwrap_or(default.thread_auto_follow),
            enable_channel_viewed_messages: service
                .enable_channel_viewed_messages
                .unwrap_or(default.enable_channel_viewed_messages),
            // `FeatureFlags` is stripped before the document is persisted; see the field docs.
            feature_flag_enable_shift_escape_to_mark_all_read: default
                .feature_flag_enable_shift_escape_to_mark_all_read,
            enable_post_username_override: service
                .enable_post_username_override
                .unwrap_or(default.enable_post_username_override),
            enable_custom_emoji: service
                .enable_custom_emoji
                .unwrap_or(default.enable_custom_emoji),
            enable_bot_account_creation: service
                .enable_bot_account_creation
                .unwrap_or(default.enable_bot_account_creation),
            post_priority: service.post_priority.unwrap_or(default.post_priority),
            allow_persistent_notifications: service
                .allow_persistent_notifications
                .unwrap_or(default.allow_persistent_notifications),
            // The clamp is Go's, and it is applied on *load*: `SetDefaults` rewrites a document
            // value above 500 down to 500 (config.go:1025), so a running server never operates on
            // the number the document holds. Reading it without the clamp would let a
            // misconfigured document raise this server's limit above the Go server's.
            unique_emoji_reaction_limit_per_post: clamp_unique_reactions(
                service
                    .unique_emoji_reaction_limit_per_post
                    .unwrap_or(default.unique_emoji_reaction_limit_per_post),
            ),
            restrict_direct_message: team_settings
                .restrict_direct_message
                .unwrap_or(default.restrict_direct_message),
            restrict_creation_to_domains: team_settings
                .restrict_creation_to_domains
                .unwrap_or(default.restrict_creation_to_domains),
            enable_open_server: team_settings
                .enable_open_server
                .unwrap_or(default.enable_open_server),
            enable_user_statuses: service
                .enable_user_statuses
                .unwrap_or(default.enable_user_statuses),
            user_status_away_timeout: team_settings
                .user_status_away_timeout
                .unwrap_or(default.user_status_away_timeout),
            enable_custom_user_statuses: team_settings
                .enable_custom_user_statuses
                .unwrap_or(default.enable_custom_user_statuses),
            require_email_verification: email_settings
                .require_email_verification
                .unwrap_or(default.require_email_verification),
            guest_restrict_creation_to_domains: guest_accounts
                .restrict_creation_to_domains
                .unwrap_or(default.guest_restrict_creation_to_domains),
            allow_synced_drafts: service
                .allow_synced_drafts
                .unwrap_or(default.allow_synced_drafts),
            enable_api_channel_deletion: service
                .enable_api_channel_deletion
                .unwrap_or(default.enable_api_channel_deletion),
            enable_api_team_deletion: service
                .enable_api_team_deletion
                .unwrap_or(default.enable_api_team_deletion),
            enable_channel_category_sorting: team_settings
                .enable_channel_category_sorting
                .unwrap_or(default.enable_channel_category_sorting),
            max_channels_per_team: team_settings
                .max_channels_per_team
                .unwrap_or(default.max_channels_per_team),
            max_users_per_team: team_settings
                .max_users_per_team
                .unwrap_or(default.max_users_per_team),
            experimental_default_channels: team_settings
                .experimental_default_channels
                .unwrap_or(default.experimental_default_channels),
            enable_burn_on_read: service
                .enable_burn_on_read
                .unwrap_or(default.enable_burn_on_read),
            experimental_enable_default_channel_leave_join_messages: service
                .experimental_enable_default_channel_leave_join_messages
                .unwrap_or(default.experimental_enable_default_channel_leave_join_messages),
            post_edit_time_limit: service
                .post_edit_time_limit
                .unwrap_or(default.post_edit_time_limit),
            experimental_enable_hardened_mode: service
                .experimental_enable_hardened_mode
                .unwrap_or(default.experimental_enable_hardened_mode),
            // Deliberately NOT read from the document: Go clears `FeatureFlags` before persisting
            // (store.go:306-310), so the section is absent from every row it writes. Sourcing it
            // here would read an absence as a deliberate `false` on the next `readOnlyFF` change.
            feature_flag_burn_on_read: default.feature_flag_burn_on_read,
            file_driver_name: file_settings
                .driver_name
                .unwrap_or(default.file_driver_name),
            // `SetDefaults` replaces an empty directory with the default as well as a nil one
            // (config.go:1903), which `unwrap_or` alone would not: a document holding `""` must
            // read `./data/`, not `""`.
            file_directory: non_empty_or(file_settings.directory, default.file_directory),
            file_max_file_size: file_settings
                .max_file_size
                .unwrap_or(default.file_max_file_size),
            ldap_picture_attribute: parsed
                .ldap_settings
                .unwrap_or_default()
                .picture_attribute
                .unwrap_or(default.ldap_picture_attribute),
            saml_enable_sync_with_ldap: parsed
                .saml_settings
                .unwrap_or_default()
                .enable_sync_with_ldap
                .unwrap_or(default.saml_enable_sync_with_ldap),
            lock_profile_fields_for_email_users: team_settings
                .lock_profile_fields_for_email_users
                .unwrap_or(default.lock_profile_fields_for_email_users),
            public_link_salt: file_settings
                .public_link_salt
                .unwrap_or(default.public_link_salt),
            dedicated_export_store: file_settings
                .dedicated_export_store
                .unwrap_or(default.dedicated_export_store),
            file_export_driver_name: file_settings
                .export_driver_name
                .unwrap_or(default.file_export_driver_name),
            file_export_directory: non_empty_or(
                file_settings.export_directory,
                default.file_export_directory,
            ),
            export_directory: non_empty_or(
                parsed.export_settings.unwrap_or_default().directory,
                default.export_directory,
            ),
            import_directory: non_empty_or(
                parsed.import_settings.unwrap_or_default().directory,
                default.import_directory,
            ),
            webserver_mode: normalise_webserver_mode(
                service.webserver_mode.unwrap_or(default.webserver_mode),
            ),
            enable_incoming_webhooks: service
                .enable_incoming_webhooks
                .unwrap_or(default.enable_incoming_webhooks),
            enable_outgoing_webhooks: service
                .enable_outgoing_webhooks
                .unwrap_or(default.enable_outgoing_webhooks),
            enable_oauth_service_provider: service
                .enable_oauth_service_provider
                .unwrap_or(default.enable_oauth_service_provider),
            show_full_name: parsed
                .privacy_settings
                .as_ref()
                .and_then(|p| p.show_full_name)
                .unwrap_or(default.show_full_name),
            show_email_address: parsed
                .privacy_settings
                .and_then(|p| p.show_email_address)
                .unwrap_or(default.show_email_address),
            session_idle_timeout_in_minutes: service
                .session_idle_timeout_in_minutes
                .unwrap_or(default.session_idle_timeout_in_minutes),
            // The two-step cascade of `SetDefaults` (config.go:767), reproduced rather than
            // collapsed: an explicit `SessionLengthMobileInHours` wins outright; failing that,
            // an explicit `SessionLengthMobileInDays` is multiplied by 24; failing *that*,
            // `isUpdate` chooses 180 days or 30. A document carrying only the days field is the
            // middle branch and is the one a collapsed port would get wrong.
            session_length_mobile_in_hours: service
                .session_length_mobile_in_hours
                .or_else(|| service.session_length_mobile_in_days.map(|days| days * 24))
                .unwrap_or(if is_update { 180 * 24 } else { 30 * 24 }),
            allow_cookies_for_subdomains: service
                .allow_cookies_for_subdomains
                .unwrap_or(default.allow_cookies_for_subdomains),
            // The one setting here whose default is computed rather than looked up. `new(!isUpdate)`
            // (config.go:729), and the comment above it in the Go source says why: "Must be
            // manually enabled for existing installations." Resolving it against
            // `Config::default` like every neighbour would read an absent key on a real server's
            // document as `true` and silently disarm the idle-timeout check.
            extend_session_length_with_activity: service
                .extend_session_length_with_activity
                .unwrap_or(!is_update),
            maximum_login_attempts: service
                .maximum_login_attempts
                .unwrap_or(default.maximum_login_attempts),
            // The second `!isUpdate` default (config.go:733), and the one that matters more: a
            // resolved-from-`default` `true` would log every other device out on a password
            // change that Go leaves alone.
            terminate_sessions_on_password_change: service
                .terminate_sessions_on_password_change
                .unwrap_or(!is_update),
            password_minimum_length: password_settings
                .minimum_length
                .unwrap_or(default.password_minimum_length),
            password_lowercase: password_settings
                .lowercase
                .unwrap_or(default.password_lowercase),
            password_number: password_settings.number.unwrap_or(default.password_number),
            password_uppercase: password_settings
                .uppercase
                .unwrap_or(default.password_uppercase),
            password_symbol: password_settings.symbol.unwrap_or(default.password_symbol),
            goroutine_health_threshold: service
                .goroutine_health_threshold
                .unwrap_or(default.goroutine_health_threshold),
            enable_testing: service.enable_testing.unwrap_or(default.enable_testing),
            scheduled_posts: service.scheduled_posts.unwrap_or(default.scheduled_posts),
            android_latest_version: client_requirements
                .android_latest_version
                .unwrap_or(default.android_latest_version),
            android_min_version: client_requirements
                .android_min_version
                .unwrap_or(default.android_min_version),
            ios_latest_version: client_requirements
                .ios_latest_version
                .unwrap_or(default.ios_latest_version),
            ios_min_version: client_requirements
                .ios_min_version
                .unwrap_or(default.ios_min_version),
            disable_database_search: parsed
                .sql_settings
                .unwrap_or_default()
                .disable_database_search
                .unwrap_or(default.disable_database_search),
            elasticsearch_enable_searching: parsed
                .elasticsearch_settings
                .unwrap_or_default()
                .enable_searching
                .unwrap_or(default.elasticsearch_enable_searching),
            // Same rule as `feature_flag_burn_on_read` above: `FeatureFlags` is cleared before
            // the document is persisted, so reading it here would turn an absence into a value.
            feature_flag_test_feature: default.feature_flag_test_feature,
            feature_flag_enable_ai_recaps: default.feature_flag_enable_ai_recaps,
            // **Not** `unwrap_or(default)`: the field is `Option` on purpose and an absent
            // `Enable` is a different input from `false`. Carried through as it arrived.
            ai_recap_settings_enable: parsed.ai_recap_settings.unwrap_or_default().enable,
            // Not a config field on either server — `MM_LICENSE` is its own variable, read by
            // `apply_env`.
            license: default.license,
        })
    }

    /// Load the configuration the Go server is running on: the active document, then the
    /// environment overlay.
    ///
    /// The order is Go's (config/store.go:260 → :285 → :292) and it is the whole point of the
    /// function. An absent document is not an error — Go's own `DatabaseStore.Load` answers a
    /// missing row with a marshalled default config (database.go:232) — so this falls back to
    /// [`Config::default`] and still applies the overlay.
    pub async fn load(store: &impl mm_store::ConfigStore) -> Result<Self, ConfigError> {
        Self::load_with_env(store, &|key| std::env::var(key).ok()).await
    }

    /// [`Config::load`] against an arbitrary environment lookup, so the *composition* of document
    /// and overlay is testable and not only each half separately.
    async fn load_with_env(
        store: &impl mm_store::ConfigStore,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let document = store.load_active().await?;
        let base = match document.as_deref() {
            Some(raw) => Self::from_document(raw)?,
            None => {
                tracing::warn!(
                    "no active row in Configurations; falling back to Go's compiled-in defaults. \
                     Is MM_CONFIG pointed at this database?"
                );
                Self::default()
            }
        };
        Ok(base.apply_env_from(lookup))
    }
}

/// Failure modes of [`Config::load`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The row could not be read.
    #[error("failed to read the active configuration")]
    Store(#[from] mm_store::StoreError),

    /// The row exists but is not a `model.Config` document.
    ///
    /// Go treats this as fatal too (`HumanizeJSONError`, store.go:261) — a server running on a
    /// configuration it could not parse is worse than one that refuses to start.
    #[error("the active configuration is not valid JSON")]
    Malformed {
        #[source]
        source: serde_json::Error,
    },
}

/// The slice of the persisted `model.Config` document this module reads.
///
/// Field-for-field a subset; the names are Go's own struct field names, because `config.go`
/// carries no `json:` tags on these and `encoding/json` therefore uses the Go identifier verbatim.
/// Confirmed against the live row rather than assumed.
#[derive(Debug, serde::Deserialize)]
struct Document {
    #[serde(rename = "ServiceSettings")]
    service_settings: Option<ServiceSettingsDocument>,
    #[serde(rename = "ComplianceSettings")]
    compliance_settings: Option<EnableOnlyDocument>,
    #[serde(rename = "ExperimentalSettings")]
    experimental_settings: Option<ExperimentalSettingsDocument>,
    #[serde(rename = "ImageProxySettings")]
    image_proxy_settings: Option<EnableOnlyDocument>,
    #[serde(rename = "FileSettings")]
    file_settings: Option<FileSettingsDocument>,
    #[serde(rename = "PasswordSettings")]
    password_settings: Option<PasswordSettingsDocument>,
    #[serde(rename = "ExportSettings")]
    export_settings: Option<ExportSettingsDocument>,
    #[serde(rename = "ImportSettings")]
    import_settings: Option<ImportSettingsDocument>,
    #[serde(rename = "PrivacySettings")]
    privacy_settings: Option<PrivacySettingsDocument>,
    #[serde(rename = "ClientRequirements")]
    client_requirements: Option<ClientRequirementsDocument>,
    #[serde(rename = "SqlSettings")]
    sql_settings: Option<SqlSettingsDocument>,
    #[serde(rename = "ElasticsearchSettings")]
    elasticsearch_settings: Option<ElasticsearchSettingsDocument>,
    #[serde(rename = "AIRecapSettings")]
    ai_recap_settings: Option<AIRecapSettingsDocument>,
    #[serde(rename = "TeamSettings")]
    team_settings: Option<TeamSettingsDocument>,
    #[serde(rename = "EmailSettings")]
    email_settings: Option<EmailSettingsDocument>,
    #[serde(rename = "GuestAccountsSettings")]
    guest_accounts_settings: Option<GuestAccountsSettingsDocument>,
    #[serde(rename = "MessageExportSettings")]
    message_export_settings: Option<MessageExportSettingsDocument>,
    #[serde(rename = "CloudSettings")]
    cloud_settings: Option<CloudSettingsDocument>,
    #[serde(rename = "LdapSettings")]
    ldap_settings: Option<LdapSettingsDocument>,
    #[serde(rename = "SamlSettings")]
    saml_settings: Option<SamlSettingsDocument>,
}

/// The one field of `LdapSettings` a migrated route reads — `setProfileImage`'s 409.
#[derive(Debug, Default, serde::Deserialize)]
struct LdapSettingsDocument {
    #[serde(rename = "PictureAttribute")]
    picture_attribute: Option<String>,
}

/// The one field of `SamlSettings` a migrated route reads, and it is only ever consulted
/// alongside `LdapSettings.PictureAttribute`.
#[derive(Debug, Default, serde::Deserialize)]
struct SamlSettingsDocument {
    #[serde(rename = "EnableSyncWithLdap")]
    enable_sync_with_ldap: Option<bool>,
}

/// The four rule flags and the minimum length `IsPasswordValidWithSettings` reads.
///
/// `EnableForgotLink` is absent: it gates a *link in the webapp*, not a server check, and no
/// ported route consults it.
#[derive(Debug, Default, serde::Deserialize)]
struct PasswordSettingsDocument {
    #[serde(rename = "MinimumLength")]
    minimum_length: Option<i64>,
    #[serde(rename = "Lowercase")]
    lowercase: Option<bool>,
    #[serde(rename = "Number")]
    number: Option<bool>,
    #[serde(rename = "Uppercase")]
    uppercase: Option<bool>,
    #[serde(rename = "Symbol")]
    symbol: Option<bool>,
}

/// The one field of `CloudSettings` a migrated route reads.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CloudSettingsDocument {
    #[serde(rename = "PreviewModalBucketURL")]
    preview_modal_bucket_url: Option<String>,
}

/// The one field of `MessageExportSettings` a migrated route reads.
#[derive(Debug, Default, serde::Deserialize)]
struct MessageExportSettingsDocument {
    #[serde(rename = "DownloadExportResults")]
    download_export_results: Option<bool>,
}

/// The one field of `TeamSettings` a migrated route reads.
#[derive(Debug, Default, serde::Deserialize)]
struct TeamSettingsDocument {
    #[serde(rename = "RestrictDirectMessage")]
    restrict_direct_message: Option<String>,
    #[serde(rename = "RestrictCreationToDomains")]
    restrict_creation_to_domains: Option<String>,
    #[serde(rename = "EnableOpenServer")]
    enable_open_server: Option<bool>,
    #[serde(rename = "UserStatusAwayTimeout")]
    user_status_away_timeout: Option<i64>,
    #[serde(rename = "EnableCustomUserStatuses")]
    enable_custom_user_statuses: Option<bool>,
    #[serde(rename = "EnableChannelCategorySorting")]
    enable_channel_category_sorting: Option<bool>,
    #[serde(rename = "MaxChannelsPerTeam")]
    max_channels_per_team: Option<i64>,
    #[serde(rename = "MaxUsersPerTeam")]
    max_users_per_team: Option<i64>,
    #[serde(rename = "ExperimentalDefaultChannels")]
    experimental_default_channels: Option<Vec<String>>,
    #[serde(rename = "LockProfileFieldsForEmailUsers")]
    lock_profile_fields_for_email_users: Option<String>,
}

/// The one field of `EmailSettings` a migrated route reads.
#[derive(Debug, Default, serde::Deserialize)]
struct EmailSettingsDocument {
    #[serde(rename = "RequireEmailVerification")]
    require_email_verification: Option<bool>,
}

/// The one field of `GuestAccountsSettings` a migrated route reads. Its name collides with
/// `TeamSettings.RestrictCreationToDomains`, which is exactly why it needs its own section.
#[derive(Debug, Default, serde::Deserialize)]
struct GuestAccountsSettingsDocument {
    #[serde(rename = "RestrictCreationToDomains")]
    restrict_creation_to_domains: Option<String>,
}

/// The one field of `AIRecapSettings` a migrated route reads. `Option<bool>` all the way through:
/// absent means **enabled**.
#[derive(Debug, Default, serde::Deserialize)]
struct AIRecapSettingsDocument {
    #[serde(rename = "Enable")]
    enable: Option<bool>,
}

/// The four mobile version strings `getSystemPing` echoes. Every field is `Option<String>` for
/// the usual reason: an absent key and an explicit `""` must both fall back to the same default,
/// and here they happen to agree — but the shape is what stops the next field added from
/// disagreeing silently.
#[derive(Debug, Default, serde::Deserialize)]
struct ClientRequirementsDocument {
    #[serde(rename = "AndroidLatestVersion")]
    android_latest_version: Option<String>,
    #[serde(rename = "AndroidMinVersion")]
    android_min_version: Option<String>,
    #[serde(rename = "IosLatestVersion")]
    ios_latest_version: Option<String>,
    #[serde(rename = "IosMinVersion")]
    ios_min_version: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct SqlSettingsDocument {
    #[serde(rename = "DisableDatabaseSearch")]
    disable_database_search: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ElasticsearchSettingsDocument {
    #[serde(rename = "EnableSearching")]
    enable_searching: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ServiceSettingsDocument {
    /// Not a setting anything ported reads — the **`isUpdate` discriminator**, and the only
    /// reason it is modelled. `Config.isUpdate` is `ServiceSettings.SiteURL != nil`
    /// (config.go:4289), and two settings' defaults are `!isUpdate`. Only its presence is
    /// consulted, never its value, which is just as well: the live document holds `""`.
    #[serde(rename = "SiteURL")]
    site_url: Option<String>,
    #[serde(rename = "WebserverMode")]
    webserver_mode: Option<String>,
    #[serde(rename = "SessionIdleTimeoutInMinutes")]
    session_idle_timeout_in_minutes: Option<i64>,
    #[serde(rename = "SessionLengthMobileInHours")]
    session_length_mobile_in_hours: Option<i64>,
    /// Only ever read as the fallback for the field above — Go derives hours from days when the
    /// hours field is absent, so a document written before the hours setting existed still
    /// produces the operator's intended length.
    #[serde(rename = "SessionLengthMobileInDays")]
    session_length_mobile_in_days: Option<i64>,
    #[serde(rename = "AllowCookiesForSubdomains")]
    allow_cookies_for_subdomains: Option<bool>,
    #[serde(rename = "MaximumLoginAttempts")]
    maximum_login_attempts: Option<i64>,
    #[serde(rename = "TerminateSessionsOnPasswordChange")]
    terminate_sessions_on_password_change: Option<bool>,
    #[serde(rename = "ExtendSessionLengthWithActivity")]
    extend_session_length_with_activity: Option<bool>,
    #[serde(rename = "GoroutineHealthThreshold")]
    goroutine_health_threshold: Option<i64>,
    #[serde(rename = "EnableTesting")]
    enable_testing: Option<bool>,
    #[serde(rename = "ScheduledPosts")]
    scheduled_posts: Option<bool>,
    #[serde(rename = "EnablePostIconOverride")]
    enable_post_icon_override: Option<bool>,
    #[serde(rename = "EnableUserStatuses")]
    enable_user_statuses: Option<bool>,
    #[serde(rename = "CollapsedThreads")]
    collapsed_threads: Option<String>,
    #[serde(rename = "ThreadAutoFollow")]
    thread_auto_follow: Option<bool>,
    #[serde(rename = "EnableChannelViewedMessages")]
    enable_channel_viewed_messages: Option<bool>,
    #[serde(rename = "EnableDynamicClientRegistration")]
    enable_dynamic_client_registration: Option<bool>,
    #[serde(rename = "EnableOutgoingOAuthConnections")]
    enable_outgoing_oauth_connections: Option<bool>,
    #[serde(rename = "MaximumPersonalAccessTokenLifetimeDays")]
    maximum_personal_access_token_lifetime_days: Option<i64>,
    #[serde(rename = "EnableUserAccessTokens")]
    enable_user_access_tokens: Option<bool>,
    #[serde(rename = "EnableCommands")]
    enable_commands: Option<bool>,
    #[serde(rename = "EnablePostUsernameOverride")]
    enable_post_username_override: Option<bool>,
    #[serde(rename = "EnableCustomEmoji")]
    enable_custom_emoji: Option<bool>,
    #[serde(rename = "EnableBotAccountCreation")]
    enable_bot_account_creation: Option<bool>,
    #[serde(rename = "PostPriority")]
    post_priority: Option<bool>,
    #[serde(rename = "AllowPersistentNotifications")]
    allow_persistent_notifications: Option<bool>,
    #[serde(rename = "UniqueEmojiReactionLimitPerPost")]
    unique_emoji_reaction_limit_per_post: Option<i64>,
    #[serde(rename = "AllowSyncedDrafts")]
    allow_synced_drafts: Option<bool>,
    #[serde(rename = "EnableAPIChannelDeletion")]
    enable_api_channel_deletion: Option<bool>,
    #[serde(rename = "EnableAPITeamDeletion")]
    enable_api_team_deletion: Option<bool>,
    #[serde(rename = "EnableBurnOnRead")]
    enable_burn_on_read: Option<bool>,
    #[serde(rename = "ExperimentalEnableDefaultChannelLeaveJoinMessages")]
    experimental_enable_default_channel_leave_join_messages: Option<bool>,
    #[serde(rename = "PostEditTimeLimit")]
    post_edit_time_limit: Option<i64>,
    #[serde(rename = "ExperimentalEnableHardenedMode")]
    experimental_enable_hardened_mode: Option<bool>,
    #[serde(rename = "EnableIncomingWebhooks")]
    enable_incoming_webhooks: Option<bool>,
    #[serde(rename = "EnableOutgoingWebhooks")]
    enable_outgoing_webhooks: Option<bool>,
    #[serde(rename = "EnableOAuthServiceProvider")]
    enable_oauth_service_provider: Option<bool>,
}

/// `ComplianceSettings` and `ImageProxySettings` both contribute exactly one field, and it has the
/// same name in each.
#[derive(Debug, Default, serde::Deserialize)]
struct EnableOnlyDocument {
    #[serde(rename = "Enable")]
    enable: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ExperimentalSettingsDocument {
    #[serde(rename = "RestrictSystemAdmin")]
    restrict_system_admin: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileSettingsDocument {
    #[serde(rename = "DriverName")]
    driver_name: Option<String>,
    #[serde(rename = "MaxFileSize")]
    max_file_size: Option<i64>,
    #[serde(rename = "Directory")]
    directory: Option<String>,
    #[serde(rename = "EnablePublicLink")]
    enable_public_link: Option<bool>,
    #[serde(rename = "PublicLinkSalt")]
    public_link_salt: Option<String>,
    #[serde(rename = "DedicatedExportStore")]
    dedicated_export_store: Option<bool>,
    #[serde(rename = "ExportDriverName")]
    export_driver_name: Option<String>,
    #[serde(rename = "ExportDirectory")]
    export_directory: Option<String>,
}

/// `ExportSettings` — one key, and it is not the same directory as `FileSettings.ExportDirectory`.
#[derive(Debug, Default, serde::Deserialize)]
struct ExportSettingsDocument {
    #[serde(rename = "Directory")]
    directory: Option<String>,
}

/// `ImportSettings`.
#[derive(Debug, Default, serde::Deserialize)]
struct ImportSettingsDocument {
    #[serde(rename = "Directory")]
    directory: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct PrivacySettingsDocument {
    #[serde(rename = "ShowFullName")]
    show_full_name: Option<bool>,
    #[serde(rename = "ShowEmailAddress")]
    show_email_address: Option<bool>,
}

/// `SetDefaults`' `if s.X == nil || *s.X == ""` shape — an absent key **and** an empty string both
/// take the default.
///
/// Four of the directory settings use it and the rest of the config does not, which is why it is
/// a helper rather than an `unwrap_or`: writing `unwrap_or` here would leave a document holding
/// `""` pointing the backend at the process working directory.
fn non_empty_or(value: Option<String>, default: String) -> String {
    match value {
        Some(value) if !value.is_empty() => value,
        _ => default,
    }
}

/// `ServiceSettings.SetDefaults`' rewrite of `regular` to `gzip` (config.go:845).
///
/// Go *mutates the config* rather than folding at read time, so `regular` is not a value a
/// running server ever holds. Applied to both the document and the environment overlay, since
/// `applyEnvironmentMap` runs before nothing — `SetDefaults` has already been called, so an
/// overlay of `regular` would in Go survive as `regular`. This is the one place the port
/// deliberately normalises more than Go: see [D-202].
fn normalise_webserver_mode(mode: String) -> String {
    if mode == "regular" {
        "gzip".to_owned()
    } else {
        mode
    }
}

/// `strconv.ParseBool` (Go strconv/atob.go:10) — the exact set of accepted spellings, and
/// `None` for everything else.
///
/// Go's list is closed and case-sensitive apart from the six forms below: `TRUE`, `True` and
/// `true` parse, but `tRuE` and `yes` do not. Widening it to `eq_ignore_ascii_case` would accept
/// values the Go server rejects, which is how the two configurations drift apart.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// [`parse_bool`] over an environment variable, with an absent *or* unparseable value falling
/// back to the default.
///
/// The fallback direction is Go's: viper leaves the setting at its configured default when an
/// override does not parse, so `MM_COMPLIANCESETTINGS_ENABLE=yes` is *not* true on either
/// server. Treating an unparseable value as `true` would silently diverge on a typo.
/// See [`Config::apply_env_from`] for why the lookup is a parameter rather than `std::env`.
fn lookup_bool(lookup: &impl Fn(&str) -> Option<String>, key: &str, default: bool) -> bool {
    lookup(key)
        .and_then(|raw| parse_bool(&raw))
        .unwrap_or(default)
}

/// Go's `ServiceSettingsMaxUniqueReactionsPerPost` ceiling (config.go:139-141), applied in
/// `SetDefaults` (config.go:1025) — so it is part of *loading* a document, not of using the value.
///
/// Go clamps only the upper end; a zero or negative document value is left alone and makes
/// `count >= limit` true for the first reaction, refusing every one. Reproduced rather than
/// sanitised: a server configured that way accepts no reactions on either side.
fn clamp_unique_reactions(value: i64) -> i64 {
    value.min(MAX_UNIQUE_REACTIONS_PER_POST)
}

/// `model.ServiceSettingsMaxUniqueReactionsPerPost` (config.go:141).
const MAX_UNIQUE_REACTIONS_PER_POST: i64 = 500;

/// `model.TeamSettingsLockProfileFieldsAll` (config.go:151) — the only one of the three legal
/// values of `TeamSettings.LockProfileFieldsForEmailUsers` that locks a profile *picture*.
///
/// `"name_and_username"` (config.go:150) locks the other fields and leaves the picture alone;
/// `IsProfileImageLockedForUser` compares against this constant and nothing else.
pub const TEAM_SETTINGS_LOCK_PROFILE_FIELDS_ALL: &str = "all";

/// `model.DirectMessageAny` (config.go:80).
pub const DIRECT_MESSAGE_ANY: &str = "any";
/// `model.DirectMessageTeam` (config.go:81) — the only value that restricts anything.
pub const DIRECT_MESSAGE_TEAM: &str = "team";

/// The integer arm of `applyEnvKey` (config/environment.go:64): `strconv.ParseInt(value, 10, 0)`,
/// with an unparseable value leaving the setting at its default.
///
/// Base 10 is explicit in Go, so `0x10` and `1_000` are **not** accepted — which matches
/// `i64::from_str`. The fallback direction is the same as [`lookup_bool`]'s and for the same
/// reason: Go's `if err == nil { ... }` simply never assigns.
fn lookup_int(lookup: &impl Fn(&str) -> Option<String>, key: &str, default: i64) -> i64 {
    lookup(key)
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(default)
}

/// Go's environment decoder for a `[]string` setting: split on commas, keep the pieces as
/// written. Whitespace is **not** trimmed — `"a, b"` is `["a", " b"]` in Go too — and an empty
/// string yields an empty list rather than one empty element.
fn split_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(',').map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_gos_set_defaults() {
        let config = Config::default();
        assert!(!config.restrict_system_admin, "config.go:1269 — new(false)");
        assert!(!config.compliance_enable, "config.go:2875 — new(false)");
        assert!(!config.image_proxy_enable, "config.go:3996 — new(false)");
        // `getUsersWithInvalidEmails` answers 400 when this is **on**, so a wrong default turns
        // a working route into an unconditional refusal on any server that has not set it. The
        // stack's own servers both set it in the environment, which is exactly why the default
        // needs a test of its own rather than a route to exercise it.
        assert!(!config.enable_open_server, "config.go:2588 — new(false)");
        assert!(
            !config.enable_post_icon_override,
            "config.go:848 — new(false)"
        );
        // The two that are **not** false. A port that assumed the pattern held would drop
        // `metadata.emojis` and `metadata.priority` from every post.
        assert!(config.enable_custom_emoji, "config.go:850 — new(true)");
        assert!(config.post_priority, "config.go:993 — new(true)");
        assert!(config.enable_burn_on_read, "config.go:1034 — new(true)");
        assert!(
            config.feature_flag_burn_on_read,
            "feature_flags.go:187 — f.BurnOnRead = true"
        );
        // And therefore the conjunction, which is what decides which cursor query runs.
        assert!(
            config.burn_on_read(),
            "post_helpers.go:270 — both halves true"
        );
        assert_eq!(
            config.session_idle_timeout_in_minutes, 43_200,
            "config.go:800 — new(43200), thirty days"
        );
        assert!(
            config.extend_session_length_with_activity,
            "config.go:729 — new(!isUpdate), and this constructor is the fresh case"
        );
        // The one string setting. `model.ImageDriverLocal` is the literal `"local"`; the emoji
        // reads compare it against `""`, so a default of `""` here would 403 every one of them.
        assert_eq!(
            config.file_driver_name, "local",
            "config.go:1900 — new(ImageDriverLocal)"
        );
    }

    /// Either half alone turns it off — the reason the two are separate fields.
    #[test]
    fn burn_on_read_needs_both_halves() {
        let config = Config {
            enable_burn_on_read: false,
            ..Config::default()
        };
        assert!(!config.burn_on_read(), "the setting alone disables it");

        let config = Config {
            feature_flag_burn_on_read: false,
            ..Config::default()
        };
        assert!(!config.burn_on_read(), "the flag alone disables it");
    }

    /// Exactly `strconv.ParseBool`'s twelve spellings, and nothing else.
    #[test]
    fn parse_bool_matches_go_strconv() {
        for raw in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(raw), Some(true), "{raw} should parse true");
        }
        for raw in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw} should parse false");
        }
    }

    /// The near-misses. Each of these is a value an operator plausibly writes, and Go rejects
    /// every one — so each must fall back rather than being read as true.
    #[test]
    fn parse_bool_rejects_everything_go_rejects() {
        for raw in [
            "yes", "no", "on", "off", "tRuE", "TrUe", "y", "n", "2", "-1", "", " true", "true ",
            "enabled",
        ] {
            assert_eq!(parse_bool(raw), None, "{raw:?} must not parse");
        }
    }

    /// An absent key takes the default in **both** directions — a fallback that always returned
    /// `false` would look correct against Go's defaults while ignoring its argument entirely.
    ///
    /// `env_bool` is exercised through a key nothing sets rather than by mutating the
    /// environment: the process environment is global, and writing it from one test races every
    /// other test in the binary.
    #[test]
    fn an_absent_key_takes_the_default_either_way() {
        let empty = |_: &str| None;
        assert!(lookup_bool(&empty, "MM_ANYTHING", true));
        assert!(!lookup_bool(&empty, "MM_ANYTHING", false));
    }

    /// A value that does not parse falls back too — Go's viper leaves the setting at its
    /// configured value rather than treating a typo as `true`.
    #[test]
    fn an_unparseable_value_takes_the_default_either_way() {
        let yes = |_: &str| Some("yes".to_owned());
        assert!(lookup_bool(&yes, "MM_ANYTHING", true));
        assert!(
            !lookup_bool(&yes, "MM_ANYTHING", false),
            "`yes` is not `true`"
        );
    }

    /// And a value that *does* parse wins over the default in both directions.
    #[test]
    fn a_parseable_value_overrides_the_default() {
        assert!(lookup_bool(
            &|_: &str| Some("true".to_owned()),
            "MM_ANYTHING",
            false
        ));
        assert!(!lookup_bool(
            &|_: &str| Some("false".to_owned()),
            "MM_ANYTHING",
            true
        ));
    }

    /// [`lookup_int`] against `strconv.ParseInt(value, 10, 0)`, which is the conversion
    /// `applyEnvKey` runs for an `int` field (config/environment.go:64).
    ///
    /// Base 10 is explicit on the Go side, so the hex and underscore spellings a base-0 parse
    /// would accept are **not** overrides on either server — they leave the setting alone.
    #[test]
    fn lookup_int_matches_gos_parse_int() {
        let with = |v: &'static str| lookup_int(&move |_: &str| Some(v.to_owned()), "MM_X", 43_200);

        assert_eq!(with("5"), 5);
        assert_eq!(with("0"), 0, "zero is a value, not an absence — it disarms");
        assert_eq!(with("-1"), -1);
        assert_eq!(
            with("+7"),
            7,
            "strconv accepts a leading plus and so does Rust"
        );

        for rejected in ["", " 5", "5 ", "5m", "0x10", "1_000", "1.0", "yes"] {
            assert_eq!(
                lookup_int(&|_: &str| Some(rejected.to_owned()), "MM_X", 43_200),
                43_200,
                "`{rejected}` must leave the default alone"
            );
        }

        assert_eq!(lookup_int(&|_: &str| None, "MM_X", 43_200), 43_200);
    }

    /// Both new settings reach [`Config`] through the overlay, under the variable names Go's own
    /// `MM_<SECTION>_<SETTING>` rule produces.
    #[test]
    fn the_session_settings_are_overridable_by_environment() {
        let config = Config::default().apply_env_from(&|key| match key {
            "MM_SERVICESETTINGS_SESSIONIDLETIMEOUTINMINUTES" => Some("15".to_owned()),
            "MM_SERVICESETTINGS_EXTENDSESSIONLENGTHWITHACTIVITY" => Some("false".to_owned()),
            _ => None,
        });

        assert_eq!(config.session_idle_timeout_in_minutes, 15);
        assert!(
            !config.extend_session_length_with_activity,
            "and it moved off the fresh-config default of true"
        );
    }

    /// The four settings the image routes read reach the overlay, each under its own name.
    ///
    /// An operator who raises `MaxFileSize` or names an LDAP picture attribute through the
    /// environment has overridden the Go server beside us, and a variable this port does not
    /// consult is a route answering differently on each side for a reason that has nothing to do
    /// with the code. Two of the four are also the only readers of their whole section, so a
    /// transposed name would silently keep the default rather than fail to compile.
    #[test]
    fn the_image_settings_are_overridable_by_environment() {
        let config = Config::default().apply_env_from(&|key| match key {
            "MM_FILESETTINGS_MAXFILESIZE" => Some("4096".to_owned()),
            "MM_TEAMSETTINGS_LOCKPROFILEFIELDSFOREMAILUSERS" => Some("all".to_owned()),
            "MM_LDAPSETTINGS_PICTUREATTRIBUTE" => Some("thumbnailPhoto".to_owned()),
            "MM_SAMLSETTINGS_ENABLESYNCWITHLDAP" => Some("true".to_owned()),
            _ => None,
        });

        assert_eq!(config.file_max_file_size, 4096, "down from 100 MiB");
        assert_eq!(config.lock_profile_fields_for_email_users, "all");
        assert_eq!(config.ldap_picture_attribute, "thumbnailPhoto");
        assert!(config.saml_enable_sync_with_ldap);

        // And with nothing set every one of them keeps the value the document gave it.
        let untouched = Config::default().apply_env_from(&|_| None);
        assert_eq!(untouched.file_max_file_size, 100 * 1024 * 1024);
        assert_eq!(untouched.lock_profile_fields_for_email_users, "none");
        assert_eq!(untouched.ldap_picture_attribute, "");
        assert!(!untouched.saml_enable_sync_with_ldap);
    }

    /// `MM_TEAMSETTINGS_ENABLEOPENSERVER` reaches the overlay.
    ///
    /// The Go server beside us sets this variable and nothing else does, so without the overlay
    /// the two servers disagree about `GET /api/v4/users/invalid_emails` — a 400 on one and a
    /// page of users on the other. `scripts/mm-api-env.sh` is the other half of the fix.
    #[test]
    fn the_open_server_flag_is_overridable_by_environment() {
        let config = Config::default().apply_env_from(&|key| match key {
            "MM_TEAMSETTINGS_ENABLEOPENSERVER" => Some("true".to_owned()),
            _ => None,
        });
        assert!(
            config.enable_open_server,
            "and it moved off the fresh-config default of false"
        );

        assert!(
            !Config::default()
                .apply_env_from(&|_| None)
                .enable_open_server,
            "with no variable set the default stands"
        );
    }

    /// `MM_SERVICESETTINGS_SITEURL` reaches the overlay too — which matters more than it looks,
    /// because it is the one variable the Go container beside us actually sets, and it moves both
    /// [`Config::subpath`] and (by making the value present) the `isUpdate` defaults.
    ///
    /// Added after a mutation that dropped the override survived the whole suite: nothing had ever
    /// driven this variable, and the document's value alone is indistinguishable from no overlay.
    #[test]
    fn the_site_url_is_overridable_by_environment() {
        let from_document = Config {
            site_url: Some("http://from-the-document".to_owned()),
            ..Config::default()
        };

        let overridden = from_document.clone().apply_env_from(&|key| match key {
            "MM_SERVICESETTINGS_SITEURL" => Some("https://example.com/mattermost".to_owned()),
            _ => None,
        });
        assert_eq!(
            overridden.site_url.as_deref(),
            Some("https://example.com/mattermost")
        );
        assert_eq!(
            overridden.subpath(),
            "/mattermost",
            "and the override is what the cookie is scoped to"
        );

        let untouched = from_document.apply_env_from(&|_| None);
        assert_eq!(
            untouched.site_url.as_deref(),
            Some("http://from-the-document"),
            "with nothing set, the document survives"
        );
    }
}

/// Parity tests against `fixtures/config_active.json` — the configuration a real Go server booted
/// itself on, projected to the modelled keys by `scripts/dump-config-fixture.sh`.
#[cfg(test)]
mod go_parity {
    use super::*;

    /// `AIRecapsEnabled`'s truth table, and the corner that inverts the intuition.
    ///
    /// `IsEnabled()` is `s == nil || s.Enable == nil || *s.Enable`, so an **absent** `Enable`
    /// means *enabled*. A port reaching for `unwrap_or(false)` would refuse recaps on every
    /// server that has never configured them — which is every server — and the fifteen routes
    /// behind the gate would then answer for a feature the operator had switched on.
    #[test]
    fn an_absent_recap_setting_means_enabled_not_disabled() {
        let gate = |flag: bool, enable: Option<bool>| {
            Config {
                feature_flag_enable_ai_recaps: flag,
                ai_recap_settings_enable: enable,
                ..Config::default()
            }
            .ai_recaps_enabled()
        };

        assert!(!gate(false, None), "the feature flag is off by default");
        assert!(!gate(false, Some(true)), "and it is the outer `&&`");
        assert!(
            gate(true, None),
            "an absent setting is enabled — this is the one that inverts"
        );
        assert!(gate(true, Some(true)));
        assert!(
            !gate(true, Some(false)),
            "and an explicit false still disables"
        );
    }

    /// The default configuration does **not** enable recaps, so all fifteen routes are ours until
    /// an operator sets the flag.
    #[test]
    fn recaps_are_off_on_a_stock_server() {
        assert!(!Config::default().ai_recaps_enabled());
        assert!(!Config::default().feature_flag_enable_ai_recaps);
        assert_eq!(Config::default().ai_recap_settings_enable, None);
    }

    const ACTIVE: &str = include_str!("../../../fixtures/config_active.json");

    /// **The test this whole change exists for.** Every default in [`Config::default`] was
    /// transcribed by reading `SetDefaults` in a 5,795-line Go file; this asserts each one against
    /// what a Go server actually wrote after running it.
    ///
    /// Before the config document was reachable there was no way to check the transcription at
    /// all — `defaults_match_gos_set_defaults` asserts the same values against line numbers a
    /// human read, which catches a typo in the test and nothing in the world.
    ///
    /// # Three fields are expected to differ, and none of them is a drift
    ///
    /// `Store.Load` plants a `SiteURL` of `""` before calling `SetDefaults` (config/store.go:280),
    /// so a persisted document always **has** one where [`Config::default`] — which models a
    /// config that has never been through `Load` — does not. That is the first difference, and it
    /// causes the second: `ExtendSessionLengthWithActivity` defaults to `!isUpdate` and `isUpdate`
    /// is `SiteURL != nil`, so the fresh config gets `true` and every real document `false`.
    ///
    /// The third appeared the moment the fixture stopped being a six-section projection:
    /// `AIRecapSettings.SetDefaults` writes `Enable = true` when it is nil
    /// (ai_recap_settings.go:105), so a persisted document carries `Some(true)` where the fresh
    /// config carries `None`. **They mean the same thing** — `IsEnabled()` is
    /// `s == nil || s.Enable == nil || *s.Enable`, so absent *is* enabled — which is why the field
    /// is an `Option` at all. Adjusted here rather than changed in [`Config::default`], because
    /// the default is modelling the pre-`Load` config correctly.
    ///
    /// The fourth is `ExtendSessionLengthWithActivity`'s twin.
    /// `TerminateSessionsOnPasswordChange` also defaults to `!isUpdate` (config.go:733), so it too
    /// is `true` on a config that has never been through `Load` and `false` on every document a
    /// running server persists. Note the fixture does not carry the key **at all** where it does
    /// carry `ExtendSessionLengthWithActivity: false` — an absent key and an explicit `false` reach
    /// the same place through `SetDefaults`, which is exactly why the resolution has to happen in
    /// [`Config::from_document`] rather than against [`Config::default`].
    ///
    /// All four are correct for their input, which is why this compares against the adjusted
    /// default rather than widening the assertion to let a genuine drift through.
    #[test]
    fn every_default_matches_what_go_actually_wrote() {
        let from_go = Config::from_document(ACTIVE).expect("the fixture is a config document");
        let transcribed = Config {
            site_url: Some(String::new()),
            extend_session_length_with_activity: false,
            terminate_sessions_on_password_change: false,
            ai_recap_settings_enable: Some(true),
            // The fifth adjustment, and the same reason as the two `!isUpdate` booleans above:
            // `Config::default` models a *fresh* config, where the mobile length is 30 days.
            // The live server's document is an update, so Go wrote 180 days — 4320 hours — and
            // the fixture now carries both that and the `SessionLengthMobileInDays` it was
            // derived from, so this is Go's own number rather than a transcribed one.
            session_length_mobile_in_hours: 4320,
            ..Config::default()
        };

        assert_eq!(
            from_go, transcribed,
            "a default transcribed from config.go disagrees with the one Go wrote"
        );
    }

    /// [`Config::subpath`] against Go's own answers for every corpus input.
    ///
    /// The two ingredients — `go_url::go_parse` and `go_path::clean` — each have an oracle
    /// already. This covers what neither does: the eight lines of glue between them, whose three
    /// branches all reach `"/"` or `""` from different places.
    #[test]
    fn subpath_matches_go_for_every_corpus_input() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_subpath.json"))
                .expect("behaviour_subpath.json is generated by reference/dump");
        let cases = oracle["get_subpath_from_config"]
            .as_array()
            .expect("an array of cases");
        assert!(
            cases.len() >= 25,
            "the corpus should cover the parse, empty-path and clean branches"
        );

        let mut failures = 0;
        for case in cases {
            let site_url = case["site_url"].as_str().map(str::to_owned);
            let config = Config {
                site_url: site_url.clone(),
                ..Config::default()
            };
            assert_eq!(
                config.subpath(),
                case["subpath"].as_str().expect("a subpath"),
                "GetSubpathFromConfig({site_url:?})"
            );
            if case["failed"].as_bool() == Some(true) {
                failures += 1;
            }
        }

        assert!(
            failures >= 2,
            "the corpus must contain SiteURLs Go refuses to parse, or the empty-string return \
             is never exercised"
        );
    }

    /// The three routes to `"/"` are separately reachable, so the branches cannot be collapsed
    /// without one of them moving.
    #[test]
    fn the_three_paths_to_a_root_subpath_are_distinct() {
        let with = |site_url: Option<&str>| {
            Config {
                site_url: site_url.map(str::to_owned),
                ..Config::default()
            }
            .subpath()
        };

        assert_eq!(with(None), "/", "no SiteURL at all");
        assert_eq!(with(Some("http://localhost:8065")), "/", "an empty u.Path");
        assert_eq!(with(Some("https://example.com/..")), "/", "cleaned away");
        assert_eq!(
            with(Some("http://%zz/sub")),
            "",
            "and a parse failure is the empty string, not a root"
        );
    }

    /// A real subpath deployment, which the development stack cannot produce — both its SiteURLs
    /// (the document's `""` and the container's `http://localhost:8065`) have an empty path and
    /// therefore agree on `/`. Pinned here instead.
    #[test]
    fn a_configured_subpath_is_cleaned_not_copied() {
        let config = Config {
            site_url: Some("https://example.com/mattermost/".to_owned()),
            ..Config::default()
        };
        assert_eq!(config.subpath(), "/mattermost", "the trailing slash goes");
    }

    /// The `isUpdate` rule, both directions, on the fixture and on a document without a `SiteURL`.
    ///
    /// This is the only default in the module that is computed rather than looked up, and getting
    /// it backwards would **disarm the idle-timeout check on every real server** — a session Go
    /// revokes would authenticate here indefinitely. The live document proves the arming half; a
    /// hand-built document with no `SiteURL` proves the other, since no real server writes one.
    #[test]
    fn the_extend_session_default_follows_is_update() {
        let from_go = Config::from_document(ACTIVE).expect("the fixture is a config document");
        assert!(
            !from_go.extend_session_length_with_activity,
            "a persisted document has a SiteURL, so isUpdate is true and the default is false"
        );
        assert!(
            ACTIVE.contains("\"SiteURL\""),
            "and the fixture is the evidence for that — the key must stay projected"
        );

        let fresh = Config::from_document(r#"{"ServiceSettings":{"PostPriority":true}}"#)
            .expect("valid document");
        assert!(
            fresh.extend_session_length_with_activity,
            "no SiteURL means a fresh config, and Go defaults it true there"
        );
    }

    /// `SessionLengthMobileInHours` has a **three-step** fallback, and each step is a different
    /// number a client's session length depends on. Every branch, in the order Go tries them.
    #[test]
    fn the_mobile_session_length_falls_back_through_days_then_is_update() {
        let hours = |document: &str| {
            Config::from_document(document)
                .expect("valid document")
                .session_length_mobile_in_hours
        };

        // 1. An explicit hours value wins outright, even beside a contradictory days value.
        assert_eq!(
            hours(
                r#"{"ServiceSettings":{"SiteURL":"","SessionLengthMobileInHours":12,"SessionLengthMobileInDays":99}}"#
            ),
            12
        );
        // 2. Failing that, days * 24 — the branch a port that only read the hours field misses,
        //    and the one every document written before the hours setting existed takes.
        assert_eq!(
            hours(r#"{"ServiceSettings":{"SiteURL":"","SessionLengthMobileInDays":7}}"#),
            7 * 24
        );
        // 3. Failing both, `isUpdate` chooses 180 days or 30 — 4320 hours against 720. A
        //    persisted document always has a SiteURL and therefore always takes the first.
        assert_eq!(
            hours(r#"{"ServiceSettings":{"SiteURL":""}}"#),
            180 * 24,
            "a persisted document is an update, so the mobile default is 180 days"
        );
        assert_eq!(
            hours(r#"{"ServiceSettings":{"PostPriority":true}}"#),
            30 * 24,
            "no SiteURL is a fresh install, so it is 30 days"
        );
        // The four answers must be four distinct numbers, or the assertions above are satisfied
        // by a function that ignores its input.
        assert_eq!(
            [12, 168, 4320, 720]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
    }

    /// `GetCookieDomain`'s flag arm, on the one input that matters — the flag is off by default,
    /// so a stock server sends **no** `Domain` attribute however its SiteURL is spelled. The
    /// per-URL hostnames are asserted against Go in `mm_api::sessions::cookie_domain_matches_go`.
    #[test]
    fn the_cookie_domain_is_empty_unless_subdomains_are_allowed() {
        let site_url = Some("https://mattermost.example.com:8065/sub".to_owned());
        assert_eq!(
            Config {
                site_url: site_url.clone(),
                allow_cookies_for_subdomains: false,
                ..Config::default()
            }
            .cookie_domain(),
            ""
        );
        assert_eq!(
            Config {
                site_url,
                allow_cookies_for_subdomains: true,
                ..Config::default()
            }
            .cookie_domain(),
            "mattermost.example.com"
        );
        // Default-off, which is what makes the empty answer the reachable one.
        assert!(!Config::default().allow_cookies_for_subdomains);
    }

    /// `url.Hostname()` over Go's own answers for every raw `URL.Host` shape in the corpus.
    ///
    /// The two rules a naive split on the last colon gets wrong are both here: the port must be
    /// **numeric** to be a port (`example.com:https` keeps its whole authority), and the bracket
    /// strip runs **after** the split, so an unclosed IPv6 literal is cut inside
    /// (`[::1` is `[:`). The second was asserted here by hand at the wrong value before this
    /// corpus existed, which is the argument for the corpus.
    #[test]
    fn the_hostname_split_matches_go() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_session_write.json"
        ))
        .expect("the generated oracle parses");
        let cases = oracle["split_host_port"]
            .as_object()
            .expect("the section is an object");
        assert!(cases.len() >= 15, "the corpus shrank");

        for (host, want) in cases {
            assert_eq!(
                go_hostname(host.as_bytes()),
                want.as_str().expect("a string"),
                "Hostname() for Host={host:?}"
            );
        }

        // The corpus must contain at least one row where the input survives whole, one where a
        // port comes off and one where brackets do — otherwise the loop is satisfied by an
        // identity function or by a truncation.
        assert_eq!(cases["example.com"], "example.com");
        assert_eq!(cases["example.com:8065"], "example.com");
        assert_eq!(cases["[::1]:8065"], "::1");
    }

    /// Presence decides it, not truthiness. An empty `SiteURL` is a non-nil pointer in Go and it
    /// is exactly what the live row holds, so reading `""` as "unset" would invert the default on
    /// every stock server.
    #[test]
    fn an_empty_site_url_still_counts_as_an_update() {
        let config =
            Config::from_document(r#"{"ServiceSettings":{"SiteURL":""}}"#).expect("valid document");
        assert!(!config.extend_session_length_with_activity);
    }

    /// An explicit value in the document beats the computed default in both directions — the
    /// field is read, not merely defaulted.
    #[test]
    fn an_explicit_extend_session_value_wins_over_is_update() {
        let on = Config::from_document(
            r#"{"ServiceSettings":{"SiteURL":"x","ExtendSessionLengthWithActivity":true}}"#,
        )
        .expect("valid document");
        assert!(on.extend_session_length_with_activity);

        let off = Config::from_document(
            r#"{"ServiceSettings":{"ExtendSessionLengthWithActivity":false}}"#,
        )
        .expect("valid document");
        assert!(!off.extend_session_length_with_activity);
    }

    /// The idle timeout is an integer, so it is the one setting where the *value* of the default
    /// matters rather than just its polarity. 43200 minutes is thirty days.
    #[test]
    fn the_idle_timeout_default_is_thirty_days() {
        let from_go = Config::from_document(ACTIVE).expect("the fixture is a config document");
        assert_eq!(from_go.session_idle_timeout_in_minutes, 43_200);
        assert_eq!(
            Config::from_document("{}")
                .expect("valid document")
                .session_idle_timeout_in_minutes,
            43_200,
            "an absent key takes Go's default, not zero — zero would disarm the check"
        );
    }

    /// The fixture covers **every** setting read from the document.
    ///
    /// Without this the coverage rots silently. `scripts/dump-config-fixture.sh` carries its own
    /// copy of the key list, so a field added to [`Config`] but not to the script is simply absent
    /// from the fixture — and then `every_default_matches_what_go_actually_wrote` compares its
    /// *default* against its default and passes, having proved nothing about the new field. The
    /// count is the cheapest thing that fails instead.
    ///
    /// **The count was 17 and the script's key list had drifted to match it.** `Document` had
    /// grown to fourteen sections and thirty-eight keys while the projection still carried six
    /// sections and seventeen, so eight sections of Go's own output were never compared against
    /// anything and this assertion agreed with the omission. The list in the script is now the
    /// struct's keys, and the number below is what the script writes: **40** — the thirty-eight
    /// modelled keys plus `ServiceSettings.SiteURL`, projected for its presence rather than its
    /// value, and counted here like any other.
    ///
    /// **It drifted again, the same way, and the count is why.** The four image-route settings —
    /// `FileSettings.MaxFileSize`, `TeamSettings.LockProfileFieldsForEmailUsers`,
    /// `LdapSettings.PictureAttribute` and `SamlSettings.EnableSyncWithLdap` — were added to
    /// [`Config`] with their defaults transcribed from `config.go` and *not* added to the
    /// script, so [`every_default_matches_what_go_actually_wrote`] compared four defaults against
    /// their own fallback and passed. A hardcoded number cannot notice a missing key on its own;
    /// what it can do is fail the moment somebody adds the key, which is what happened here.
    #[test]
    fn the_fixture_covers_every_document_sourced_setting() {
        let fixture: serde_json::Value = serde_json::from_str(ACTIVE).expect("the fixture is JSON");
        let keys: usize = fixture
            .as_object()
            .expect("an object of sections")
            .values()
            .map(|section| section.as_object().expect("a section of settings").len())
            .sum();

        assert_eq!(
            keys, 53,
            "the fixture covers {keys} settings and Config reads 53 from the document. \
             Add the new key to scripts/dump-config-fixture.sh and re-run it — a modelled \
             setting the fixture does not carry is a setting Go's own output never checked"
        );
    }

    /// The document supplies values; it does not merely fail to override defaults. Flipping every
    /// modelled boolean away from its default proves each field is genuinely read — without this,
    /// a `from_document` that ignored its argument entirely would pass the test above.
    #[test]
    fn every_field_is_actually_read_from_the_document() {
        let inverted = r#"{
            "ServiceSettings": {
                "EnablePostIconOverride": true,
                "EnableCustomEmoji": false,
                "PostPriority": false,
                "AllowSyncedDrafts": false,
                "EnableBurnOnRead": false,
                "EnableIncomingWebhooks": false,
                "EnableOutgoingWebhooks": false,
                "EnableOAuthServiceProvider": false,
                "SessionIdleTimeoutInMinutes": 17,
                "ExtendSessionLengthWithActivity": true
            },
            "ComplianceSettings": { "Enable": true },
            "ExperimentalSettings": { "RestrictSystemAdmin": true },
            "ImageProxySettings": { "Enable": true },
            "FileSettings": { "DriverName": "amazons3", "MaxFileSize": 4096 },
            "TeamSettings": { "LockProfileFieldsForEmailUsers": "all" },
            "LdapSettings": { "PictureAttribute": "thumbnailPhoto" },
            "SamlSettings": { "EnableSyncWithLdap": true },
            "PrivacySettings": { "ShowFullName": false, "ShowEmailAddress": false }
        }"#;
        let config = Config::from_document(inverted).expect("valid document");

        assert!(config.enable_post_icon_override);
        assert!(!config.enable_custom_emoji);
        assert!(!config.post_priority);
        assert!(!config.allow_synced_drafts);
        assert!(!config.enable_burn_on_read);
        assert!(!config.enable_incoming_webhooks);
        assert!(!config.enable_outgoing_webhooks);
        assert!(!config.enable_oauth_service_provider);
        assert!(config.compliance_enable);
        assert!(config.restrict_system_admin);
        assert!(config.image_proxy_enable);
        assert_eq!(config.file_driver_name, "amazons3");
        // The four the image routes read. Each is in its own section, and two of those sections
        // exist in `Document` for one key each — so a wiring that dropped either would fall back
        // to a default that every other test in this module is happy with.
        assert_eq!(config.file_max_file_size, 4096);
        assert_eq!(config.lock_profile_fields_for_email_users, "all");
        assert_eq!(config.ldap_picture_attribute, "thumbnailPhoto");
        assert!(config.saml_enable_sync_with_ldap);
        assert!(!config.show_full_name);
        assert!(!config.show_email_address);
        assert_eq!(config.session_idle_timeout_in_minutes, 17);
        // Inverted against the *fresh* default, which this document has too: no `SiteURL`, so
        // `!isUpdate` is `true` and an unread field would also read `true`. The 17 above is what
        // makes the pair honest — an integer has no default to coincide with.
        assert!(config.extend_session_length_with_activity);
    }

    /// The two privacy settings are read from **different** keys.
    ///
    /// Both default to `true` and the inversion test above sets both to `false`, so a port that
    /// wired `ShowFullName` into both fields would pass every other test in this module. They are
    /// separated here because they are also separated on the wire: `Sanitize` consults them
    /// independently, and a caller who may see names but not emails is an ordinary configuration.
    #[test]
    fn the_two_privacy_settings_are_not_the_same_key() {
        let config = Config::from_document(
            r#"{"PrivacySettings":{"ShowFullName":true,"ShowEmailAddress":false}}"#,
        )
        .expect("valid document");
        assert!(config.show_full_name, "names are shown");
        assert!(!config.show_email_address, "emails are not");

        let swapped = Config::from_document(
            r#"{"PrivacySettings":{"ShowFullName":false,"ShowEmailAddress":true}}"#,
        )
        .expect("valid document");
        assert!(!swapped.show_full_name);
        assert!(swapped.show_email_address);
    }

    /// The same asymmetry for the two webhook toggles, which are likewise both `true` by default
    /// and carry *different* error ids on the routes they gate.
    #[test]
    fn the_two_webhook_toggles_are_not_the_same_key() {
        let config = Config::from_document(
            r#"{"ServiceSettings":{"EnableIncomingWebhooks":false,"EnableOutgoingWebhooks":true}}"#,
        )
        .expect("valid document");
        assert!(!config.enable_incoming_webhooks);
        assert!(config.enable_outgoing_webhooks);
    }

    /// **The trap this module is shaped to avoid.** An absent key is Go's *default*, not the zero
    /// value: `config.go` makes every setting a pointer and `SetDefaults` fills the nil ones. A
    /// `#[serde(default)]` on these bools would read `{}` as eight features switched off.
    #[test]
    fn an_absent_section_takes_gos_default_not_the_zero_value() {
        let config = Config::from_document("{}").expect("an empty object is a partial config");
        assert_eq!(
            config,
            Config::default(),
            "an empty document is all defaults"
        );

        // Named individually, because these are the ones where default and zero disagree.
        assert!(config.enable_custom_emoji);
        assert!(config.post_priority);
        assert!(config.allow_synced_drafts);
        assert!(config.enable_burn_on_read);
        assert!(config.enable_incoming_webhooks);
        assert!(config.enable_outgoing_webhooks);
        assert!(config.enable_oauth_service_provider);
        assert!(config.show_full_name);
        assert!(config.show_email_address);
        assert_eq!(config.file_driver_name, "local");
    }

    /// A present section with an absent key is the same as an absent section — `SetDefaults` walks
    /// the whole struct, not only the sections the document mentioned.
    #[test]
    fn a_half_filled_section_defaults_the_rest() {
        let config = Config::from_document(r#"{"ServiceSettings":{"PostPriority":false}}"#)
            .expect("valid document");
        assert!(!config.post_priority, "the key that was present");
        assert!(
            config.enable_custom_emoji,
            "its neighbour keeps Go's default"
        );
    }

    /// `null` unmarshals to a nil pointer in Go, which `SetDefaults` then fills — so it is the
    /// same as absent, and specifically *not* `false`.
    #[test]
    fn an_explicit_null_is_the_default_not_false() {
        let config = Config::from_document(r#"{"ServiceSettings":{"EnableCustomEmoji":null}}"#)
            .expect("null is a nil pointer, not a parse failure");
        assert!(config.enable_custom_emoji);
    }

    /// `FeatureFlags` is never persisted (config/store.go:306-310), so it must not be sourced from
    /// the document even when something puts one there — a stray section must not be able to turn
    /// a flag off. `scripts/dump-config-fixture.sh` fails loudly if the live row ever grows one.
    #[test]
    fn feature_flags_are_not_read_from_the_document() {
        let config = Config::from_document(r#"{"FeatureFlags":{"BurnOnRead":false}}"#)
            .expect("valid document");
        assert!(
            config.feature_flag_burn_on_read,
            "the flag comes from the environment or the compiled-in default, never the row"
        );
    }

    /// The flag is not the setting. `isBurnOnReadEnabled` ands the two (app/post_helpers.go:270),
    /// which is why they are separate fields — and why sourcing the flag from
    /// `ServiceSettings.EnableBurnOnRead`, the nearest plausible confusion, has to be visible.
    #[test]
    fn the_burn_on_read_flag_is_not_the_burn_on_read_setting() {
        let config = Config::from_document(r#"{"ServiceSettings":{"EnableBurnOnRead":false}}"#)
            .expect("valid document");
        assert!(
            !config.enable_burn_on_read,
            "the setting is read from the row"
        );
        assert!(
            config.feature_flag_burn_on_read,
            "the flag is not, and must not follow it"
        );
        assert!(
            !config.burn_on_read(),
            "either half alone still disables it"
        );
    }

    /// The document holds all 47 sections and this struct models six of them. Ignoring the rest is
    /// what makes growing the struct one reader at a time safe.
    ///
    /// `SiteURL` stopped being an unknown key when the `isUpdate` rule landed, which is why the
    /// expectation carries the **two** fields it moves — see
    /// [`the_extend_session_default_follows_is_update`]. Both default to `!isUpdate`, so naming
    /// one and not the other is how this test failed when the second landed.
    #[test]
    fn unknown_sections_and_keys_are_ignored() {
        let config = Config::from_document(
            r#"{"SqlSettings":{"DataSource":"secret"},"ServiceSettings":{"SiteURL":"x"}}"#,
        )
        .expect("valid document");
        assert_eq!(
            config,
            Config {
                site_url: Some("x".to_owned()),
                extend_session_length_with_activity: false,
                terminate_sessions_on_password_change: false,
                // Also `!isUpdate`-shaped: a document with a `SiteURL` is an update, so the
                // mobile session length defaults to 180 days rather than 30.
                session_length_mobile_in_hours: 4320,
                ..Config::default()
            }
        );
    }

    /// Go refuses to start on a configuration it cannot parse (`HumanizeJSONError`,
    /// config/store.go:261) and so do we — see the `?` in `main.rs`.
    #[test]
    fn a_malformed_document_is_an_error() {
        let err = Config::from_document("{not json").expect_err("must not be accepted");
        assert!(matches!(err, ConfigError::Malformed { .. }));
    }

    /// A [`mm_store::ConfigStore`] that answers with whatever it was built on, so [`Config::load`]
    /// can be tested without a database.
    struct FakeStore(Option<String>);

    impl mm_store::ConfigStore for FakeStore {
        async fn load_active(&self) -> Result<Option<String>, mm_store::StoreError> {
            Ok(self.0.clone())
        }
    }

    /// `load` reads the document. Without this, every assertion in this module could hold while
    /// `load` ignored its store and returned `Config::default().apply_env()` — the tests above all
    /// call `from_document` directly, and the wiring between the two is exactly what a reader
    /// would get wrong.
    #[tokio::test]
    async fn load_takes_its_values_from_the_document() {
        let store = FakeStore(Some(
            r#"{"ComplianceSettings":{"Enable":true},"ServiceSettings":{"PostPriority":false}}"#
                .to_owned(),
        ));
        let config = Config::load(&store).await.expect("loads");

        assert!(
            config.compliance_enable,
            "read from the document, not defaulted"
        );
        assert!(!config.post_priority, "and so is this one");
        assert!(
            config.enable_custom_emoji,
            "while the rest keep Go's defaults"
        );
    }

    /// An absent document is not an error — Go answers a missing row with a marshalled default
    /// config (config/database.go:232) rather than refusing to boot.
    #[tokio::test]
    async fn load_falls_back_to_defaults_when_no_row_is_active() {
        let config = Config::load(&FakeStore(None)).await.expect("loads");
        assert_eq!(config, Config::default().apply_env());
    }

    /// A malformed document *is* an error, and it reaches the caller rather than being swallowed
    /// into defaults. `main.rs` turns this into a refusal to start.
    #[tokio::test]
    async fn load_propagates_a_malformed_document() {
        let store = FakeStore(Some("{not json".to_owned()));
        let err = Config::load(&store)
            .await
            .expect_err("must not be accepted");
        assert!(
            matches!(err, ConfigError::Malformed { .. }),
            "a config we cannot parse must not silently become the defaults"
        );
    }

    /// **The overlay actually overrides the document.** Both survivors of the first mutation run
    /// lived here: with no `MM_` variable set — which is every test process — `apply_env` is
    /// indistinguishable from doing nothing, so deleting it from `load` changed no observable
    /// behaviour. A fake environment is the fixture that was missing.
    #[tokio::test]
    async fn load_lets_the_environment_override_the_document() {
        // The document says compliance is ON and custom emoji are OFF; the environment says the
        // reverse of each. Both directions, so an overlay that only ever forced `true` — or only
        // ever forced `false` — is still visible.
        let store = FakeStore(Some(
            r#"{"ComplianceSettings":{"Enable":true},"ServiceSettings":{"EnableCustomEmoji":false}}"#
                .to_owned(),
        ));
        let env = |key: &str| match key {
            "MM_COMPLIANCESETTINGS_ENABLE" => Some("false".to_owned()),
            "MM_SERVICESETTINGS_ENABLECUSTOMEMOJI" => Some("true".to_owned()),
            _ => None,
        };

        let config = Config::load_with_env(&store, &env).await.expect("loads");

        assert!(
            !config.compliance_enable,
            "the environment must win over the document, and it must be able to turn a setting off"
        );
        assert!(
            config.enable_custom_emoji,
            "and to turn one on — an overlay applied in the wrong order would leave this false"
        );
    }

    /// The environment does not reach settings it does not name, even when it names others. Guards
    /// against an overlay that resets the whole config to defaults whenever any variable is set.
    #[tokio::test]
    async fn the_environment_only_moves_what_it_names() {
        let store = FakeStore(Some(
            r#"{"ComplianceSettings":{"Enable":true},"ServiceSettings":{"PostPriority":false}}"#
                .to_owned(),
        ));
        let env = |key: &str| match key {
            "MM_COMPLIANCESETTINGS_ENABLE" => Some("false".to_owned()),
            _ => None,
        };

        let config = Config::load_with_env(&store, &env).await.expect("loads");

        assert!(!config.compliance_enable, "named, so it moved");
        assert!(
            !config.post_priority,
            "not named, so the document's value survives — not the default of true"
        );
    }

    /// The string setting takes the same route, and an override of `""` is a deliberate empty
    /// driver rather than an unparseable value to fall back from.
    #[tokio::test]
    async fn an_empty_string_override_survives_as_one() {
        let store = FakeStore(Some(
            r#"{"FileSettings":{"DriverName":"amazons3"}}"#.to_owned(),
        ));
        let env = |key: &str| match key {
            "MM_FILESETTINGS_DRIVERNAME" => Some(String::new()),
            _ => None,
        };

        let config = Config::load_with_env(&store, &env).await.expect("loads");

        assert_eq!(
            config.file_driver_name, "",
            "an empty driver is what the emoji reads 403 on; it must not fall back to the document"
        );
    }

    /// The overlay layers over the document rather than replacing it: a field the environment does
    /// not name keeps the document's value. Exercised with an environment that sets nothing, which
    /// is the only way to read the environment without racing every other test in the binary.
    #[test]
    fn the_env_overlay_preserves_document_values_it_does_not_name() {
        let from_doc = Config::from_document(r#"{"ComplianceSettings":{"Enable":true}}"#)
            .expect("valid document");
        let overlaid = from_doc.clone().apply_env();
        assert_eq!(
            overlaid, from_doc,
            "no MM_ variable is set in the test environment, so nothing may move"
        );
        assert!(
            overlaid.compliance_enable,
            "and the document's value survives"
        );
    }

    /// The eight file-storage settings, read from a document that sets all of them.
    #[test]
    fn the_file_storage_settings_are_read_from_the_document() {
        let config = Config::from_document(
            r#"{
                "ServiceSettings": {"WebserverMode": "disabled"},
                "FileSettings": {
                    "DriverName": "local",
                    "Directory": "/srv/mm/data/",
                    "PublicLinkSalt": "saltysaltysaltysaltysaltysaltysa",
                    "DedicatedExportStore": true,
                    "ExportDriverName": "amazons3",
                    "ExportDirectory": "/srv/mm/exportroot/"
                },
                "ExportSettings": {"Directory": "exports"},
                "ImportSettings": {"Directory": "imports"}
            }"#,
        )
        .expect("valid document");

        assert_eq!(config.file_directory, "/srv/mm/data/");
        assert_eq!(config.public_link_salt, "saltysaltysaltysaltysaltysaltysa");
        assert!(config.dedicated_export_store);
        assert_eq!(config.file_export_driver_name, "amazons3");
        assert_eq!(config.file_export_directory, "/srv/mm/exportroot/");
        assert_eq!(config.export_directory, "exports");
        assert_eq!(config.import_directory, "imports");
        assert_eq!(config.webserver_mode, "disabled");
    }

    /// A document that omits them takes Go's defaults — and the three directories are three
    /// *different* defaults, which is the thing a reader is most likely to get wrong.
    #[test]
    fn the_file_storage_defaults_are_three_different_directories() {
        let config = Config::from_document("{}").expect("valid document");
        assert_eq!(config.file_directory, "./data/");
        assert_eq!(config.file_export_directory, "./data/");
        assert_eq!(config.export_directory, "./export");
        assert_eq!(config.import_directory, "./import");
        assert_eq!(config.webserver_mode, "gzip");
        assert!(config.public_link_salt.is_empty());
        assert!(!config.dedicated_export_store);
        assert_eq!(config.file_export_driver_name, "local");
    }

    /// `SetDefaults` replaces an **empty** directory with the default, not only a missing one —
    /// so `""` must not reach the backend as "the process working directory".
    #[test]
    fn an_empty_directory_takes_the_default_rather_than_the_working_directory() {
        let config = Config::from_document(
            r#"{
                "FileSettings": {"Directory": "", "ExportDirectory": ""},
                "ExportSettings": {"Directory": ""},
                "ImportSettings": {"Directory": ""}
            }"#,
        )
        .expect("valid document");
        assert_eq!(config.file_directory, "./data/");
        assert_eq!(config.file_export_directory, "./data/");
        assert_eq!(config.export_directory, "./export");
        assert_eq!(config.import_directory, "./import");
    }

    /// `regular` is rewritten to `gzip` on load, so it is not a value a running server holds.
    /// Every other spelling — including one Go has never heard of — survives untouched.
    #[test]
    fn webserver_mode_regular_is_rewritten_to_gzip() {
        for (document, want) in [
            (r#"{"ServiceSettings":{"WebserverMode":"regular"}}"#, "gzip"),
            (r#"{"ServiceSettings":{"WebserverMode":"gzip"}}"#, "gzip"),
            (
                r#"{"ServiceSettings":{"WebserverMode":"disabled"}}"#,
                "disabled",
            ),
            (
                r#"{"ServiceSettings":{"WebserverMode":"regularly"}}"#,
                "regularly",
            ),
        ] {
            assert_eq!(
                Config::from_document(document)
                    .expect("valid document")
                    .webserver_mode,
                want,
                "{document}"
            );
        }
    }

    /// The environment overlay reaches all eight, and normalises `regular` on the way through.
    #[test]
    fn the_file_storage_settings_are_overridable_by_environment() {
        let env = std::collections::HashMap::from([
            ("MM_FILESETTINGS_DIRECTORY", "/env/data"),
            ("MM_FILESETTINGS_PUBLICLINKSALT", "envsalt"),
            ("MM_FILESETTINGS_DEDICATEDEXPORTSTORE", "true"),
            ("MM_FILESETTINGS_EXPORTDRIVERNAME", "azureblob"),
            ("MM_FILESETTINGS_EXPORTDIRECTORY", "/env/exportroot"),
            ("MM_EXPORTSETTINGS_DIRECTORY", "/env/export"),
            ("MM_IMPORTSETTINGS_DIRECTORY", "/env/import"),
            ("MM_SERVICESETTINGS_WEBSERVERMODE", "regular"),
        ]);
        let config = Config::default().apply_env_from(&|key| env.get(key).map(|v| (*v).to_owned()));

        assert_eq!(config.file_directory, "/env/data");
        assert_eq!(config.public_link_salt, "envsalt");
        assert!(config.dedicated_export_store);
        assert_eq!(config.file_export_driver_name, "azureblob");
        assert_eq!(config.file_export_directory, "/env/exportroot");
        assert_eq!(config.export_directory, "/env/export");
        assert_eq!(config.import_directory, "/env/import");
        assert_eq!(config.webserver_mode, "gzip");
    }
}

// =================================================================================================
// The **whole** `model.Config` document
//
// Everything above this line is the narrow projection a permission check reads. The three
// `/api/v4/config` reads need the document itself, so what follows works on
// [`mm_model::config::Config`] — the full 53-struct wire port — rather than on [`Config`].
//
// The two are deliberately not merged. [`Config`] is loaded **once at boot** and is what a
// permission gate consults; the functions below re-read the row on **every request**, because the
// Go server beside us owns `PUT /api/v4/config` and a cached answer would report a configuration
// that is no longer in force. That is a divergence from Go, which serves `/config` out of memory
// and refreshes it from a config listener: a client that writes through Go and reads through us
// sees its own write, where against Go alone it might briefly not. Fresher, not staler — but it
// is a difference, and it costs one small `SELECT` per request.
// =================================================================================================

/// Port of `config.GetEnvironment` (config/environment.go:16).
///
/// **The prefix is `MM`, not `MM_`.** `MMRS_STACK` is therefore in this map, as is anything else
/// beginning with those two letters; `applyEnvironmentMap` trims a leading `MM_` and simply finds
/// no field for what is left. Narrowing the filter to `MM_` would look tidier and would change
/// which keys [`generate_environment_map`] reports.
pub fn get_environment() -> std::collections::BTreeMap<String, String> {
    std::env::vars()
        .map(|(key, value)| (key.to_uppercase(), value))
        .filter(|(key, _)| key.starts_with("MM"))
        .collect()
}

/// The `map[string]…` fields of `model.Config` (config.go:3609-3610).
///
/// `applyEnvKey` switches on `reflect.Kind`, and a Go map and a Go struct are **both** a JSON
/// object — so a port that walks the decoded document cannot tell them apart from the value
/// alone. Go treats a map as a leaf and `json.Unmarshal`s the whole variable into it, ignoring
/// any remaining key parts; it treats a struct as a level to descend into. Getting this backwards
/// would let `MM_PLUGINSETTINGS_PLUGINS_ANYTHING=…` silently replace the plugin map.
const MAP_VALUED_PATHS: &[&str] = &["PluginSettings.Plugins", "PluginSettings.PluginStates"];

/// The `json.RawMessage` fields of `model.Config` (config.go:1621 and :1712).
///
/// Go's `applyEnvKey` special-cases them *before* its slice arm and assigns the raw bytes
/// (environment.go:74). They are neither pointers nor structs, so — unlike every other field —
/// a `null` in the document does **not** stop the overlay from setting them, which is why this is
/// checked ahead of the nil-pointer arm rather than inside the match.
const RAW_MESSAGE_PATHS: &[&str] = &[
    "LogSettings.AdvancedLoggingJSON",
    "ExperimentalAuditSettings.AdvancedLoggingJSON",
];

/// Port of `config.applyEnvironmentMap` (config/environment.go:89), over the decoded document.
///
/// Go reflects over `*model.Config`; this walks the same shape as JSON, which is equivalent
/// because `model.Config` carries no `json:` tags — every key *is* the Go field name. See
/// [`MAP_VALUED_PATHS`] for the one place the two representations genuinely differ.
pub fn apply_environment_map(
    config: &mut serde_json::Value,
    env: &std::collections::BTreeMap<String, String>,
) {
    for (key, value) in env {
        apply_env_key(key.strip_prefix("MM_").unwrap_or(key), value, config, "");
    }
}

/// Port of `config.applyEnvKey` (config/environment.go:28).
///
/// `path` is the dotted position of `subject` in the document and exists only to recognise the
/// two field kinds JSON cannot express; it is not part of Go's algorithm.
fn apply_env_key(key: &str, value: &str, subject: &mut serde_json::Value, path: &str) {
    // `strings.SplitN(key, "_", 2)`: the first segment names a field, the remainder is whatever
    // is left for a deeper level. Go's `len(keyParts) < 1` guard is unreachable — SplitN never
    // returns an empty slice — so there is nothing to port from it.
    let (head, rest) = match key.split_once('_') {
        Some((head, rest)) => (head, Some(rest)),
        None => (key, None),
    };

    let Some(object) = subject.as_object_mut() else {
        return;
    };
    // `FieldByNameFunc(candidate => strings.ToUpper(candidate) == keyParts[0])`. The environment
    // key arrived upper-cased from `GetEnvironment`, so this is a case-insensitive match against
    // the field name and **not** an exact one: `MM_SERVICESETTINGS_SITEURL` finds `SiteURL`.
    let Some(field) = object
        .keys()
        .find(|candidate| candidate.to_uppercase() == head)
        .cloned()
    else {
        return;
    };
    let child_path = if path.is_empty() {
        field.clone()
    } else {
        format!("{path}.{field}")
    };
    let Some(child) = object.get_mut(&field) else {
        return;
    };

    if RAW_MESSAGE_PATHS.contains(&child_path.as_str())
        || MAP_VALUED_PATHS.contains(&child_path.as_str())
    {
        // `json.Unmarshal([]byte(value), target)` for a map; a raw assignment of the bytes for a
        // `json.RawMessage`. Both are "the variable is the value, parsed as JSON", and both leave
        // the field alone when it does not parse — Go's map arm by its `if err == nil`, and the
        // raw-message arm because storing non-JSON bytes there would only produce a document that
        // cannot be marshalled again.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value) {
            *child = parsed;
        }
        return;
    }

    match child {
        // `rFieldValue.Elem()` on a nil pointer is invalid and Go returns. **The environment
        // cannot bring a nil setting to life**, which is why the overlay has to run after
        // `SetDefaults` rather than instead of it.
        serde_json::Value::Null => {}
        serde_json::Value::String(existing) => *existing = value.to_owned(),
        serde_json::Value::Bool(existing) => {
            if let Some(parsed) = parse_bool(value) {
                *existing = parsed;
            }
        }
        serde_json::Value::Number(_) => {
            if let Ok(parsed) = value.parse::<i64>() {
                *child = serde_json::Value::from(parsed);
            }
        }
        // `strings.Split(value, " ")` — **spaces, not commas**, and unlike [`split_list`] an
        // empty variable yields one empty element rather than an empty list, because that is what
        // `strings.Split("", " ")` returns.
        serde_json::Value::Array(_) => {
            *child = serde_json::Value::Array(
                value
                    .split(' ')
                    .map(|piece| serde_json::Value::String(piece.to_owned()))
                    .collect(),
            );
        }
        serde_json::Value::Object(_) => {
            // "If we have only one part left, we can't deal with a struct" (environment.go:52).
            if let Some(rest) = rest {
                apply_env_key(rest, value, child, &child_path);
            }
        }
    }
}

/// Port of `config.generateEnvironmentMap` (config/environment.go:99) — the body of
/// `GET /api/v4/config/environment`.
///
/// A nested `map[string]any` mirroring `model.Config`, carrying `true` at each leaf whose
/// variable is set and **omitting** every section that ends up empty. The shape is taken from a
/// default [`mm_model::config::Config`] rather than from a type reflection, which agrees with Go
/// everywhere except a section field that is a *value* struct in Go and an `Option` here: Go
/// would descend into it, this treats it as a leaf. No such field is settable by a variable any
/// deployment sets, and the section list is the assertion in the parity suite.
///
/// **`FeatureFlags` never appears**, on either server. Go's walk sees `*FeatureFlags`, a pointer,
/// and pointers are leaves — so it looks for a variable literally named `MM_FEATUREFLAGS` and
/// never for `MM_FEATUREFLAGS_<FLAG>`. Here the field is skipped when serialising a default
/// config, which lands in the same place. So `MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD`
/// **is** applied by [`apply_environment_map`] and is **not** reported by this — which looks like
/// an inconsistency and is Go's behaviour exactly.
pub fn generate_environment_map(
    env: &std::collections::BTreeMap<String, String>,
) -> serde_json::Value {
    let shape = serde_json::to_value(mm_model::config::Config::default())
        .unwrap_or(serde_json::Value::Null);
    environment_map_for(env, &shape, "MM")
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
}

/// Port of `config.generateEnvironmentMapWithBaseKey` (config/environment.go:104). `None` is
/// Go's `nil`, which the caller drops rather than storing as an empty object.
fn environment_map_for(
    env: &std::collections::BTreeMap<String, String>,
    shape: &serde_json::Value,
    base: &str,
) -> Option<serde_json::Value> {
    let object = shape.as_object()?;
    let mut out = serde_json::Map::new();
    for (field, value) in object {
        let key = format!("{base}_{field}");
        if value.is_object() {
            if let Some(nested) = environment_map_for(env, value, &key) {
                out.insert(field.clone(), nested);
            }
        } else if env.contains_key(&key.to_uppercase()) {
            out.insert(field.clone(), serde_json::Value::Bool(true));
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(out))
}

/// Load the configuration the Go server is running on, as the whole `model.Config`.
///
/// Port of the `Store.Load` sequence (config/store.go:260 → :285 → :292) for a reader:
/// unmarshal the document, fill `FeatureFlags` — the one section `SetDefaults` has to supply,
/// because Go strips it before persisting (store.go:306) — then overlay the environment.
///
/// `SetDefaults` is otherwise **not** run, and does not need to be: the row Go persists has
/// already been through it, so every pointer in it is non-nil. A row written by something else
/// would come back with nulls where Go would have defaults, and this would report them
/// faithfully rather than inventing values it cannot verify.
pub async fn load_model_config(
    store: &impl mm_store::ConfigStore,
) -> Result<mm_model::config::Config, ConfigError> {
    load_model_config_with_env(store, &get_environment()).await
}

/// [`load_model_config`] against an arbitrary environment, so the composition is testable: the
/// process environment is global and a test that sets one races every other test in the binary.
pub async fn load_model_config_with_env(
    store: &impl mm_store::ConfigStore,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<mm_model::config::Config, ConfigError> {
    let document = store.load_active().await?;
    let mut value: serde_json::Value = match document.as_deref() {
        Some(raw) => {
            serde_json::from_str(raw).map_err(|source| ConfigError::Malformed { source })?
        }
        None => serde_json::Value::Object(serde_json::Map::new()),
    };

    if let Some(object) = value.as_object_mut() {
        // `Config.SetDefaults` (config.go:4297): a nil `FeatureFlags` gets a fresh struct with
        // `SetDefaults` applied. Absent **and** explicitly null both count, because Go's nil
        // check cannot tell them apart.
        if !object
            .get("FeatureFlags")
            .is_some_and(|flags| !flags.is_null())
        {
            let mut flags = mm_model::feature_flags::FeatureFlags::default();
            flags.set_defaults();
            if let Ok(encoded) = serde_json::to_value(&flags) {
                object.insert("FeatureFlags".to_owned(), encoded);
            }
        }
    }

    apply_environment_map(&mut value, env);

    serde_json::from_value(value).map_err(|source| ConfigError::Malformed { source })
}

/// Port of `(*model.Config).Sanitize(nil, nil)` (config.go:5346) — what `GET /api/v4/config`
/// returns instead of the secrets.
///
/// **This is the function that keeps the database password out of a response and out of a
/// fixture.** Every branch below is Go's, in Go's order, including the two distinctions that look
/// like oversights and are not:
///
/// - Most fields are replaced only when **non-empty**, so an unset SMTP password stays `""` and
///   does not acquire a fake one. Five are replaced unconditionally — `PublicLinkSalt`,
///   `SqlSettings.DataSource`, `AtRestEncryptKey`, `ElasticsearchSettings.Password` and
///   `ServiceSettings.SplitKey` — so those read `FakeSetting` even on a server that has none.
/// - `opts.PartiallyRedactDataSources` is Go's only alternative for the data sources and is
///   `false` here: `getConfig` passes `nil` options (api4/config.go:62 reaches `Sanitize` through
///   `App.SanitizedConfig`). So a data source is the full `FakeSetting`, not a redacted URL.
///
/// `PluginSettings.Sanitize(manifests)` is the last step in Go and is **not** ported: with no
/// plugin host there are no manifests, and Go's no-manifest path deletes every plugin that has
/// stored settings from the map. The live document's `Plugins` is `{}`, so the two agree today;
/// a server with configured plugin settings would not. See [D-311].
pub fn sanitize(config: &mut mm_model::config::Config) {
    use mm_model::utils::FAKE_SETTING;

    /// `if p != nil && *p != "" { *p = FakeSetting }`.
    fn mask_if_set(field: &mut Option<String>) {
        if field.as_deref().is_some_and(|value| !value.is_empty()) {
            *field = Some(FAKE_SETTING.to_owned());
        }
    }
    /// `if p != nil { *p = FakeSetting }` — no emptiness test.
    fn mask_always(field: &mut Option<String>) {
        if field.is_some() {
            *field = Some(FAKE_SETTING.to_owned());
        }
    }

    mask_if_set(&mut config.ldap_settings.bind_password);
    mask_always(&mut config.file_settings.public_link_salt);
    mask_if_set(&mut config.file_settings.amazon_s3_secret_access_key);
    mask_if_set(&mut config.file_settings.export_amazon_s3_secret_access_key);
    mask_if_set(&mut config.file_settings.azure_access_key);
    mask_if_set(&mut config.file_settings.export_azure_access_key);
    mask_if_set(&mut config.email_settings.smtp_password);
    mask_if_set(&mut config.git_lab_settings.secret);
    mask_if_set(&mut config.google_settings.secret);
    mask_if_set(&mut config.office365_settings.secret);
    mask_if_set(&mut config.open_id_settings.secret);
    mask_always(&mut config.sql_settings.data_source);
    mask_always(&mut config.sql_settings.at_rest_encrypt_key);
    mask_always(&mut config.elasticsearch_settings.password);

    for replica in config
        .sql_settings
        .data_source_replicas
        .iter_mut()
        .flatten()
    {
        *replica = FAKE_SETTING.to_owned();
    }
    for replica in config
        .sql_settings
        .data_source_search_replicas
        .iter_mut()
        .flatten()
    {
        *replica = FAKE_SETTING.to_owned();
    }
    for lag in config
        .sql_settings
        .replica_lag_settings
        .iter_mut()
        .flatten()
    {
        // Go rebuilds the pointer rather than testing it for emptiness: `if p != nil { p = new(…) }`.
        if lag.data_source.is_some() {
            lag.data_source = Some(FAKE_SETTING.to_owned());
        }
    }

    if let Some(relay) = config
        .message_export_settings
        .global_relay_settings
        .as_mut()
    {
        mask_if_set(&mut relay.smtp_password);
    }
    mask_always(&mut config.service_settings.split_key);
    mask_if_set(&mut config.service_settings.google_developer_key);
    mask_if_set(&mut config.service_settings.giphy_sdk_key);
    mask_always(&mut config.cache_settings.redis_password);
    if let Some(libre) = config.auto_translation_settings.libre_translate.as_mut() {
        mask_if_set(&mut libre.api_key);
    }
}

/// Port of `model.GetServiceEnvironment` (service_environment.go:36).
///
/// `MM_SERVICEENVIRONMENT` when it holds one of the three known values, and otherwise the
/// **build**'s default: `production` for a binary built with the `production` tag and `dev`
/// otherwise (service_environment_dev_default.go:9). This binary has no such tag, so the fallback
/// is `dev` — which agrees with a Go server built the way this repo builds one and **disagrees**
/// with an official release. Set `MM_SERVICEENVIRONMENT` explicitly beside a release binary; it
/// changes `ServiceEnvironment`, `GiphySdkKey` and `CWSURL` in the client configuration. [D-310].
pub fn service_environment() -> String {
    match std::env::var("MM_SERVICEENVIRONMENT").as_deref() {
        Ok(SERVICE_ENVIRONMENT_PRODUCTION) => SERVICE_ENVIRONMENT_PRODUCTION.to_owned(),
        Ok(SERVICE_ENVIRONMENT_TEST) => SERVICE_ENVIRONMENT_TEST.to_owned(),
        Ok(SERVICE_ENVIRONMENT_DEV) => SERVICE_ENVIRONMENT_DEV.to_owned(),
        _ => SERVICE_ENVIRONMENT_DEV.to_owned(),
    }
}

/// `model.ServiceEnvironmentProduction` (service_environment.go:15).
pub const SERVICE_ENVIRONMENT_PRODUCTION: &str = "production";
/// `model.ServiceEnvironmentTest` (service_environment.go:18).
pub const SERVICE_ENVIRONMENT_TEST: &str = "test";
/// `model.ServiceEnvironmentDev` (service_environment.go:22).
pub const SERVICE_ENVIRONMENT_DEV: &str = "dev";
/// `model.ServiceSettingsDefaultGiphySdkKeyTest` (config.go:137).
const GIPHY_SDK_KEY_TEST: &str = "s0glxvzVg9azvPipKxcPLpXV0q1x1fVP";
/// `model.TeamSettingsLockProfileFieldsNone` (config.go:149).
const LOCK_PROFILE_FIELDS_NONE: &str = "none";
/// `model.PluginSettingsDefaultMarketplaceURL` (config.go:271).
const DEFAULT_MARKETPLACE_URL: &str = "https://api.integrations.mattermost.com";
/// `model.PluginIdApps` (plugin_constants.go:9).
const PLUGIN_ID_APPS: &str = "com.mattermost.apps";

/// `strconv.FormatBool(*p)` on a setting `SetDefaults` has filled.
///
/// Go dereferences these pointers without a nil check and would panic on one; this crate may not
/// panic, so a nil reads as Go's zero value. That case is unreachable against a document a Go
/// server wrote — every pointer in it is non-nil — and reaching it at all would mean the row came
/// from something else.
fn flag(value: Option<bool>) -> String {
    if value.unwrap_or(false) {
        "true"
    } else {
        "false"
    }
    .to_owned()
}

/// `strconv.Itoa(*p)` / `strconv.FormatInt(*p, 10)`. See [`flag`] for the nil case.
fn number(value: Option<i64>) -> String {
    value.unwrap_or(0).to_string()
}

/// `*p` for a `*string`. See [`flag`] for the nil case.
fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

/// Port of `config.getGiphySdkKey` (config/client.go:470).
fn giphy_sdk_key(settings: &mm_model::config::ServiceSettings) -> String {
    match service_environment().as_str() {
        // `model.MattermostGiphySdkKey` is injected with `-ldflags` and is empty in every build
        // from source, so the production fallback is the empty string here.
        SERVICE_ENVIRONMENT_PRODUCTION => text(&settings.giphy_sdk_key),
        SERVICE_ENVIRONMENT_DEV | SERVICE_ENVIRONMENT_TEST => GIPHY_SDK_KEY_TEST.to_owned(),
        _ => String::new(),
    }
}

/// Port of `config.IsAuditLoggingActive` (config/logger.go:118) for an **unlicensed** server.
///
/// `allowAdvancedLogging` is `license != nil && *license.Features.AdvancedLogging`, so it is
/// false here and the function reduces to its first branch. The advanced-logging arm — parse
/// `AdvancedLoggingJSON` and look for an audit level among the targets — is not ported because
/// nothing unlicensed can reach it.
fn audit_logging_active(settings: &mm_model::config::ExperimentalAuditSettings) -> bool {
    settings.file_enabled.unwrap_or(false)
}

/// Port of `config.GenerateLimitedClientConfig` (config/client.go:285) for an **unlicensed**
/// server: the map an *anonymous* caller gets from `GET /api/v4/config/client`.
///
/// # What "unlicensed" leaves out
///
/// Go's licensed block adds `EnableCustomTermsOfService`, `CustomTermsOfServiceReAcceptancePeriod`,
/// the GitLab trio, the Intune keys and the enterprise mobile keys, and it *overwrites* the
/// LDAP/SAML/MFA/Google/Office365 defaults this function leaves at their unlicensed values.
/// None of that is ported: `mm_api::config` forwards a licensed installation to Go rather than
/// guessing at a feature matrix it cannot verify, exactly as `getClientLicense` already does.
///
/// # `Version` and the four `Build*` keys are properties of the **binary**
///
/// `model.CurrentVersion` is a compile-time constant and is ported; `BuildNumber`, `BuildDate`,
/// `BuildHash`, `BuildHashEnterprise` and `BuildEnterpriseReady` are `-ldflags` variables, empty
/// in any build from source and non-empty in an official release. This reports the empty strings
/// a source build reports. [D-310].
pub fn generate_limited_client_config(
    config: &mm_model::config::Config,
    telemetry_id: &str,
) -> mm_model::utils::StringMap {
    let c = config;
    let mut props = mm_model::utils::StringMap::new();
    let mut put = |key: &str, value: String| {
        props.insert(key.to_owned(), value);
    };

    put("Version", mm_model::utils::CURRENT_VERSION.to_owned());
    put("BuildNumber", String::new());
    put("BuildDate", String::new());
    put("BuildHash", String::new());
    put("BuildHashEnterprise", String::new());
    put("BuildEnterpriseReady", String::new());
    put("ServiceEnvironment", service_environment());
    // `fips.IsEnabled` is a build tag; this binary has no FIPS mode at all.
    put("IsFipsEnabled", "false".to_owned());

    put(
        "EnableBotAccountCreation",
        flag(c.service_settings.enable_bot_account_creation),
    );
    put(
        "EnableDesktopLandingPage",
        flag(c.service_settings.enable_desktop_landing_page),
    );
    put("EnableFile", flag(c.log_settings.enable_file));
    put("FileLevel", text(&c.log_settings.file_level));

    // `strings.TrimRight(…, "/")` strips **every** trailing slash, not one.
    put(
        "SiteURL",
        text(&c.service_settings.site_url)
            .trim_end_matches('/')
            .to_owned(),
    );
    put("SiteName", text(&c.team_settings.site_name));
    put(
        "WebsocketURL",
        text(&c.service_settings.websocket_url)
            .trim_end_matches('/')
            .to_owned(),
    );
    put("WebsocketPort", number(c.service_settings.websocket_port));
    put(
        "WebsocketSecurePort",
        number(c.service_settings.websocket_secure_port),
    );
    put(
        "EnableUserCreation",
        flag(c.team_settings.enable_user_creation),
    );
    put("EnableOpenServer", flag(c.team_settings.enable_open_server));
    put(
        "EnableJoinLeaveMessageByDefault",
        flag(c.team_settings.enable_join_leave_message_by_default),
    );

    // The four `ClientRequirements` fields are plain `string`s in Go, not pointers.
    put(
        "AndroidLatestVersion",
        c.client_requirements.android_latest_version.clone(),
    );
    put(
        "AndroidMinVersion",
        c.client_requirements.android_min_version.clone(),
    );
    put(
        "IosLatestVersion",
        c.client_requirements.ios_latest_version.clone(),
    );
    put(
        "IosMinVersion",
        c.client_requirements.ios_min_version.clone(),
    );

    put("EnableDiagnostics", flag(c.log_settings.enable_diagnostics));
    put(
        "EnableClientMetrics",
        flag(c.metrics_settings.enable_client_metrics),
    );

    put(
        "EnableComplianceExport",
        flag(c.message_export_settings.enable_export),
    );

    put(
        "EnableSignUpWithEmail",
        flag(c.email_settings.enable_sign_up_with_email),
    );
    put(
        "EnableSignInWithEmail",
        flag(c.email_settings.enable_sign_in_with_email),
    );
    put(
        "EnableSignInWithUsername",
        flag(c.email_settings.enable_sign_in_with_username),
    );

    put(
        "EmailLoginButtonColor",
        text(&c.email_settings.login_button_color),
    );
    put(
        "EmailLoginButtonBorderColor",
        text(&c.email_settings.login_button_border_color),
    );
    put(
        "EmailLoginButtonTextColor",
        text(&c.email_settings.login_button_text_color),
    );

    put(
        "TermsOfServiceLink",
        text(&c.support_settings.terms_of_service_link),
    );
    put(
        "PrivacyPolicyLink",
        text(&c.support_settings.privacy_policy_link),
    );
    put("AboutLink", text(&c.support_settings.about_link));
    put("HelpLink", text(&c.support_settings.help_link));
    put(
        "ReportAProblemType",
        text(&c.support_settings.report_a_problem_type),
    );
    put(
        "ReportAProblemLink",
        text(&c.support_settings.report_a_problem_link),
    );
    put(
        "ReportAProblemMail",
        text(&c.support_settings.report_a_problem_mail),
    );
    put(
        "AllowDownloadLogs",
        flag(c.support_settings.allow_download_logs),
    );
    put(
        "ForgotPasswordLink",
        text(&c.support_settings.forgot_password_link),
    );
    put("SupportEmail", text(&c.support_settings.support_email));
    put(
        "EnableAskCommunityLink",
        flag(c.support_settings.enable_ask_community_link),
    );

    put(
        "DefaultClientLocale",
        text(&c.localization_settings.default_client_locale),
    );

    put(
        "EnableCustomEmoji",
        flag(c.service_settings.enable_custom_emoji),
    );
    put(
        "EnableUserStatuses",
        flag(c.service_settings.enable_user_statuses),
    );
    put(
        "AppDownloadLink",
        text(&c.native_app_settings.app_download_link),
    );
    put(
        "AndroidAppDownloadLink",
        text(&c.native_app_settings.android_app_download_link),
    );
    put(
        "IosAppDownloadLink",
        text(&c.native_app_settings.ios_app_download_link),
    );
    put(
        "MobileExternalBrowser",
        flag(c.native_app_settings.mobile_external_browser),
    );

    put("DiagnosticId", telemetry_id.to_owned());
    put("TelemetryId", telemetry_id.to_owned());
    put(
        "DiagnosticsEnabled",
        flag(c.log_settings.enable_diagnostics),
    );

    put("HasImageProxy", flag(c.image_proxy_settings.enable));

    put("PluginsEnabled", flag(c.plugin_settings.enable));
    // `Enable && PluginStates[PluginIdApps] != nil && PluginStates[PluginIdApps].Enable` — the
    // absence of the entry is a distinct case from the entry being disabled, and both are false.
    put(
        "AppsPluginEnabled",
        flag(Some(
            c.plugin_settings.enable.unwrap_or(false)
                && c.plugin_settings
                    .plugin_states
                    .as_ref()
                    .and_then(|states| states.get(PLUGIN_ID_APPS))
                    .is_some_and(|state| state.enable),
        )),
    );

    put(
        "PasswordMinimumLength",
        number(c.password_settings.minimum_length),
    );
    put(
        "PasswordRequireLowercase",
        flag(c.password_settings.lowercase),
    );
    put(
        "PasswordRequireUppercase",
        flag(c.password_settings.uppercase),
    );
    put("PasswordRequireNumber", flag(c.password_settings.number));
    put("PasswordRequireSymbol", flag(c.password_settings.symbol));
    put(
        "PasswordEnableForgotLink",
        flag(c.password_settings.enable_forgot_link),
    );

    // "Set default values for all options that require a license" (client.go:400). Go writes
    // `EnableCustomBrand`, `CustomBrandText`, `CustomDescriptionText` and `CWSURL` twice — once
    // as a licence default and again from the config two lines later — so the first write of each
    // is dead. Only the surviving value is produced here.
    put("EnableLdap", "false".to_owned());
    put("LdapLoginFieldName", String::new());
    put("EnableSaml", "false".to_owned());
    put("SamlLoginButtonText", String::new());
    put("EnableSignUpWithGoogle", "false".to_owned());
    put("EnableSignUpWithOffice365", "false".to_owned());
    put("EnableSignUpWithOpenId", "false".to_owned());
    put("OpenIdButtonText", String::new());
    put("OpenIdButtonColor", String::new());
    put("CWSURL", String::new());
    put(
        "EnableCustomBrand",
        flag(c.team_settings.enable_custom_brand),
    );
    put("CustomBrandText", text(&c.team_settings.custom_brand_text));
    put(
        "CustomDescriptionText",
        text(&c.team_settings.custom_description_text),
    );
    put(
        "EnableMultifactorAuthentication",
        flag(c.service_settings.enable_multifactor_authentication),
    );
    put("EnforceMultifactorAuthentication", "false".to_owned());
    put(
        "EnableGuestAccounts",
        flag(c.guest_accounts_settings.enable),
    );
    put("HideGuestTags", flag(c.guest_accounts_settings.hide_tags));
    put(
        "GuestAccountsEnforceMultifactorAuthentication",
        flag(c.guest_accounts_settings.enforce_multifactor_authentication),
    );
    put(
        "EnableGuestMagicLink",
        flag(c.guest_accounts_settings.enable_guest_magic_link),
    );

    // `for key, value := range c.FeatureFlags.ToMap() { props["FeatureFlag"+key] = value }`.
    if let Some(flags) = c.feature_flags.as_ref() {
        for (key, value) in flags.to_map() {
            props.insert(format!("FeatureFlag{key}"), value);
        }
    }

    props
}

/// Port of `config.GenerateClientConfig` (config/client.go:16) for an **unlicensed** server: the
/// map a caller *with a session* gets from `GET /api/v4/config/client`.
///
/// Go builds on the limited map and adds to it, so the two share every key the limited one
/// carries. See [`generate_limited_client_config`] for what a licence would change.
pub fn generate_client_config(
    config: &mm_model::config::Config,
    telemetry_id: &str,
) -> mm_model::utils::StringMap {
    let c = config;
    let mut props = generate_limited_client_config(config, telemetry_id);
    let mut put = |key: &str, value: String| {
        props.insert(key.to_owned(), value);
    };

    put(
        "EnableCustomUserStatuses",
        flag(c.team_settings.enable_custom_user_statuses),
    );
    put(
        "EnableLastActiveTime",
        flag(c.team_settings.enable_last_active_time),
    );
    put(
        "EnableUserDeactivation",
        flag(c.team_settings.enable_user_deactivation),
    );
    put(
        "RestrictDirectMessage",
        text(&c.team_settings.restrict_direct_message),
    );
    put(
        "TeammateNameDisplay",
        text(&c.team_settings.teammate_name_display),
    );
    put(
        "LockTeammateNameDisplay",
        flag(c.team_settings.lock_teammate_name_display),
    );
    // The **constant**, not the setting: Go writes `model.TeamSettingsLockProfileFieldsNone` here
    // and only an Enterprise licence replaces it with the configured value.
    put(
        "LockProfileFieldsForEmailUsers",
        LOCK_PROFILE_FIELDS_NONE.to_owned(),
    );
    put(
        "ExperimentalPrimaryTeam",
        text(&c.team_settings.experimental_primary_team),
    );
    put(
        "EnableJoinLeaveMessageByDefault",
        flag(c.team_settings.enable_join_leave_message_by_default),
    );
    put(
        "EnableChannelCategorySorting",
        flag(c.team_settings.enable_channel_category_sorting),
    );

    put(
        "EnableBotAccountCreation",
        flag(c.service_settings.enable_bot_account_creation),
    );
    put(
        "EnableDesktopLandingPage",
        flag(c.service_settings.enable_desktop_landing_page),
    );
    put(
        "EnableOAuthServiceProvider",
        flag(c.service_settings.enable_o_auth_service_provider),
    );
    put(
        "GoogleDeveloperKey",
        text(&c.service_settings.google_developer_key),
    );
    put(
        "EnableIncomingWebhooks",
        flag(c.service_settings.enable_incoming_webhooks),
    );
    put(
        "EnableOutgoingWebhooks",
        flag(c.service_settings.enable_outgoing_webhooks),
    );
    put(
        "EnableOutgoingOAuthConnections",
        flag(c.service_settings.enable_outgoing_o_auth_connections),
    );
    put("EnableCommands", flag(c.service_settings.enable_commands));
    put(
        "EnablePostUsernameOverride",
        flag(c.service_settings.enable_post_username_override),
    );
    put(
        "EnablePostIconOverride",
        flag(c.service_settings.enable_post_icon_override),
    );
    put(
        "EnableUserAccessTokens",
        flag(c.service_settings.enable_user_access_tokens),
    );
    put(
        "MaximumPersonalAccessTokenLifetimeDays",
        number(
            c.service_settings
                .maximum_personal_access_token_lifetime_days,
        ),
    );
    put(
        "EnableLinkPreviews",
        flag(c.service_settings.enable_link_previews),
    );
    put(
        "EnablePermalinkPreviews",
        flag(c.service_settings.enable_permalink_previews),
    );
    put("EnableTesting", flag(c.service_settings.enable_testing));
    put("EnableDeveloper", flag(c.service_settings.enable_developer));
    put(
        "EnableClientPerformanceDebugging",
        flag(c.service_settings.enable_client_performance_debugging),
    );
    put(
        "PostEditTimeLimit",
        number(c.service_settings.post_edit_time_limit),
    );
    put(
        "MinimumHashtagLength",
        number(c.service_settings.minimum_hashtag_length),
    );
    put("EnableTutorial", flag(c.service_settings.enable_tutorial));
    put(
        "EnableOnboardingFlow",
        flag(c.service_settings.enable_onboarding_flow),
    );
    put(
        "ExperimentalEnableDefaultChannelLeaveJoinMessages",
        flag(
            c.service_settings
                .experimental_enable_default_channel_leave_join_messages,
        ),
    );
    put(
        "ExperimentalGroupUnreadChannels",
        text(&c.service_settings.experimental_group_unread_channels),
    );
    put("EnableSVGs", flag(c.service_settings.enable_sv_gs));
    put(
        "EnableMarketplace",
        flag(c.plugin_settings.enable_marketplace),
    );
    put("EnableLatex", flag(c.service_settings.enable_latex));
    put(
        "EnableInlineLatex",
        flag(c.service_settings.enable_inline_latex),
    );
    put(
        "ExtendSessionLengthWithActivity",
        flag(c.service_settings.extend_session_length_with_activity),
    );
    put(
        "ManagedResourcePaths",
        text(&c.service_settings.managed_resource_paths),
    );
    put(
        "DeleteAccountLink",
        text(&c.service_settings.delete_account_link),
    );

    // Two keys Go hardcodes rather than reads. The comment on the first is Go's own: the setting
    // it used to mirror is gone and the name is kept for old mobile and web clients.
    put("ExperimentalEnablePostMetadata", "true".to_owned());
    put("ExperimentalTimezone", "true".to_owned());

    put(
        "DisableAppBar",
        flag(c.experimental_settings.disable_app_bar),
    );

    put(
        "ExperimentalEnableAutomaticReplies",
        flag(c.team_settings.experimental_enable_automatic_replies),
    );

    put(
        "SendEmailNotifications",
        flag(c.email_settings.send_email_notifications),
    );
    put(
        "SendPushNotifications",
        flag(c.email_settings.send_push_notifications),
    );
    put(
        "RequireEmailVerification",
        flag(c.email_settings.require_email_verification),
    );
    put(
        "EnableEmailBatching",
        flag(c.email_settings.enable_email_batching),
    );
    put(
        "EnablePreviewModeBanner",
        flag(c.email_settings.enable_preview_mode_banner),
    );
    put(
        "EmailNotificationContentsType",
        text(&c.email_settings.email_notification_contents_type),
    );

    put(
        "ShowEmailAddress",
        flag(c.privacy_settings.show_email_address),
    );
    put("ShowFullName", flag(c.privacy_settings.show_full_name));
    put(
        "UseAnonymousURLs",
        flag(c.privacy_settings.use_anonymous_ur_ls),
    );

    put(
        "EnableFileAttachments",
        flag(c.file_settings.enable_file_attachments),
    );
    put("EnablePublicLink", flag(c.file_settings.enable_public_link));

    put(
        "AvailableLocales",
        text(&c.localization_settings.available_locales),
    );
    put(
        "EnableExperimentalLocales",
        flag(c.localization_settings.enable_experimental_locales),
    );

    put("SQLDriverName", text(&c.sql_settings.driver_name));

    put(
        "EnableEmojiPicker",
        flag(c.service_settings.enable_emoji_picker),
    );
    put(
        "EnableGifPicker",
        flag(c.service_settings.enable_gif_picker),
    );
    put("GiphySdkKey", giphy_sdk_key(&c.service_settings));
    put("MaxFileSize", number(c.file_settings.max_file_size));

    put(
        "MaxNotificationsPerChannel",
        number(c.team_settings.max_notifications_per_channel),
    );
    put(
        "EnableConfirmNotificationsToChannel",
        flag(c.team_settings.enable_confirm_notifications_to_channel),
    );
    put(
        "TimeBetweenUserTypingUpdatesMilliseconds",
        number(
            c.service_settings
                .time_between_user_typing_updates_milliseconds,
        ),
    );
    put(
        "EnableUserTypingMessages",
        flag(c.service_settings.enable_user_typing_messages),
    );
    put(
        "EnableChannelViewedMessages",
        flag(c.service_settings.enable_channel_viewed_messages),
    );

    put("RunJobs", flag(c.job_settings.run_jobs));

    put(
        "EnableEmailInvitations",
        flag(c.service_settings.enable_email_invitations),
    );

    put("CWSURL", text(&c.cloud_settings.cwsurl));
    // `model.MockCWS` is an `-ldflags` variable and empty in any build from source.
    put("CWSMock", String::new());

    put(
        "DisableRefetchingOnBrowserFocus",
        flag(c.experimental_settings.disable_refetching_on_browser_focus),
    );
    put(
        "DisableWakeUpReconnectHandler",
        flag(c.experimental_settings.disable_wake_up_reconnect_handler),
    );
    put(
        "UsersStatusAndProfileFetchingPollIntervalMilliseconds",
        number(
            c.experimental_settings
                .users_status_and_profile_fetching_poll_interval_milliseconds,
        ),
    );
    // One setting under two keys: the second is the name mobile < 2.27 looks for, and it is
    // written **after** the `FeatureFlag*` block that would otherwise own that prefix — so a real
    // `ExperimentalCrossTeamSearch` flag would be overwritten here, not the other way round.
    put(
        "EnableCrossTeamSearch",
        flag(c.service_settings.enable_cross_team_search),
    );
    put(
        "FeatureFlagExperimentalCrossTeamSearch",
        flag(c.service_settings.enable_cross_team_search),
    );

    // "Set default values for all options that require a license" (client.go:118).
    put(
        "ExperimentalEnableAuthenticationTransfer",
        "true".to_owned(),
    );
    put("LdapNicknameAttributeSet", "false".to_owned());
    put("LdapFirstNameAttributeSet", "false".to_owned());
    put("LdapLastNameAttributeSet", "false".to_owned());
    put("LdapPictureAttributeSet", "false".to_owned());
    put("LdapPositionAttributeSet", "false".to_owned());
    put("EnableCompliance", "false".to_owned());
    put("EnableMobileFileDownload", "true".to_owned());
    put("EnableMobileFileUpload", "true".to_owned());
    put("SamlFirstNameAttributeSet", "false".to_owned());
    put("SamlLastNameAttributeSet", "false".to_owned());
    put("SamlNicknameAttributeSet", "false".to_owned());
    put("SamlPositionAttributeSet", "false".to_owned());
    put("EnableCluster", "false".to_owned());
    put("EnableMetrics", "false".to_owned());
    put("EnableBanner", "false".to_owned());
    put("BannerText", String::new());
    put("BannerColor", String::new());
    put("BannerTextColor", String::new());
    put("AllowBannerDismissal", "false".to_owned());
    put("EnableThemeSelection", "true".to_owned());
    put("DefaultTheme", String::new());
    put("AllowCustomThemes", "true".to_owned());
    put("AllowedThemes", String::new());
    put("DataRetentionEnableMessageDeletion", "false".to_owned());
    put("DataRetentionMessageRetentionHours", "0".to_owned());
    put("DataRetentionEnableFileDeletion", "false".to_owned());
    put("DataRetentionFileRetentionHours", "0".to_owned());

    put(
        "CustomUrlSchemes",
        join_commas(c.display_settings.custom_url_schemes.as_deref()),
    );
    put(
        "MaxMarkdownNodes",
        number(c.display_settings.max_markdown_nodes),
    );
    put(
        "IsDefaultMarketplace",
        flag(Some(
            text(&c.plugin_settings.marketplace_url) == DEFAULT_MARKETPLACE_URL,
        )),
    );
    put("ExperimentalSharedChannels", "false".to_owned());
    put(
        "CollapsedThreads",
        text(&c.service_settings.collapsed_threads),
    );
    put("EnableCustomGroups", "false".to_owned());
    put("PostPriority", flag(c.service_settings.post_priority));
    put(
        "AllowPersistentNotifications",
        flag(c.service_settings.allow_persistent_notifications),
    );
    put(
        "AllowPersistentNotificationsForGuests",
        flag(c.service_settings.allow_persistent_notifications_for_guests),
    );
    put(
        "PersistentNotificationMaxCount",
        number(c.service_settings.persistent_notification_max_count),
    );
    put(
        "PersistentNotificationIntervalMinutes",
        number(c.service_settings.persistent_notification_interval_minutes),
    );
    put(
        "PersistentNotificationMaxRecipients",
        number(c.service_settings.persistent_notification_max_recipients),
    );
    put(
        "EnableBurnOnRead",
        flag(c.service_settings.enable_burn_on_read),
    );
    put(
        "BurnOnReadDurationSeconds",
        number(c.service_settings.burn_on_read_duration_seconds),
    );
    put(
        "BurnOnReadMaximumTimeToLiveSeconds",
        number(c.service_settings.burn_on_read_maximum_time_to_live_seconds),
    );
    put(
        "AllowSyncedDrafts",
        flag(c.service_settings.allow_synced_drafts),
    );
    put(
        "DelayChannelAutocomplete",
        flag(c.experimental_settings.delay_channel_autocomplete),
    );
    put(
        "YoutubeReferrerPolicy",
        flag(c.experimental_settings.youtube_referrer_policy),
    );
    put(
        "UniqueEmojiReactionLimitPerPost",
        number(c.service_settings.unique_emoji_reaction_limit_per_post),
    );

    put(
        "EnableAttributeBasedAccessControl",
        flag(
            c.access_control_settings
                .enable_attribute_based_access_control,
        ),
    );
    put(
        "EnableUserManagedAttributes",
        flag(c.access_control_settings.enable_user_managed_attributes),
    );
    put(
        "EnableAccessControlAuditLogging",
        flag(
            c.access_control_settings
                .enable_access_control_audit_logging,
        ),
    );
    put(
        "AuditLoggingActive",
        flag(Some(audit_logging_active(&c.experimental_audit_settings))),
    );
    put(
        "EnableChannelPolicyIndicators",
        flag(c.access_control_settings.enable_channel_policy_indicators),
    );

    put(
        "WranglerPermittedWranglerRoles",
        join_commas(c.wrangler_settings.permitted_wrangler_roles.as_deref()),
    );
    put(
        "WranglerAllowedEmailDomain",
        join_commas(c.wrangler_settings.allowed_email_domain.as_deref()),
    );
    put(
        "WranglerMoveThreadMaxCount",
        number(c.wrangler_settings.move_thread_max_count),
    );
    put(
        "WranglerMoveThreadToAnotherTeamEnable",
        flag(c.wrangler_settings.move_thread_to_another_team_enable),
    );
    put(
        "WranglerMoveThreadFromPrivateChannelEnable",
        flag(c.wrangler_settings.move_thread_from_private_channel_enable),
    );
    put(
        "WranglerMoveThreadFromDirectMessageChannelEnable",
        flag(
            c.wrangler_settings
                .move_thread_from_direct_message_channel_enable,
        ),
    );
    put(
        "WranglerMoveThreadFromGroupMessageChannelEnable",
        flag(
            c.wrangler_settings
                .move_thread_from_group_message_channel_enable,
        ),
    );

    props
}

/// `strings.Join(slice, ",")` on a Go slice that may be nil — which joins to `""`.
/// Port of `url.URL.Hostname` over `splitHostPort` (net/url/url.go:1180).
///
/// `GoUrl::host` is `host` **or** `host:port`, so the port has to come off before the value can
/// be used as a cookie `Domain`. Two details a hand-rolled `split(':')` gets wrong:
///
/// - **The port must be numeric to be a port.** Go takes the *last* colon and only strips it when
///   everything after it is a digit — `strings.LastIndexByte` plus `validOptionalPort`. So
///   `example.com:https` keeps its whole authority as the hostname rather than losing the scheme
///   name, and an unbracketed IPv6 literal is not silently truncated at its final group.
/// - **Brackets around an IPv6 literal are stripped**, and only when they are balanced at both
///   ends. `[::1]:8065` is `::1`; `[::1]` alone is also `::1`.
///
/// Splitting first and unbracketing second is Go's order and matters: `[::1]:8065` needs the port
/// gone before the trailing `]` is at the end of the string.
fn go_hostname(host: &[u8]) -> String {
    let mut host = host;

    if let Some(colon) = host.iter().rposition(|&b| b == b':')
        && host[colon + 1..].iter().all(|b| b.is_ascii_digit())
    {
        // `validOptionalPort(host[colon:])`: the substring is `":"` plus the rest, and Go accepts
        // it when every byte after the colon is a digit — including **none at all**, so a
        // trailing bare `:` is a valid empty port and comes off.
        host = &host[..colon];
    }

    if host.len() >= 2 && host.first() == Some(&b'[') && host.last() == Some(&b']') {
        host = &host[1..host.len() - 1];
    }

    // The host is percent-decoded by `go_parse`, so it can in principle hold bytes no `str` can.
    // A cookie `Domain` is a header value; lossy is the only thing that could be sent anyway.
    String::from_utf8_lossy(host).into_owned()
}

fn join_commas(values: Option<&[String]>) -> String {
    values.unwrap_or(&[]).join(",")
}

/// `model.SystemAsymmetricSigningKeyKey` (system.go:16).
const SYSTEM_ASYMMETRIC_SIGNING_KEY: &str = "AsymmetricSigningKey";
/// `model.SystemDiagnosticId` (system.go).
const SYSTEM_DIAGNOSTIC_ID: &str = "DiagnosticId";
/// `model.SystemInstallationDateKey` (system.go:18).
const SYSTEM_INSTALLATION_DATE: &str = "InstallationDate";
/// `model.SystemUpgradedFromTeId` (system.go:24).
const SYSTEM_UPGRADED_FROM_TE: &str = "UpgradedFromTE";

/// The `Systems` row behind `AsymmetricSigningPublicKey`.
///
/// `X` and `Y` are `*big.Int` and land in the row as **bare JSON integers of about 78 digits** —
/// far outside `f64`, so they are kept as raw text. Decoding them into `serde_json::Number` would
/// silently round them and produce a public key that is merely plausible.
#[derive(serde::Deserialize)]
struct AsymmetricSigningKeyRow {
    ecdsa_key: Option<EcdsaKeyRow>,
}

#[derive(serde::Deserialize)]
struct EcdsaKeyRow {
    curve: String,
    x: Box<serde_json::value::RawValue>,
    y: Box<serde_json::value::RawValue>,
}

/// Port of the `AsymmetricSigningPublicKey` line of `regenerateClientConfig`
/// (platform/config.go:234): `x509.MarshalPKIXPublicKey` of the P-256 public half, base64 with
/// the standard padded alphabet.
///
/// The DER is assembled by hand rather than through a crypto crate, because for P-256 it is
/// **fixed**: a 26-byte SubjectPublicKeyInfo prefix naming `id-ecPublicKey` and `prime256v1`,
/// then a BIT STRING holding the uncompressed point `0x04 || X || Y`. Every byte of the prefix is
/// determined by the curve, so there is no key material in it and nothing to get subtly wrong
/// that a round-trip against the running server would not catch.
///
/// `None` — the key row missing, a curve other than P-256, a coordinate that will not fit in 32
/// bytes — means Go's `if key := ps.AsymmetricSigningKey(); key != nil` did not fire and the
/// property is **absent** from the map rather than empty.
fn asymmetric_signing_public_key(row: &str) -> Option<String> {
    use base64::Engine as _;

    let parsed: AsymmetricSigningKeyRow = serde_json::from_str(row).ok()?;
    let key = parsed.ecdsa_key?;
    // `switch key.ECDSAKey.Curve { case "P-256": … default: return fmt.Errorf(…) }`
    // (platform/config.go:311). An unknown curve is a startup error in Go, so no client config is
    // generated at all; here it drops the one property, which is the closest thing a reader can do.
    if key.curve != "P-256" {
        return None;
    }
    let x = decimal_to_fixed_bytes(key.x.get(), 32)?;
    let y = decimal_to_fixed_bytes(key.y.get(), 32)?;

    /// SEQUENCE { SEQUENCE { OID 1.2.840.10045.2.1, OID 1.2.840.10045.3.1.7 }, BIT STRING …
    const P256_SPKI_PREFIX: &[u8] = &[
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let mut der = Vec::with_capacity(P256_SPKI_PREFIX.len() + 65);
    der.extend_from_slice(P256_SPKI_PREFIX);
    // `elliptic.Marshal`'s uncompressed point format.
    der.push(0x04);
    der.extend_from_slice(&x);
    der.extend_from_slice(&y);

    Some(base64::engine::general_purpose::STANDARD.encode(der))
}

/// A non-negative decimal integer as `width` big-endian bytes, left-padded with zeroes.
///
/// Long division by 256 over the decimal digits, because the values are 256-bit and nothing in
/// the dependency set does bignums. `None` for anything that is not a run of ASCII digits, or
/// that needs more than `width` bytes — both of which mean the row is not a P-256 key.
fn decimal_to_fixed_bytes(decimal: &str, width: usize) -> Option<Vec<u8>> {
    let decimal = decimal.trim();
    if decimal.is_empty() || !decimal.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut digits: Vec<u8> = decimal.bytes().map(|byte| byte - b'0').collect();
    let mut little_endian = Vec::with_capacity(width);
    while digits.iter().any(|digit| *digit != 0) {
        let mut remainder = 0u32;
        for digit in &mut digits {
            let value = remainder * 10 + u32::from(*digit);
            *digit = u8::try_from(value / 256).ok()?;
            remainder = value % 256;
        }
        little_endian.push(u8::try_from(remainder).ok()?);
        if little_endian.len() > width {
            return None;
        }
    }
    little_endian.resize(width, 0);
    little_endian.reverse();
    Some(little_endian)
}

impl crate::App {
    /// Port of `PlatformService.LimitedClientConfigWithComputed` (platform/config.go:330) for an
    /// **unlicensed** server — the body of `GET /api/v4/config/client` for a caller with no
    /// session.
    pub async fn limited_client_config_with_computed(
        &self,
    ) -> Result<mm_model::utils::StringMap, ConfigError> {
        let config = load_model_config(self.store().config()).await?;
        let mut props = generate_limited_client_config(&config, &self.telemetry_id().await);
        self.add_signing_key(&mut props).await;
        props.insert(
            "NoAccounts".to_owned(),
            flag(Some(self.no_accounts().await)),
        );
        Ok(props)
    }

    /// Port of `PlatformService.ClientConfigWithComputed` (platform/config.go:341) for an
    /// **unlicensed** server — the body of `GET /api/v4/config/client` for a caller with one.
    ///
    /// Every computed property here is best-effort in Go, and each failure has its **own**
    /// fallback rather than a shared one: a broken user count is `NoAccounts=false`, a broken
    /// install date is `InstallationDate=""` — present and empty — and a broken schema query
    /// leaves `SchemaVersion` **absent** from the map altogether. Collapsing the three into one
    /// error would change the key set a client sees on a bad day.
    pub async fn client_config_with_computed(
        &self,
    ) -> Result<mm_model::utils::StringMap, ConfigError> {
        let config = load_model_config(self.store().config()).await?;
        let mut props = generate_client_config(&config, &self.telemetry_id().await);
        self.add_signing_key(&mut props).await;

        props.insert(
            "NoAccounts".to_owned(),
            flag(Some(self.no_accounts().await)),
        );
        props.insert(
            "MaxPostSize".to_owned(),
            match self.max_post_size().await {
                Ok(size) => size.to_string(),
                Err(err) => {
                    tracing::warn!(error = %err, "could not read the maximum post size");
                    // `GetMaxPostSize` swallows its own error and answers the default, so the key
                    // is present either way.
                    mm_model::post::POST_MESSAGE_MAX_RUNES_V2.to_string()
                }
            },
        );
        props.insert(
            "UpgradedFromTE".to_owned(),
            flag(Some(
                self.system_value(SYSTEM_UPGRADED_FROM_TE).await.as_deref() == Some("true"),
            )),
        );
        // `respCfg["InstallationDate"] = ""` first, then overwritten only when the row parses.
        props.insert(
            "InstallationDate".to_owned(),
            self.system_value(SYSTEM_INSTALLATION_DATE)
                .await
                .filter(|raw| raw.parse::<i64>().is_ok())
                .unwrap_or_default(),
        );
        // `GetDBSchemaVersion` — the newest applied migration. On error Go logs and writes
        // **nothing**, so the key is missing rather than zero.
        match self.get_applied_schema_migrations().await {
            Ok(migrations) => {
                if let Some(latest) = migrations.first() {
                    props.insert("SchemaVersion".to_owned(), latest.version.to_string());
                }
            }
            Err(err) => tracing::warn!(error = %err, "could not read the schema version"),
        }

        Ok(props)
    }

    /// `ps.telemetryId`, which the platform loads from the `DiagnosticId` system row.
    async fn telemetry_id(&self) -> String {
        self.system_value(SYSTEM_DIAGNOSTIC_ID)
            .await
            .unwrap_or_default()
    }

    /// One `Systems` row, with a read failure treated as absence — which is what every caller
    /// here does with it.
    async fn system_value(&self, name: &str) -> Option<String> {
        use mm_store::SystemStore as _;
        match self.store().system().get_by_name(name).await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, name, "could not read a system row");
                None
            }
        }
    }

    /// `if key := ps.AsymmetricSigningKey(); key != nil { … }` (platform/config.go:234) — the
    /// property is added to **both** client maps or to neither.
    async fn add_signing_key(&self, props: &mut mm_model::utils::StringMap) {
        let Some(row) = self.system_value(SYSTEM_ASYMMETRIC_SIGNING_KEY).await else {
            return;
        };
        if let Some(encoded) = asymmetric_signing_public_key(&row) {
            props.insert("AsymmetricSigningPublicKey".to_owned(), encoded);
        }
    }

    /// Port of `PlatformService.IsFirstUserAccount` (platform/config.go:364).
    ///
    /// Go caches a "no users yet" flag and only queries while it is still set, so the query runs
    /// at most a handful of times in a server's life; here it runs per request. The **answer** is
    /// the same — `Count(UserCountOptions{IncludeDeleted: true}) == 0` — and a count that fails
    /// is `false`, not an error, exactly as Go's `if err != nil { return false }`.
    async fn no_accounts(&self) -> bool {
        use mm_store::UserStore as _;
        let options = mm_model::user_count::UserCountOptions {
            include_deleted: true,
            ..Default::default()
        };
        match self.store().user().count(&options).await {
            Ok(count) => count == 0,
            Err(err) => {
                tracing::warn!(error = %err, "could not count users for NoAccounts");
                false
            }
        }
    }
}

#[cfg(test)]
mod document {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// A document shaped like the real one for the fields each test touches, with every kind
    /// `applyEnvKey` switches on present: string, bool, int, slice, map, nested struct, a nil
    /// pointer and a `json.RawMessage`.
    fn document() -> serde_json::Value {
        serde_json::json!({
            "ServiceSettings": {
                "SiteURL": "",
                "EnableTesting": false,
                "WebsocketPort": 80,
                "TLSOverwriteCiphers": [],
                "LicenseFileLocation": null,
            },
            "PluginSettings": {
                "Enable": true,
                "Plugins": {},
                "PluginStates": null,
            },
            "LogSettings": { "AdvancedLoggingJSON": null },
            "MessageExportSettings": { "GlobalRelaySettings": null },
        })
    }

    #[test]
    fn a_string_setting_is_replaced_and_the_field_match_ignores_case() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_SITEURL", "http://x/")]),
        );
        assert_eq!(config["ServiceSettings"]["SiteURL"], "http://x/");
    }

    #[test]
    fn a_bool_parses_with_gos_spellings_and_an_unparseable_value_changes_nothing() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_ENABLETESTING", "T")]),
        );
        assert_eq!(config["ServiceSettings"]["EnableTesting"], true);

        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_ENABLETESTING", "yes")]),
        );
        assert_eq!(
            config["ServiceSettings"]["EnableTesting"], false,
            "`if err == nil` — Go never assigns when ParseBool fails"
        );
    }

    #[test]
    fn an_int_parses_base_ten_only() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_WEBSOCKETPORT", "8443")]),
        );
        assert_eq!(config["ServiceSettings"]["WebsocketPort"], 8443);

        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_WEBSOCKETPORT", "0x10")]),
        );
        assert_eq!(config["ServiceSettings"]["WebsocketPort"], 80);
    }

    /// `strings.Split(value, " ")` — **spaces**, not the commas [`split_list`] uses for the
    /// settings the narrow `Config` reads. Getting this wrong turns one cipher name into several.
    #[test]
    fn a_slice_splits_on_spaces_and_never_on_commas() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_TLSOVERWRITECIPHERS", "a,b c")]),
        );
        assert_eq!(
            config["ServiceSettings"]["TLSOverwriteCiphers"],
            serde_json::json!(["a,b", "c"])
        );
    }

    /// `rFieldValue.Elem()` on a nil pointer is invalid and Go returns without assigning. The
    /// environment cannot bring an unset setting to life — only `SetDefaults` can.
    #[test]
    fn a_nil_pointer_is_not_brought_to_life_by_the_environment() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_SERVICESETTINGS_LICENSEFILELOCATION", "/etc/mm.lic")]),
        );
        assert!(config["ServiceSettings"]["LicenseFileLocation"].is_null());
    }

    /// A Go **map** is a leaf that swallows the whole variable as JSON, and it does so even
    /// though a key part is left over. A Go **struct** is a level to descend into and needs one.
    /// JSON cannot tell the two apart, which is what [`MAP_VALUED_PATHS`] is for.
    #[test]
    fn a_map_takes_the_whole_value_as_json_and_a_struct_takes_a_deeper_key() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_PLUGINSETTINGS_PLUGINS_ANYTHING", r#"{"a":{"b":1}}"#)]),
        );
        assert_eq!(
            config["PluginSettings"]["Plugins"],
            serde_json::json!({"a": {"b": 1}}),
            "the leftover key part is ignored, as in Go"
        );

        let mut config = document();
        apply_environment_map(&mut config, &env(&[("MM_PLUGINSETTINGS", "true")]));
        assert_eq!(
            config["PluginSettings"]["Enable"], true,
            "a struct with no key part left is untouched, not replaced"
        );
    }

    /// A nil map is `null` on the wire but is still a map to `reflect`, so unlike a nil pointer
    /// the environment **can** set it.
    #[test]
    fn a_null_map_is_still_set_by_the_environment() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[("MM_PLUGINSETTINGS_PLUGINSTATES", r#"{"x":{"Enable":true}}"#)]),
        );
        assert_eq!(
            config["PluginSettings"]["PluginStates"],
            serde_json::json!({"x": {"Enable": true}})
        );
    }

    /// `json.RawMessage` is a `[]byte`, so the same applies to it — and it is checked before the
    /// nil arm for exactly that reason.
    #[test]
    fn a_null_raw_message_is_still_set_by_the_environment() {
        let mut config = document();
        apply_environment_map(
            &mut config,
            &env(&[(
                "MM_LOGSETTINGS_ADVANCEDLOGGINGJSON",
                r#"{"c":{"Type":"file"}}"#,
            )]),
        );
        assert_eq!(
            config["LogSettings"]["AdvancedLoggingJSON"],
            serde_json::json!({"c": {"Type": "file"}})
        );
    }

    #[test]
    fn an_unknown_field_and_a_variable_that_is_not_a_setting_change_nothing() {
        let mut config = document();
        let before = config.clone();
        apply_environment_map(
            &mut config,
            &env(&[
                ("MM_SERVICESETTINGS_NOSUCHSETTING", "x"),
                ("MM_NOSUCHSECTION_ANYTHING", "x"),
                // `GetEnvironment` keeps anything starting with `MM`, not `MM_`; the prefix trim
                // then leaves `MMRS_STACK`, which names no field.
                ("MMRS_STACK", "3"),
                ("MM_API_LISTEN", "127.0.0.1:8066"),
            ]),
        );
        assert_eq!(config, before);
    }

    #[test]
    fn the_environment_map_reports_only_what_is_set_and_drops_empty_sections() {
        let map = generate_environment_map(&env(&[
            ("MM_SERVICESETTINGS_SITEURL", "http://x"),
            ("MM_TEAMSETTINGS_ENABLEOPENSERVER", "true"),
        ]));
        assert_eq!(
            map,
            serde_json::json!({
                "ServiceSettings": {"SiteURL": true},
                "TeamSettings": {"EnableOpenServer": true},
            })
        );
    }

    /// Go's walk sees `*FeatureFlags` as a pointer, so it looks for `MM_FEATUREFLAGS` and never
    /// for a per-flag variable — even though [`apply_environment_map`] happily applies one. The
    /// asymmetry is Go's, and it is measured against the live pair by the parity suite.
    #[test]
    fn the_environment_map_never_reports_a_feature_flag() {
        let map = generate_environment_map(&env(&[(
            "MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD",
            "true",
        )]));
        assert_eq!(map, serde_json::json!({}));

        let mut config =
            serde_json::json!({"FeatureFlags": {"EnableShiftEscapeToMarkAllRead": false}});
        apply_environment_map(
            &mut config,
            &env(&[("MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD", "true")]),
        );
        assert_eq!(
            config["FeatureFlags"]["EnableShiftEscapeToMarkAllRead"], true,
            "the overlay applies it even though the environment map will not report it"
        );
    }

    /// `x509.MarshalPKIXPublicKey` of the P-256 key this stack's Go server generated, transcribed
    /// from `GET /api/v4/config/client`'s own answer — Go's bytes, not a value computed here.
    ///
    /// Only the **public** coordinates appear: `d` is the private half and is neither read by
    /// [`asymmetric_signing_public_key`] nor written down.
    const SIGNING_KEY_ROW: &str = concat!(
        r#"{"ecdsa_key":{"curve":"P-256","#,
        r#""x":10628728706615536606915249921457720916166495303664134344306663176299871581582,"#,
        r#""y":39914676909083839426969281223075403254422723626260708177174659508446728097973}}"#
    );
    const SIGNING_KEY_DER_BASE64: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEF3+lsuz4Qa9zkmXOBEolVGaOCJxpRVU1caUBScPVtY5YPugS8FHmlYpw/KGrvLf+bANnyvWWLTZp6RXWPDEQtQ==";

    #[test]
    fn the_signing_public_key_matches_gos_own_der() {
        assert_eq!(
            asymmetric_signing_public_key(SIGNING_KEY_ROW).as_deref(),
            Some(SIGNING_KEY_DER_BASE64)
        );
    }

    #[test]
    fn a_key_that_is_not_p256_or_not_there_yields_no_property() {
        assert!(asymmetric_signing_public_key("{}").is_none());
        assert!(asymmetric_signing_public_key(r#"{"ecdsa_key":null}"#).is_none());
        assert!(
            asymmetric_signing_public_key(
                &SIGNING_KEY_ROW.replace(r#""curve":"P-256""#, r#""curve":"P-384""#)
            )
            .is_none()
        );
        assert!(asymmetric_signing_public_key("not json").is_none());
    }

    #[test]
    fn a_coordinate_is_left_padded_and_a_too_large_one_is_refused() {
        assert_eq!(decimal_to_fixed_bytes("0", 4), Some(vec![0, 0, 0, 0]));
        assert_eq!(decimal_to_fixed_bytes("1", 4), Some(vec![0, 0, 0, 1]));
        assert_eq!(decimal_to_fixed_bytes("255", 4), Some(vec![0, 0, 0, 255]));
        assert_eq!(decimal_to_fixed_bytes("256", 4), Some(vec![0, 0, 1, 0]));
        assert_eq!(
            decimal_to_fixed_bytes("4294967295", 4),
            Some(vec![255, 255, 255, 255])
        );
        assert_eq!(decimal_to_fixed_bytes("4294967296", 4), None);
        assert_eq!(decimal_to_fixed_bytes("-1", 4), None);
        assert_eq!(decimal_to_fixed_bytes("", 4), None);
    }
}

#[cfg(test)]
mod sanitizing {
    use super::*;
    use mm_model::config::{
        Config as ModelConfig, GlobalRelayMessageExportSettings, LibreTranslateProviderSettings,
        ReplicaLagSettings,
    };
    use mm_model::utils::FAKE_SETTING;

    fn configured() -> ModelConfig {
        let mut config = ModelConfig::default();
        config.ldap_settings.bind_password = Some("ldap-secret".to_owned());
        config.file_settings.public_link_salt = Some(String::new());
        config.file_settings.amazon_s3_secret_access_key = Some("s3".to_owned());
        config.email_settings.smtp_password = Some("smtp".to_owned());
        config.git_lab_settings.secret = Some("gitlab".to_owned());
        config.sql_settings.data_source =
            Some("postgres://mmuser:hunter2@localhost:5432/mattermost".to_owned());
        config.sql_settings.at_rest_encrypt_key = Some(String::new());
        config.sql_settings.data_source_replicas = Some(vec!["postgres://a".to_owned()]);
        config.sql_settings.data_source_search_replicas = Some(vec!["postgres://b".to_owned()]);
        config.sql_settings.replica_lag_settings = Some(vec![ReplicaLagSettings {
            data_source: Some("postgres://c".to_owned()),
            ..ReplicaLagSettings::default()
        }]);
        config.elasticsearch_settings.password = Some(String::new());
        config.service_settings.split_key = Some(String::new());
        config.service_settings.google_developer_key = Some(String::new());
        config.service_settings.giphy_sdk_key = Some("giphy".to_owned());
        config.cache_settings.redis_password = Some(String::new());
        config.message_export_settings.global_relay_settings =
            Some(GlobalRelayMessageExportSettings {
                smtp_password: Some("relay".to_owned()),
                ..GlobalRelayMessageExportSettings::default()
            });
        config.auto_translation_settings.libre_translate = Some(LibreTranslateProviderSettings {
            api_key: Some("libre".to_owned()),
            ..LibreTranslateProviderSettings::default()
        });
        config.service_settings.site_url = Some("http://localhost:8065".to_owned());
        config
    }

    /// **The test that keeps the database password out of a response.**
    #[test]
    fn every_secret_is_replaced_and_the_data_source_is_not_merely_redacted() {
        let mut config = configured();
        sanitize(&mut config);

        for value in [
            &config.ldap_settings.bind_password,
            &config.file_settings.amazon_s3_secret_access_key,
            &config.email_settings.smtp_password,
            &config.git_lab_settings.secret,
            &config.sql_settings.data_source,
            &config.service_settings.giphy_sdk_key,
        ] {
            assert_eq!(value.as_deref(), Some(FAKE_SETTING));
        }
        assert_eq!(
            config.sql_settings.data_source.as_deref(),
            Some(FAKE_SETTING),
            "getConfig passes nil SanitizeOptions, so PartiallyRedactDataSources is off and the \
             data source is fully masked rather than url-redacted"
        );
        assert_eq!(
            config
                .message_export_settings
                .global_relay_settings
                .as_ref()
                .and_then(|relay| relay.smtp_password.as_deref()),
            Some(FAKE_SETTING)
        );
        assert_eq!(
            config
                .auto_translation_settings
                .libre_translate
                .as_ref()
                .and_then(|libre| libre.api_key.as_deref()),
            Some(FAKE_SETTING)
        );
        assert_eq!(
            config.sql_settings.data_source_replicas.as_deref(),
            Some([FAKE_SETTING.to_owned()].as_slice())
        );
        assert_eq!(
            config.sql_settings.data_source_search_replicas.as_deref(),
            Some([FAKE_SETTING.to_owned()].as_slice())
        );
        assert_eq!(
            config
                .sql_settings
                .replica_lag_settings
                .as_ref()
                .and_then(|lags| lags.first())
                .and_then(|lag| lag.data_source.as_deref()),
            Some(FAKE_SETTING)
        );
    }

    /// Five fields are masked **whether or not they hold anything** and the rest only when
    /// non-empty. Go writes the two forms two lines apart and the distinction is visible on the
    /// wire: an unconfigured SMTP password reads `""`, an unconfigured salt reads `FakeSetting`.
    #[test]
    fn the_unconditional_five_are_masked_even_when_empty_and_the_rest_are_not() {
        let mut config = configured();
        config.email_settings.smtp_password = Some(String::new());
        config.ldap_settings.bind_password = Some(String::new());
        sanitize(&mut config);

        for value in [
            &config.file_settings.public_link_salt,
            &config.sql_settings.at_rest_encrypt_key,
            &config.elasticsearch_settings.password,
            &config.service_settings.split_key,
            &config.cache_settings.redis_password,
        ] {
            assert_eq!(
                value.as_deref(),
                Some(FAKE_SETTING),
                "masked with no emptiness test"
            );
        }
        for value in [
            &config.email_settings.smtp_password,
            &config.ldap_settings.bind_password,
            &config.service_settings.google_developer_key,
        ] {
            assert_eq!(value.as_deref(), Some(""), "empty stays empty");
        }
    }

    #[test]
    fn a_setting_that_is_not_a_secret_is_untouched_and_an_absent_one_stays_absent() {
        let mut config = configured();
        config.ldap_settings.bind_password = None;
        sanitize(&mut config);
        assert_eq!(
            config.service_settings.site_url.as_deref(),
            Some("http://localhost:8065")
        );
        assert_eq!(config.ldap_settings.bind_password, None);
        assert_eq!(
            config.file_settings.azure_access_key, None,
            "a nil pointer is not given a fake value"
        );
    }
}

#[cfg(test)]
mod client_config {
    use super::*;
    use mm_model::config::{Config as ModelConfig, PluginState};

    fn base() -> ModelConfig {
        let mut config = ModelConfig::default();
        let mut flags = mm_model::feature_flags::FeatureFlags::default();
        flags.set_defaults();
        config.feature_flags = Some(flags);
        config
    }

    /// `strings.TrimRight(*SiteURL, "/")` removes **every** trailing slash, not one, and the same
    /// line runs on `WebsocketURL`.
    #[test]
    fn the_site_and_websocket_urls_lose_all_their_trailing_slashes() {
        let mut config = base();
        config.service_settings.site_url = Some("https://mm.example.com///".to_owned());
        config.service_settings.websocket_url = Some("wss://mm.example.com/".to_owned());
        let props = generate_limited_client_config(&config, "tid");
        assert_eq!(props["SiteURL"], "https://mm.example.com");
        assert_eq!(props["WebsocketURL"], "wss://mm.example.com");
    }

    /// `Enable && PluginStates[apps] != nil && PluginStates[apps].Enable` — three ways to be
    /// false and one to be true.
    #[test]
    fn the_apps_plugin_needs_plugins_on_and_its_own_state_on() {
        let mut config = base();
        config.plugin_settings.enable = Some(true);
        assert_eq!(
            generate_limited_client_config(&config, "")["AppsPluginEnabled"],
            "false",
            "no entry at all"
        );

        let mut states = std::collections::BTreeMap::new();
        states.insert(PLUGIN_ID_APPS.to_owned(), PluginState { enable: false });
        config.plugin_settings.plugin_states = Some(states.clone());
        assert_eq!(
            generate_limited_client_config(&config, "")["AppsPluginEnabled"],
            "false",
            "entry present but disabled"
        );

        states.insert(PLUGIN_ID_APPS.to_owned(), PluginState { enable: true });
        config.plugin_settings.plugin_states = Some(states);
        assert_eq!(
            generate_limited_client_config(&config, "")["AppsPluginEnabled"],
            "true"
        );

        config.plugin_settings.enable = Some(false);
        assert_eq!(
            generate_limited_client_config(&config, "")["AppsPluginEnabled"],
            "false",
            "plugins off beats an enabled state"
        );
    }

    #[test]
    fn the_marketplace_is_default_only_at_the_exact_url() {
        let mut config = base();
        config.plugin_settings.marketplace_url = Some(DEFAULT_MARKETPLACE_URL.to_owned());
        assert_eq!(
            generate_client_config(&config, "")["IsDefaultMarketplace"],
            "true"
        );
        config.plugin_settings.marketplace_url = Some(format!("{DEFAULT_MARKETPLACE_URL}/"));
        assert_eq!(
            generate_client_config(&config, "")["IsDefaultMarketplace"],
            "false"
        );
    }

    /// The **constant**, not `TeamSettings.LockProfileFieldsForEmailUsers`: only an Enterprise
    /// licence lets the configured value through, and this server never has one.
    #[test]
    fn lock_profile_fields_is_the_constant_and_not_the_setting() {
        let mut config = base();
        config.team_settings.lock_profile_fields_for_email_users = Some("all".to_owned());
        assert_eq!(
            generate_client_config(&config, "")["LockProfileFieldsForEmailUsers"],
            LOCK_PROFILE_FIELDS_NONE
        );
    }

    /// A nil `[]string` joins to `""`, and a populated one joins with commas — **not** the spaces
    /// the environment overlay splits on.
    #[test]
    fn string_slices_join_with_commas() {
        let mut config = base();
        assert_eq!(generate_client_config(&config, "")["CustomUrlSchemes"], "");
        config.display_settings.custom_url_schemes =
            Some(vec!["git".to_owned(), "smtp".to_owned()]);
        assert_eq!(
            generate_client_config(&config, "")["CustomUrlSchemes"],
            "git,smtp"
        );
    }

    /// Every flag reaches the client under a `FeatureFlag` prefix, and `TestFeature` is a string
    /// flag whose value is passed through rather than stringified as a bool.
    #[test]
    fn every_feature_flag_is_prefixed_and_string_flags_keep_their_value() {
        let props = generate_limited_client_config(&base(), "");
        assert_eq!(props["FeatureFlagTestFeature"], "off");
        assert_eq!(props["FeatureFlagNotificationMonitoring"], "true");
        assert_eq!(props["FeatureFlagAppsEnabled"], "false");
        assert!(!props.contains_key("TestFeature"));
    }

    /// A config with no `FeatureFlags` at all — which is what the persisted document carries,
    /// before `load_model_config` fills it — produces no `FeatureFlag*` keys rather than a panic.
    #[test]
    fn a_document_without_feature_flags_produces_no_flag_keys() {
        let mut config = base();
        config.feature_flags = None;
        let props = generate_limited_client_config(&config, "");
        assert!(!props.keys().any(|key| key.starts_with("FeatureFlag")));
    }

    /// `GenerateClientConfig` *starts* from the limited map, so the authenticated answer is a
    /// superset — and the keys it overwrites must differ only where Go overwrites them.
    #[test]
    fn the_full_map_is_a_superset_of_the_limited_one() {
        let mut config = base();
        config.cloud_settings.cwsurl = Some("https://portal.example".to_owned());
        let limited = generate_limited_client_config(&config, "tid");
        let full = generate_client_config(&config, "tid");

        for key in limited.keys() {
            assert!(full.contains_key(key), "{key} is missing from the full map");
        }
        assert_eq!(limited["CWSURL"], "", "the limited map's licence default");
        assert_eq!(
            full["CWSURL"], "https://portal.example",
            "the full map overwrites it from CloudSettings"
        );
        assert_eq!(limited["DiagnosticId"], "tid");
        assert_eq!(full["TelemetryId"], "tid");
    }

    /// The backwards-compatible alias is written **after** the `FeatureFlag*` block, so it is the
    /// setting that wins and not the flag.
    #[test]
    fn the_cross_team_search_alias_mirrors_the_setting() {
        let mut config = base();
        config.service_settings.enable_cross_team_search = Some(true);
        let props = generate_client_config(&config, "");
        assert_eq!(props["EnableCrossTeamSearch"], "true");
        assert_eq!(props["FeatureFlagExperimentalCrossTeamSearch"], "true");
    }

    /// Unlicensed, `IsAuditLoggingActive` reduces to `FileEnabled`; the advanced-logging arm is
    /// unreachable without `license.Features.AdvancedLogging`.
    #[test]
    fn audit_logging_is_active_only_when_the_audit_file_is_enabled() {
        let mut config = base();
        assert_eq!(
            generate_client_config(&config, "")["AuditLoggingActive"],
            "false"
        );
        config.experimental_audit_settings.file_enabled = Some(true);
        assert_eq!(
            generate_client_config(&config, "")["AuditLoggingActive"],
            "true"
        );
    }

    /// The licensed keys Go only writes inside `if license != nil` must be absent, not present
    /// and false — a client tells "the server cannot do this" from "the server did not say".
    #[test]
    fn the_licence_only_keys_are_absent_rather_than_false() {
        let props = generate_client_config(&base(), "");
        for key in [
            "PostAcknowledgements",
            "ScheduledPosts",
            "MobileEnableBiometrics",
            "ExperimentalRemoteClusterService",
            "EnableSignUpWithGitLab",
            "EnableCustomTermsOfService",
            "IntuneMAMEnabled",
            "ContentFlaggingEnabled",
        ] {
            assert!(!props.contains_key(key), "{key} needs a licence");
        }
        // …while the ones Go *does* write a default for are present.
        assert_eq!(props["EnableCompliance"], "false");
        assert_eq!(props["EnableThemeSelection"], "true");
        assert_eq!(props["ExperimentalEnableAuthenticationTransfer"], "true");
    }
}
