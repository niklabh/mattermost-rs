//! Port of `model/audit_events.go` — the `event_name` values written to the audit log.
//!
//! These are **camelCase verbs**, not snake_case ids: `applyIPFilters`, `getUserAudits`. They are
//! matched literally by audit-log consumers and SIEM rules, so a renamed constant is a broken
//! alert somewhere downstream.
//!
//! Generated from the Go source rather than transcribed — a registry this size is exactly where a
//! hand-copy silently loses or mistypes one. Grouped as Go groups them, and each entry keeps the
//! Go comment describing what the event records.

// Access Control & Security
/// apply IP address filtering
pub const AUDIT_EVENT_APPLY_IP_FILTERS: &str = "applyIPFilters";
/// assign access control policy to channels and/or teams
pub const AUDIT_EVENT_ASSIGN_ACCESS_POLICY: &str = "assignAccessPolicy";
/// create access control policy
pub const AUDIT_EVENT_CREATE_ACCESS_CONTROL_POLICY: &str = "createAccessControlPolicy";
/// delete access control policy
pub const AUDIT_EVENT_DELETE_ACCESS_CONTROL_POLICY: &str = "deleteAccessControlPolicy";
/// remove access control policy from channels and/or teams
pub const AUDIT_EVENT_UNASSIGN_ACCESS_POLICY: &str = "unassignAccessPolicy";
/// update active/inactive status of access control policy
pub const AUDIT_EVENT_UPDATE_ACTIVE_STATUS: &str = "updateActiveStatus";
/// set active/inactive status of multiple access control policies
pub const AUDIT_EVENT_SET_ACTIVE_STATUS: &str = "setActiveStatus";
/// create/update plugin-owned access control policy (activation implicit)
pub const AUDIT_EVENT_SAVE_PLUGIN_ACCESS_CONTROL_POLICY: &str = "savePluginAccessControlPolicy";
/// delete plugin-owned access control policy
pub const AUDIT_EVENT_DELETE_PLUGIN_ACCESS_CONTROL_POLICY: &str = "deletePluginAccessControlPolicy";
/// create team-scoped access control policy
pub const AUDIT_EVENT_CREATE_TEAM_ACCESS_POLICY: &str = "createTeamAccessPolicy";
/// update team-scoped access control policy
pub const AUDIT_EVENT_UPDATE_TEAM_ACCESS_POLICY: &str = "updateTeamAccessPolicy";
/// delete team-scoped access control policy
pub const AUDIT_EVENT_DELETE_TEAM_ACCESS_POLICY: &str = "deleteTeamAccessPolicy";
/// assign channels to team-scoped access control policy
pub const AUDIT_EVENT_ASSIGN_TEAM_ACCESS_POLICY: &str = "assignTeamAccessPolicy";
/// remove channels from team-scoped access control policy
pub const AUDIT_EVENT_UNASSIGN_TEAM_ACCESS_POLICY: &str = "unassignTeamAccessPolicy";
/// trigger sync for team-scoped access control policies
pub const AUDIT_EVENT_TRIGGER_TEAM_POLICY_SYNC: &str = "triggerTeamPolicySync";
/// user auto-added to a team by its membership policy
pub const AUDIT_EVENT_TEAM_MEMBERSHIP_ADDED: &str = "teamMembershipAdded";
/// user removed from a team by its membership policy
pub const AUDIT_EVENT_TEAM_MEMBERSHIP_REMOVED: &str = "teamMembershipRemoved";
/// channel membership dropped as a cascade of a policy-driven team removal
pub const AUDIT_EVENT_TEAM_CASCADED_CHANNEL_REMOVAL: &str = "teamCascadedChannelRemoval";

// Audit & Certificates
/// add certificate for secure audit log transmission
pub const AUDIT_EVENT_ADD_AUDIT_LOG_CERTIFICATE: &str = "addAuditLogCertificate";
/// get audit log entries
pub const AUDIT_EVENT_GET_AUDITS: &str = "getAudits";
/// get audit log entries for specific user
pub const AUDIT_EVENT_GET_USER_AUDITS: &str = "getUserAudits";
/// remove certificate used for audit log transmission
pub const AUDIT_EVENT_REMOVE_AUDIT_LOG_CERTIFICATE: &str = "removeAuditLogCertificate";

// Bots
/// assign bot to user
pub const AUDIT_EVENT_ASSIGN_BOT: &str = "assignBot";
/// convert bot account to regular user account
pub const AUDIT_EVENT_CONVERT_BOT_TO_USER: &str = "convertBotToUser";
/// convert regular user account to bot account
pub const AUDIT_EVENT_CONVERT_USER_TO_BOT: &str = "convertUserToBot";
/// create bot account
pub const AUDIT_EVENT_CREATE_BOT: &str = "createBot";
/// update bot properties
pub const AUDIT_EVENT_PATCH_BOT: &str = "patchBot";
/// enable or disable bot account
pub const AUDIT_EVENT_UPDATE_BOT_ACTIVE: &str = "updateBotActive";

// Branding
/// delete brand image
pub const AUDIT_EVENT_DELETE_BRAND_IMAGE: &str = "deleteBrandImage";
/// upload brand image
pub const AUDIT_EVENT_UPLOAD_BRAND_IMAGE: &str = "uploadBrandImage";

// Channel Bookmarks
/// create bookmark in channels
pub const AUDIT_EVENT_CREATE_CHANNEL_BOOKMARK: &str = "createChannelBookmark";
/// delete bookmark
pub const AUDIT_EVENT_DELETE_CHANNEL_BOOKMARK: &str = "deleteChannelBookmark";
/// update bookmark
pub const AUDIT_EVENT_UPDATE_CHANNEL_BOOKMARK: &str = "updateChannelBookmark";
/// update display order of bookmarks
pub const AUDIT_EVENT_UPDATE_CHANNEL_BOOKMARK_SORT_ORDER: &str = "updateChannelBookmarkSortOrder";
/// list bookmarks for channel
pub const AUDIT_EVENT_LIST_CHANNEL_BOOKMARKS_FOR_CHANNEL: &str = "listChannelBookmarksForChannel";

// Boards
/// create board channel
pub const AUDIT_EVENT_CREATE_BOARD: &str = "createBoard";

// Views
/// create view in channel
pub const AUDIT_EVENT_CREATE_VIEW: &str = "createView";
/// get view by ID
pub const AUDIT_EVENT_GET_VIEW: &str = "getView";
/// update view
pub const AUDIT_EVENT_UPDATE_VIEW: &str = "updateView";
/// delete view
pub const AUDIT_EVENT_DELETE_VIEW: &str = "deleteView";
/// list views for channel
pub const AUDIT_EVENT_LIST_VIEWS_FOR_CHANNEL: &str = "listViewsForChannel";
/// update view sort order
pub const AUDIT_EVENT_UPDATE_VIEW_SORT_ORDER: &str = "updateViewSortOrder";
/// get posts for view
pub const AUDIT_EVENT_GET_POSTS_FOR_VIEW: &str = "getPostsForView";

// Channel Categories
/// create channel category for user
pub const AUDIT_EVENT_CREATE_CATEGORY_FOR_TEAM_FOR_USER: &str = "createCategoryForTeamForUser";
/// delete channel category
pub const AUDIT_EVENT_DELETE_CATEGORY_FOR_TEAM_FOR_USER: &str = "deleteCategoryForTeamForUser";
/// update multiple channel categories
pub const AUDIT_EVENT_UPDATE_CATEGORIES_FOR_TEAM_FOR_USER: &str = "updateCategoriesForTeamForUser";
/// update single channel category
pub const AUDIT_EVENT_UPDATE_CATEGORY_FOR_TEAM_FOR_USER: &str = "updateCategoryForTeamForUser";
/// update display order of the categories
pub const AUDIT_EVENT_UPDATE_CATEGORY_ORDER_FOR_TEAM_FOR_USER: &str =
    "updateCategoryOrderForTeamForUser";

// Channels
/// add member to channel
pub const AUDIT_EVENT_ADD_CHANNEL_MEMBER: &str = "addChannelMember";
/// convert group message to private channel
pub const AUDIT_EVENT_CONVERT_GROUP_MESSAGE_TO_CHANNEL: &str = "convertGroupMessageToChannel";
/// create public or private channel
pub const AUDIT_EVENT_CREATE_CHANNEL: &str = "createChannel";
/// request to join a discoverable private channel
pub const AUDIT_EVENT_CREATE_CHANNEL_JOIN_REQUEST: &str = "createChannelJoinRequest";
/// approve or deny a channel join request
pub const AUDIT_EVENT_UPDATE_CHANNEL_JOIN_REQUEST: &str = "updateChannelJoinRequest";
/// requester cancels their channel join request
pub const AUDIT_EVENT_WITHDRAW_CHANNEL_JOIN_REQUEST: &str = "withdrawChannelJoinRequest";
/// create direct message channel between two users
pub const AUDIT_EVENT_CREATE_DIRECT_CHANNEL: &str = "createDirectChannel";
/// create group message channel with multiple users
pub const AUDIT_EVENT_CREATE_GROUP_CHANNEL: &str = "createGroupChannel";
/// delete channel
pub const AUDIT_EVENT_DELETE_CHANNEL: &str = "deleteChannel";
/// get pinned posts
pub const AUDIT_EVENT_GET_PINNED_POSTS: &str = "getPinnedPosts";
/// add channel member locally
pub const AUDIT_EVENT_LOCAL_ADD_CHANNEL_MEMBER: &str = "localAddChannelMember";
/// create channel locally
pub const AUDIT_EVENT_LOCAL_CREATE_CHANNEL: &str = "localCreateChannel";
/// delete channel locally
pub const AUDIT_EVENT_LOCAL_DELETE_CHANNEL: &str = "localDeleteChannel";
/// move channel locally
pub const AUDIT_EVENT_LOCAL_MOVE_CHANNEL: &str = "localMoveChannel";
/// patch channel locally
pub const AUDIT_EVENT_LOCAL_PATCH_CHANNEL: &str = "localPatchChannel";
/// remove channel member locally
pub const AUDIT_EVENT_LOCAL_REMOVE_CHANNEL_MEMBER: &str = "localRemoveChannelMember";
/// restore channel locally
pub const AUDIT_EVENT_LOCAL_RESTORE_CHANNEL: &str = "localRestoreChannel";
/// update channel privacy locally
pub const AUDIT_EVENT_LOCAL_UPDATE_CHANNEL_PRIVACY: &str = "localUpdateChannelPrivacy";
/// move channel to different team
pub const AUDIT_EVENT_MOVE_CHANNEL: &str = "moveChannel";
/// update channel properties
pub const AUDIT_EVENT_PATCH_CHANNEL: &str = "patchChannel";
/// update channel moderation settings
pub const AUDIT_EVENT_PATCH_CHANNEL_MODERATIONS: &str = "patchChannelModerations";
/// remove member from channel
pub const AUDIT_EVENT_REMOVE_CHANNEL_MEMBER: &str = "removeChannelMember";
/// restore previously deleted channel
pub const AUDIT_EVENT_RESTORE_CHANNEL: &str = "restoreChannel";
/// bulk set (replace) channel memberships
pub const AUDIT_EVENT_SET_CHANNEL_MEMBERS: &str = "setChannelMembers";
/// update channel properties
pub const AUDIT_EVENT_UPDATE_CHANNEL: &str = "updateChannel";
/// update notification preferences
pub const AUDIT_EVENT_UPDATE_CHANNEL_MEMBER_NOTIFY_PROPS: &str = "updateChannelMemberNotifyProps";
/// update autotranslation setting
pub const AUDIT_EVENT_UPDATE_CHANNEL_MEMBER_AUTOTRANSLATION: &str =
    "updateChannelMemberAutotranslation";
/// update roles and permissions
pub const AUDIT_EVENT_UPDATE_CHANNEL_MEMBER_ROLES: &str = "updateChannelMemberRoles";
/// update scheme-based roles
pub const AUDIT_EVENT_UPDATE_CHANNEL_MEMBER_SCHEME_ROLES: &str = "updateChannelMemberSchemeRoles";
/// change channel privacy settings
pub const AUDIT_EVENT_UPDATE_CHANNEL_PRIVACY: &str = "updateChannelPrivacy";
/// update permission scheme applied to channel
pub const AUDIT_EVENT_UPDATE_CHANNEL_SCHEME: &str = "updateChannelScheme";

// Commands
/// create slash command
pub const AUDIT_EVENT_CREATE_COMMAND: &str = "createCommand";
/// delete command
pub const AUDIT_EVENT_DELETE_COMMAND: &str = "deleteCommand";
/// execute command
pub const AUDIT_EVENT_EXECUTE_COMMAND: &str = "executeCommand";
/// create command locally
pub const AUDIT_EVENT_LOCAL_CREATE_COMMAND: &str = "localCreateCommand";
/// move command to another team
pub const AUDIT_EVENT_MOVE_COMMAND: &str = "moveCommand";
/// regenerate authentication token for command
pub const AUDIT_EVENT_REGEN_COMMAND_TOKEN: &str = "regenCommandToken";
/// update command
pub const AUDIT_EVENT_UPDATE_COMMAND: &str = "updateCommand";

// Compliance
/// create compliance report
pub const AUDIT_EVENT_CREATE_COMPLIANCE_REPORT: &str = "createComplianceReport";
/// download compliance report
pub const AUDIT_EVENT_DOWNLOAD_COMPLIANCE_REPORT: &str = "downloadComplianceReport";
/// get specific compliance report
pub const AUDIT_EVENT_GET_COMPLIANCE_REPORT: &str = "getComplianceReport";
/// get all compliance reports
pub const AUDIT_EVENT_GET_COMPLIANCE_REPORTS: &str = "getComplianceReports";

// Configuration
/// reload server configuration
pub const AUDIT_EVENT_CONFIG_RELOAD: &str = "configReload";
/// get current server configuration
pub const AUDIT_EVENT_GET_CONFIG: &str = "getConfig";
/// get client configuration locally
pub const AUDIT_EVENT_LOCAL_GET_CLIENT_CONFIG: &str = "localGetClientConfig";
/// get server configuration locally
pub const AUDIT_EVENT_LOCAL_GET_CONFIG: &str = "localGetConfig";
/// update server configuration locally
pub const AUDIT_EVENT_LOCAL_PATCH_CONFIG: &str = "localPatchConfig";
/// update server configuration locally
pub const AUDIT_EVENT_LOCAL_UPDATE_CONFIG: &str = "localUpdateConfig";
/// migrate configs with file values from one store to another
pub const AUDIT_EVENT_MIGRATE_CONFIG: &str = "migrateConfig";
/// update server configuration
pub const AUDIT_EVENT_PATCH_CONFIG: &str = "patchConfig";
/// update server configuration
pub const AUDIT_EVENT_UPDATE_CONFIG: &str = "updateConfig";

// Custom Profile Attributes
/// create custom profile attribute
pub const AUDIT_EVENT_CREATE_CPA_FIELD: &str = "createCPAField";
/// delete custom profile attribute
pub const AUDIT_EVENT_DELETE_CPA_FIELD: &str = "deleteCPAField";
/// update custom profile attribute field
pub const AUDIT_EVENT_PATCH_CPA_FIELD: &str = "patchCPAField";
/// update custom profile attribute values
pub const AUDIT_EVENT_PATCH_CPA_VALUES: &str = "patchCPAValues";
pub const AUDIT_EVENT_CPA_VALUE_CHANGE: &str = "cpaValueChange";

// Property Fields
/// create property field
pub const AUDIT_EVENT_CREATE_PROPERTY_FIELD: &str = "createPropertyField";
/// delete property field
pub const AUDIT_EVENT_DELETE_PROPERTY_FIELD: &str = "deletePropertyField";
/// list property fields
pub const AUDIT_EVENT_GET_PROPERTY_FIELDS: &str = "getPropertyFields";
/// update property field
pub const AUDIT_EVENT_PATCH_PROPERTY_FIELD: &str = "patchPropertyField";

// Property Values
/// get property values for target
pub const AUDIT_EVENT_GET_PROPERTY_VALUES: &str = "getPropertyValues";
/// update property values for target
pub const AUDIT_EVENT_PATCH_PROPERTY_VALUES: &str = "patchPropertyValues";

// Data Retention Policies
/// add channels to data retention policy
pub const AUDIT_EVENT_ADD_CHANNELS_TO_POLICY: &str = "addChannelsToPolicy";
/// add teams to data retention policy
pub const AUDIT_EVENT_ADD_TEAMS_TO_POLICY: &str = "addTeamsToPolicy";
/// create data retention policy
pub const AUDIT_EVENT_CREATE_POLICY: &str = "createPolicy";
/// delete data retention policy
pub const AUDIT_EVENT_DELETE_POLICY: &str = "deletePolicy";
/// update data retention policy
pub const AUDIT_EVENT_PATCH_POLICY: &str = "patchPolicy";
/// remove channels from data retention policy
pub const AUDIT_EVENT_REMOVE_CHANNELS_FROM_POLICY: &str = "removeChannelsFromPolicy";
/// remove teams from data retention policy
pub const AUDIT_EVENT_REMOVE_TEAMS_FROM_POLICY: &str = "removeTeamsFromPolicy";

// Emojis
/// create emoji
pub const AUDIT_EVENT_CREATE_EMOJI: &str = "createEmoji";
/// delete emoji
pub const AUDIT_EVENT_DELETE_EMOJI: &str = "deleteEmoji";

// Exports
/// bulk export data to a file
pub const AUDIT_EVENT_BULK_EXPORT: &str = "bulkExport";
/// delete exported file
pub const AUDIT_EVENT_DELETE_EXPORT: &str = "deleteExport";
/// generate presigned URL to download the exported file
pub const AUDIT_EVENT_GENERATE_PRESIGN_URL_EXPORT: &str = "generatePresignURLExport";
/// schedule export job
pub const AUDIT_EVENT_SCHEDULE_EXPORT: &str = "scheduleExport";

// Files
/// get or download file
pub const AUDIT_EVENT_GET_FILE: &str = "getFile";
/// generate link for file sharing
pub const AUDIT_EVENT_GET_FILE_LINK: &str = "getFileLink";
/// upload file using multipart form data
pub const AUDIT_EVENT_UPLOAD_FILE_MULTIPART: &str = "uploadFileMultipart";
/// upload file using legacy multipart method
pub const AUDIT_EVENT_UPLOAD_FILE_MULTIPART_LEGACY: &str = "uploadFileMultipartLegacy";
/// upload file using simple direct upload method
pub const AUDIT_EVENT_UPLOAD_FILE_SIMPLE: &str = "uploadFileSimple";
/// get file thumbnail
pub const AUDIT_EVENT_GET_FILE_THUMBNAIL: &str = "getFileThumbnail";
/// get file infos for post
pub const AUDIT_EVENT_GET_FILE_INFOS_FOR_POST: &str = "getFileInfosForPost";
/// get file info
pub const AUDIT_EVENT_GET_FILE_INFO: &str = "getFileInfo";
/// get file preview
pub const AUDIT_EVENT_GET_FILE_PREVIEW: &str = "getFilePreview";
/// search for files
pub const AUDIT_EVENT_SEARCH_FILES: &str = "searchFiles";

// Groups
/// add members to group
pub const AUDIT_EVENT_ADD_GROUP_MEMBERS: &str = "addGroupMembers";
/// add user to group-synchronized teams and channels
pub const AUDIT_EVENT_ADD_USER_TO_GROUP_SYNCABLES: &str = "addUserToGroupSyncables";
/// create group
pub const AUDIT_EVENT_CREATE_GROUP: &str = "createGroup";
/// delete group
pub const AUDIT_EVENT_DELETE_GROUP: &str = "deleteGroup";
/// remove members from group
pub const AUDIT_EVENT_DELETE_GROUP_MEMBERS: &str = "deleteGroupMembers";
/// link group to team or channel for synchronization
pub const AUDIT_EVENT_LINK_GROUP_SYNCABLE: &str = "linkGroupSyncable";
/// update group
pub const AUDIT_EVENT_PATCH_GROUP: &str = "patchGroup";
/// update group synchronization settings
pub const AUDIT_EVENT_PATCH_GROUP_SYNCABLE: &str = "patchGroupSyncable";
/// restore previously deleted group
pub const AUDIT_EVENT_RESTORE_GROUP: &str = "restoreGroup";
/// unlink group from team or channel synchronization
pub const AUDIT_EVENT_UNLINK_GROUP_SYNCABLE: &str = "unlinkGroupSyncable";

// Imports
/// bulk import data from a file
pub const AUDIT_EVENT_BULK_IMPORT: &str = "bulkImport";
/// delete import file
pub const AUDIT_EVENT_DELETE_IMPORT: &str = "deleteImport";
/// import data from Slack
pub const AUDIT_EVENT_SLACK_IMPORT: &str = "slackImport";

// Jobs
/// cancel a job
pub const AUDIT_EVENT_CANCEL_JOB: &str = "cancelJob";
/// create a job
pub const AUDIT_EVENT_CREATE_JOB: &str = "createJob";
/// start job server
pub const AUDIT_EVENT_JOB_SERVER: &str = "jobServer";
/// update status of a job
pub const AUDIT_EVENT_UPDATE_JOB_STATUS: &str = "updateJobStatus";

// LDAP
/// add private certificate for LDAP
pub const AUDIT_EVENT_ADD_LDAP_PRIVATE_CERTIFICATE: &str = "addLdapPrivateCertificate";
/// add public certificate for LDAP
pub const AUDIT_EVENT_ADD_LDAP_PUBLIC_CERTIFICATE: &str = "addLdapPublicCertificate";
/// migrate user ID mapping to another attribute
pub const AUDIT_EVENT_ID_MIGRATE_LDAP: &str = "idMigrateLdap";
/// link LDAP group to Mattermost team or channel
pub const AUDIT_EVENT_LINK_LDAP_GROUP: &str = "linkLdapGroup";
/// remove private certificate for LDAP
pub const AUDIT_EVENT_REMOVE_LDAP_PRIVATE_CERTIFICATE: &str = "removeLdapPrivateCertificate";
/// remove public certificate for LDAP
pub const AUDIT_EVENT_REMOVE_LDAP_PUBLIC_CERTIFICATE: &str = "removeLdapPublicCertificate";
/// synchronize users and groups from LDAP
pub const AUDIT_EVENT_SYNC_LDAP: &str = "syncLdap";
/// unlink LDAP group from Mattermost team or channel
pub const AUDIT_EVENT_UNLINK_LDAP_GROUP: &str = "unlinkLdapGroup";

// Licensing
/// add license
pub const AUDIT_EVENT_ADD_LICENSE: &str = "addLicense";
/// add license locally
pub const AUDIT_EVENT_LOCAL_ADD_LICENSE: &str = "localAddLicense";
/// remove license locally
pub const AUDIT_EVENT_LOCAL_REMOVE_LICENSE: &str = "localRemoveLicense";
/// remove license
pub const AUDIT_EVENT_REMOVE_LICENSE: &str = "removeLicense";
/// request trial license
pub const AUDIT_EVENT_REQUEST_TRIAL_LICENSE: &str = "requestTrialLicense";

// OAuth
/// authorize OAuth app
pub const AUDIT_EVENT_AUTHORIZE_O_AUTH_APP: &str = "authorizeOAuthApp";
/// authorize OAuth page
pub const AUDIT_EVENT_AUTHORIZE_O_AUTH_PAGE: &str = "authorizeOAuthPage";
/// complete OAuth authorization flow
pub const AUDIT_EVENT_COMPLETE_O_AUTH: &str = "completeOAuth";
/// create OAuth app
pub const AUDIT_EVENT_CREATE_O_AUTH_APP: &str = "createOAuthApp";
/// create outgoing OAuth connection
pub const AUDIT_EVENT_CREATE_OUTGOING_OAUTH_CONNECTION: &str = "createOutgoingOauthConnection";
/// revoke OAuth app authorization
pub const AUDIT_EVENT_DEAUTHORIZE_O_AUTH_APP: &str = "deauthorizeOAuthApp";
/// delete OAuth app
pub const AUDIT_EVENT_DELETE_O_AUTH_APP: &str = "deleteOAuthApp";
/// delete outgoing OAuth connection
pub const AUDIT_EVENT_DELETE_OUTGOING_O_AUTH_CONNECTION: &str = "deleteOutgoingOAuthConnection";
/// get OAuth access token
pub const AUDIT_EVENT_GET_ACCESS_TOKEN: &str = "getAccessToken";
/// login using OAuth authentication provider
pub const AUDIT_EVENT_LOGIN_WITH_O_AUTH: &str = "loginWithOAuth";
/// mobile application login using OAuth authentication provider
pub const AUDIT_EVENT_MOBILE_LOGIN_WITH_O_AUTH: &str = "mobileLoginWithOAuth";
/// regenerate secret key for OAuth app
pub const AUDIT_EVENT_REGENERATE_O_AUTH_APP_SECRET: &str = "regenerateOAuthAppSecret";
/// register OAuth client via dynamic client registration (RFC 7591)
pub const AUDIT_EVENT_REGISTER_O_AUTH_CLIENT: &str = "registerOAuthClient";
/// create account using OAuth authentication provider
pub const AUDIT_EVENT_SIGNUP_WITH_O_AUTH: &str = "signupWithOAuth";
/// update OAuth app
pub const AUDIT_EVENT_UPDATE_O_AUTH_APP: &str = "updateOAuthApp";
/// update outgoing OAuth connection
pub const AUDIT_EVENT_UPDATE_OUTGOING_O_AUTH_CONNECTION: &str = "updateOutgoingOAuthConnection";
/// validate credentials for outgoing OAuth connection
pub const AUDIT_EVENT_VALIDATE_OUTGOING_O_AUTH_CONNECTION_CREDENTIALS: &str =
    "validateOutgoingOAuthConnectionCredentials";

// Plugins
/// disable installed plugin
pub const AUDIT_EVENT_DISABLE_PLUGIN: &str = "disablePlugin";
/// enable installed plugin
pub const AUDIT_EVENT_ENABLE_PLUGIN: &str = "enablePlugin";
/// get first admin visit status
pub const AUDIT_EVENT_GET_FIRST_ADMIN_VISIT_MARKETPLACE_STATUS: &str =
    "getFirstAdminVisitMarketplaceStatus";
/// install plugin from official marketplace
pub const AUDIT_EVENT_INSTALL_MARKETPLACE_PLUGIN: &str = "installMarketplacePlugin";
/// install plugin from external URL
pub const AUDIT_EVENT_INSTALL_PLUGIN_FROM_URL: &str = "installPluginFromURL";
/// delete plugin
pub const AUDIT_EVENT_REMOVE_PLUGIN: &str = "removePlugin";
/// set first admin visit status
pub const AUDIT_EVENT_SET_FIRST_ADMIN_VISIT_MARKETPLACE_STATUS: &str =
    "setFirstAdminVisitMarketplaceStatus";
/// upload plugin file to server for installation
pub const AUDIT_EVENT_UPLOAD_PLUGIN: &str = "uploadPlugin";

// Posts
/// create ephemeral post
pub const AUDIT_EVENT_CREATE_EPHEMERAL_POST: &str = "createEphemeralPost";
/// create post
pub const AUDIT_EVENT_CREATE_POST: &str = "createPost";
/// delete post
pub const AUDIT_EVENT_DELETE_POST: &str = "deletePost";
/// get edit history for post
pub const AUDIT_EVENT_GET_EDIT_HISTORY_FOR_POST: &str = "getEditHistoryForPost";
/// get flagged posts
pub const AUDIT_EVENT_GET_FLAGGED_POSTS: &str = "getFlaggedPosts";
/// get posts for channel
pub const AUDIT_EVENT_GET_POSTS_FOR_CHANNEL: &str = "getPostsForChannel";
/// get posts for channel around last unread
pub const AUDIT_EVENT_GET_POSTS_FOR_CHANNEL_AROUND_LAST_UNREAD: &str =
    "getPostsForChannelAroundLastUnread";
/// get post
pub const AUDIT_EVENT_GET_POST: &str = "getPost";
/// get post thread
pub const AUDIT_EVENT_GET_POST_THREAD: &str = "getPostThread";
/// get posts by ids
pub const AUDIT_EVENT_GET_POSTS_BY_IDS: &str = "getPostsByIds";
/// get thread for user
pub const AUDIT_EVENT_GET_THREAD_FOR_USER: &str = "getThreadForUser";
/// delete post locally
pub const AUDIT_EVENT_LOCAL_DELETE_POST: &str = "localDeletePost";
/// move thread and replies to different channel
pub const AUDIT_EVENT_MOVE_THREAD: &str = "moveThread";
/// notification ack
pub const AUDIT_EVENT_NOTIFICATION_ACK: &str = "notificationAck";
/// update post meta properties
pub const AUDIT_EVENT_PATCH_POST: &str = "patchPost";
/// restore post to previous version
pub const AUDIT_EVENT_RESTORE_POST_VERSION: &str = "restorePostVersion";
/// pin or unpin post
pub const AUDIT_EVENT_SAVE_IS_PINNED_POST: &str = "saveIsPinnedPost";
/// search for posts
pub const AUDIT_EVENT_SEARCH_POSTS: &str = "searchPosts";
/// update post content
pub const AUDIT_EVENT_UPDATE_POST: &str = "updatePost";
/// reveal a post that was hidden due to burn on read
pub const AUDIT_EVENT_REVEAL_POST: &str = "revealPost";
/// burn a post that was hidden due to burn on read
pub const AUDIT_EVENT_BURN_POST: &str = "burnPost";
/// post received via websocket
pub const AUDIT_EVENT_WEBSOCKET_POST: &str = "websocketPost";

// Recaps
/// create recap summarizing channel content
pub const AUDIT_EVENT_CREATE_RECAP: &str = "createRecap";
/// view a single recap
pub const AUDIT_EVENT_GET_RECAP: &str = "getRecap";
/// list user's recaps
pub const AUDIT_EVENT_GET_RECAPS: &str = "getRecaps";
/// mark recap as read
pub const AUDIT_EVENT_MARK_RECAP_AS_READ: &str = "markRecapAsRead";
/// bulk mark user's finished recaps as viewed
pub const AUDIT_EVENT_MARK_RECAPS_AS_VIEWED: &str = "markRecapsAsViewed";
/// regenerate recap with updated channel content
pub const AUDIT_EVENT_REGENERATE_RECAP: &str = "regenerateRecap";
/// delete recap
pub const AUDIT_EVENT_DELETE_RECAP: &str = "deleteRecap";

// Scheduled Recaps
/// create scheduled recap configuration
pub const AUDIT_EVENT_CREATE_SCHEDULED_RECAP: &str = "createScheduledRecap";
/// view a single scheduled recap
pub const AUDIT_EVENT_GET_SCHEDULED_RECAP: &str = "getScheduledRecap";
/// list user's scheduled recaps
pub const AUDIT_EVENT_GET_SCHEDULED_RECAPS: &str = "getScheduledRecaps";
/// update scheduled recap configuration
pub const AUDIT_EVENT_UPDATE_SCHEDULED_RECAP: &str = "updateScheduledRecap";
/// delete scheduled recap
pub const AUDIT_EVENT_DELETE_SCHEDULED_RECAP: &str = "deleteScheduledRecap";
/// pause scheduled recap execution
pub const AUDIT_EVENT_PAUSE_SCHEDULED_RECAP: &str = "pauseScheduledRecap";
/// resume paused scheduled recap
pub const AUDIT_EVENT_RESUME_SCHEDULED_RECAP: &str = "resumeScheduledRecap";

// Preferences
/// delete user preferences
pub const AUDIT_EVENT_DELETE_PREFERENCES: &str = "deletePreferences";
/// update user preferences
pub const AUDIT_EVENT_UPDATE_PREFERENCES: &str = "updatePreferences";

// Remote Clusters
/// create connection to remote Mattermost cluster
pub const AUDIT_EVENT_CREATE_REMOTE_CLUSTER: &str = "createRemoteCluster";
/// delete connection to remote Mattermost cluster
pub const AUDIT_EVENT_DELETE_REMOTE_CLUSTER: &str = "deleteRemoteCluster";
/// generate invitation token for remote cluster connection
pub const AUDIT_EVENT_GENERATE_REMOTE_CLUSTER_INVITE: &str = "generateRemoteClusterInvite";
/// invite remote cluster users to shared channel
pub const AUDIT_EVENT_INVITE_REMOTE_CLUSTER_TO_CHANNEL: &str = "inviteRemoteClusterToChannel";
/// update remote cluster connection settings
pub const AUDIT_EVENT_PATCH_REMOTE_CLUSTER: &str = "patchRemoteCluster";
/// accept invitation from remote cluster
pub const AUDIT_EVENT_REMOTE_CLUSTER_ACCEPT_INVITE: &str = "remoteClusterAcceptInvite";
/// accept message from remote cluster
pub const AUDIT_EVENT_REMOTE_CLUSTER_ACCEPT_MESSAGE: &str = "remoteClusterAcceptMessage";
/// upload profile image from remote cluster
pub const AUDIT_EVENT_REMOTE_UPLOAD_PROFILE_IMAGE: &str = "remoteUploadProfileImage";
/// remove remote cluster access from shared channel
pub const AUDIT_EVENT_UNINVITE_REMOTE_CLUSTER_TO_CHANNEL: &str = "uninviteRemoteClusterToChannel";
/// upload data to remote cluster
pub const AUDIT_EVENT_UPLOAD_REMOTE_DATA: &str = "uploadRemoteData";

// Roles
/// update role permissions
pub const AUDIT_EVENT_PATCH_ROLE: &str = "patchRole";

// SAML
/// add SAML identity provider certificate
pub const AUDIT_EVENT_ADD_SAML_IDP_CERTIFICATE: &str = "addSamlIdpCertificate";
/// add SAML private certificate
pub const AUDIT_EVENT_ADD_SAML_PRIVATE_CERTIFICATE: &str = "addSamlPrivateCertificate";
/// add SAML public certificate
pub const AUDIT_EVENT_ADD_SAML_PUBLIC_CERTIFICATE: &str = "addSamlPublicCertificate";
/// complete SAML authentication flow
pub const AUDIT_EVENT_COMPLETE_SAML: &str = "completeSaml";
/// remove SAML identity provider certificate
pub const AUDIT_EVENT_REMOVE_SAML_IDP_CERTIFICATE: &str = "removeSamlIdpCertificate";
/// remove SAML private certificate
pub const AUDIT_EVENT_REMOVE_SAML_PRIVATE_CERTIFICATE: &str = "removeSamlPrivateCertificate";
/// remove SAML public certificate
pub const AUDIT_EVENT_REMOVE_SAML_PUBLIC_CERTIFICATE: &str = "removeSamlPublicCertificate";

// Scheduled Posts
/// create post scheduled for future delivery
pub const AUDIT_EVENT_CREATE_SCHEDULE_POST: &str = "createSchedulePost";
/// delete scheduled post before delivery
pub const AUDIT_EVENT_DELETE_SCHEDULED_POST: &str = "deleteScheduledPost";
/// update scheduled post
pub const AUDIT_EVENT_UPDATE_SCHEDULED_POST: &str = "updateScheduledPost";

// Schemes
/// create permission scheme with role definitions
pub const AUDIT_EVENT_CREATE_SCHEME: &str = "createScheme";
/// delete scheme
pub const AUDIT_EVENT_DELETE_SCHEME: &str = "deleteScheme";
/// update scheme
pub const AUDIT_EVENT_PATCH_SCHEME: &str = "patchScheme";

// Search Indexes
/// purge Bleve search indexes
pub const AUDIT_EVENT_PURGE_BLEVE_INDEXES: &str = "purgeBleveIndexes";
/// purge Elasticsearch search indexes
pub const AUDIT_EVENT_PURGE_ELASTICSEARCH_INDEXES: &str = "purgeElasticsearchIndexes";

// Server Administration
/// clear server busy status to allow normal operations
pub const AUDIT_EVENT_CLEAR_SERVER_BUSY: &str = "clearServerBusy";
/// complete system onboarding process
pub const AUDIT_EVENT_COMPLETE_ONBOARDING: &str = "completeOnboarding";
/// closes active connections
pub const AUDIT_EVENT_DATABASE_RECYCLE: &str = "databaseRecycle";
/// download server log files
pub const AUDIT_EVENT_DOWNLOAD_LOGS: &str = "downloadLogs";
/// generate support packet with server diagnostics and logs
pub const AUDIT_EVENT_GENERATE_SUPPORT_PACKET: &str = "generateSupportPacket";
/// get list of applied database schema migrations
pub const AUDIT_EVENT_GET_APPLIED_SCHEMA_MIGRATIONS: &str = "getAppliedSchemaMigrations";
/// get server log entries
pub const AUDIT_EVENT_GET_LOGS: &str = "getLogs";
/// get system onboarding status
pub const AUDIT_EVENT_GET_ONBOARDING: &str = "getOnboarding";
/// clear server caches
pub const AUDIT_EVENT_INVALIDATE_CACHES: &str = "invalidateCaches";
/// check database integrity locally
pub const AUDIT_EVENT_LOCAL_CHECK_INTEGRITY: &str = "localCheckIntegrity";
/// search server log entries
pub const AUDIT_EVENT_QUERY_LOGS: &str = "queryLogs";
/// restart Mattermost server process
pub const AUDIT_EVENT_RESTART_SERVER: &str = "restartServer";
/// set server busy status to disallow any operations
pub const AUDIT_EVENT_SET_SERVER_BUSY: &str = "setServerBusy";
/// update viewed status of product notices
pub const AUDIT_EVENT_UPDATE_VIEWED_PRODUCT_NOTICES: &str = "updateViewedProductNotices";
/// upgrade server to Enterprise edition
pub const AUDIT_EVENT_UPGRADE_TO_ENTERPRISE: &str = "upgradeToEnterprise";

// Teams
/// add member to team
pub const AUDIT_EVENT_ADD_TEAM_MEMBER: &str = "addTeamMember";
/// add multiple members to team
pub const AUDIT_EVENT_ADD_TEAM_MEMBERS: &str = "addTeamMembers";
/// add user to team using invitation link
pub const AUDIT_EVENT_ADD_USER_TO_TEAM_FROM_INVITE: &str = "addUserToTeamFromInvite";
/// create team
pub const AUDIT_EVENT_CREATE_TEAM: &str = "createTeam";
/// delete team
pub const AUDIT_EVENT_DELETE_TEAM: &str = "deleteTeam";
/// import team data from external source
pub const AUDIT_EVENT_IMPORT_TEAM: &str = "importTeam";
/// invalidate all pending email invitations
pub const AUDIT_EVENT_INVALIDATE_ALL_EMAIL_INVITES: &str = "invalidateAllEmailInvites";
/// invite guest users to specific channels
pub const AUDIT_EVENT_INVITE_GUESTS_TO_CHANNELS: &str = "inviteGuestsToChannels";
/// invite users to team
pub const AUDIT_EVENT_INVITE_USERS_TO_TEAM: &str = "inviteUsersToTeam";
/// create team locally
pub const AUDIT_EVENT_LOCAL_CREATE_TEAM: &str = "localCreateTeam";
/// delete team locally
pub const AUDIT_EVENT_LOCAL_DELETE_TEAM: &str = "localDeleteTeam";
/// invite users to team locally
pub const AUDIT_EVENT_LOCAL_INVITE_USERS_TO_TEAM: &str = "localInviteUsersToTeam";
/// update team properties
pub const AUDIT_EVENT_PATCH_TEAM: &str = "patchTeam";
/// regenerate team invitation ID
pub const AUDIT_EVENT_REGENERATE_TEAM_INVITE_ID: &str = "regenerateTeamInviteId";
/// remove custom icon from team
pub const AUDIT_EVENT_REMOVE_TEAM_ICON: &str = "removeTeamIcon";
/// remove member from team
pub const AUDIT_EVENT_REMOVE_TEAM_MEMBER: &str = "removeTeamMember";
/// restore previously deleted team
pub const AUDIT_EVENT_RESTORE_TEAM: &str = "restoreTeam";
/// set custom icon for team
pub const AUDIT_EVENT_SET_TEAM_ICON: &str = "setTeamIcon";
/// update team properties
pub const AUDIT_EVENT_UPDATE_TEAM: &str = "updateTeam";
/// update roles of team members
pub const AUDIT_EVENT_UPDATE_TEAM_MEMBER_ROLES: &str = "updateTeamMemberRoles";
/// update scheme-based roles of team members
pub const AUDIT_EVENT_UPDATE_TEAM_MEMBER_SCHEME_ROLES: &str = "updateTeamMemberSchemeRoles";
/// change team privacy settings
pub const AUDIT_EVENT_UPDATE_TEAM_PRIVACY: &str = "updateTeamPrivacy";
/// update scheme applied to team
pub const AUDIT_EVENT_UPDATE_TEAM_SCHEME: &str = "updateTeamScheme";

// Terms of Service
/// create terms of service
pub const AUDIT_EVENT_CREATE_TERMS_OF_SERVICE: &str = "createTermsOfService";
/// save user acceptance of terms of service
pub const AUDIT_EVENT_SAVE_USER_TERMS_OF_SERVICE: &str = "saveUserTermsOfService";

// Threads
/// follow thread to receive notifications about replies
pub const AUDIT_EVENT_FOLLOW_THREAD_BY_USER: &str = "followThreadByUser";
/// mark thread as unread for user by post ID
pub const AUDIT_EVENT_SET_UNREAD_THREAD_BY_POST_ID: &str = "setUnreadThreadByPostId";
/// unfollow thread to stop receiving notifications about replies
pub const AUDIT_EVENT_UNFOLLOW_THREAD_BY_USER: &str = "unfollowThreadByUser";
/// update read status for all threads for user
pub const AUDIT_EVENT_UPDATE_READ_STATE_ALL_THREADS_BY_USER: &str =
    "updateReadStateAllThreadsByUser";
/// update read status for specific thread for user
pub const AUDIT_EVENT_UPDATE_READ_STATE_THREAD_BY_USER: &str = "updateReadStateThreadByUser";

// Uploads
/// create file upload session
pub const AUDIT_EVENT_CREATE_UPLOAD: &str = "createUpload";
/// upload file data to server storage
pub const AUDIT_EVENT_UPLOAD_DATA: &str = "uploadData";

// Users
/// attach device IDs (standard or VoIP) to user session for mobile app
pub const AUDIT_EVENT_ATTACH_DEVICE_ID: &str = "attachDeviceId";
/// create user account
pub const AUDIT_EVENT_CREATE_USER: &str = "createUser";
/// create personal access token for user API access
pub const AUDIT_EVENT_CREATE_USER_ACCESS_TOKEN: &str = "createUserAccessToken";
/// delete user account
pub const AUDIT_EVENT_DELETE_USER: &str = "deleteUser";
/// demote regular user to guest account with limited permissions
pub const AUDIT_EVENT_DEMOTE_USER_TO_GUEST: &str = "demoteUserToGuest";
/// disable user personal access token
pub const AUDIT_EVENT_DISABLE_USER_ACCESS_TOKEN: &str = "disableUserAccessToken";
/// enable user personal access token
pub const AUDIT_EVENT_ENABLE_USER_ACCESS_TOKEN: &str = "enableUserAccessToken";
/// extend user session expiration time
pub const AUDIT_EVENT_EXTEND_SESSION_EXPIRY: &str = "extendSessionExpiry";
/// delete user locally
pub const AUDIT_EVENT_LOCAL_DELETE_USER: &str = "localDeleteUser";
/// permanently delete all users locally
pub const AUDIT_EVENT_LOCAL_PERMANENT_DELETE_ALL_USERS: &str = "localPermanentDeleteAllUsers";
/// user login to system
pub const AUDIT_EVENT_LOGIN: &str = "login";
/// user login to system with desktop token
pub const AUDIT_EVENT_LOGIN_WITH_DESKTOP_TOKEN: &str = "loginWithDesktopToken";
/// user logout from system
pub const AUDIT_EVENT_LOGOUT: &str = "logout";
/// user marked all direct and group messages as read
pub const AUDIT_EVENT_MARK_MESSAGES_READ: &str = "markAllMessagesRead";
/// user marked an entire team as read
pub const AUDIT_EVENT_MARK_TEAM_READ: &str = "markFullTeamRead";
/// migrate user authentication method to LDAP
pub const AUDIT_EVENT_MIGRATE_AUTH_TO_LDAP: &str = "migrateAuthToLdap";
/// migrate user authentication method to SAML
pub const AUDIT_EVENT_MIGRATE_AUTH_TO_SAML: &str = "migrateAuthToSaml";
/// update user properties
pub const AUDIT_EVENT_PATCH_USER: &str = "patchUser";
/// promote guest account to regular user
pub const AUDIT_EVENT_PROMOTE_GUEST_TO_USER: &str = "promoteGuestToUser";
/// reset user password
pub const AUDIT_EVENT_RESET_PASSWORD: &str = "resetPassword";
/// reset failed password attempt counter
pub const AUDIT_EVENT_RESET_PASSWORD_FAILED_ATTEMPTS: &str = "resetPasswordFailedAttempts";
/// revoke all active sessions for all users
pub const AUDIT_EVENT_REVOKE_ALL_SESSIONS_ALL_USERS: &str = "revokeAllSessionsAllUsers";
/// revoke all active sessions for specific user
pub const AUDIT_EVENT_REVOKE_ALL_SESSIONS_FOR_USER: &str = "revokeAllSessionsForUser";
/// revoke specific user session
pub const AUDIT_EVENT_REVOKE_SESSION: &str = "revokeSession";
/// rejected an API request because the personal access token has expired
pub const AUDIT_EVENT_REJECT_EXPIRED_USER_ACCESS_TOKEN: &str = "rejectExpiredUserAccessToken";
/// revoke user personal access token
pub const AUDIT_EVENT_REVOKE_USER_ACCESS_TOKEN: &str = "revokeUserAccessToken";
/// revoke all personal access tokens that violate the maximum lifetime policy
pub const AUDIT_EVENT_REVOKE_NON_COMPLIANT_USER_ACCESS_TOKENS: &str =
    "revokeNonCompliantUserAccessTokens";
/// rotate (regenerate secret for) user personal access token
pub const AUDIT_EVENT_ROTATE_USER_ACCESS_TOKEN: &str = "rotateUserAccessToken";
/// send password reset email to user
pub const AUDIT_EVENT_SEND_PASSWORD_RESET: &str = "sendPasswordReset";
/// send email verification link to user
pub const AUDIT_EVENT_SEND_VERIFICATION_EMAIL: &str = "sendVerificationEmail";
/// set user profile image to default avatar
pub const AUDIT_EVENT_SET_DEFAULT_PROFILE_IMAGE: &str = "setDefaultProfileImage";
/// set custom profile image for user
pub const AUDIT_EVENT_SET_PROFILE_IMAGE: &str = "setProfileImage";
/// switch user authentication method from one to another
pub const AUDIT_EVENT_SWITCH_ACCOUNT_TYPE: &str = "switchAccountType";
/// update user password
pub const AUDIT_EVENT_UPDATE_PASSWORD: &str = "updatePassword";
/// update user account properties
pub const AUDIT_EVENT_UPDATE_USER: &str = "updateUser";
/// update user active status
pub const AUDIT_EVENT_UPDATE_USER_ACTIVE: &str = "updateUserActive";
/// update user authentication method
pub const AUDIT_EVENT_UPDATE_USER_AUTH: &str = "updateUserAuth";
/// update user multi-factor authentication settings
pub const AUDIT_EVENT_UPDATE_USER_MFA: &str = "updateUserMfa";
/// update user roles
pub const AUDIT_EVENT_UPDATE_USER_ROLES: &str = "updateUserRoles";
/// verify user email address using verification token
pub const AUDIT_EVENT_VERIFY_USER_EMAIL: &str = "verifyUserEmail";
/// verify user email address without verification token
pub const AUDIT_EVENT_VERIFY_USER_EMAIL_WITHOUT_TOKEN: &str = "verifyUserEmailWithoutToken";

// Webhooks
/// create incoming webhook
pub const AUDIT_EVENT_CREATE_INCOMING_HOOK: &str = "createIncomingHook";
/// create outgoing webhook
pub const AUDIT_EVENT_CREATE_OUTGOING_HOOK: &str = "createOutgoingHook";
/// delete incoming webhook
pub const AUDIT_EVENT_DELETE_INCOMING_HOOK: &str = "deleteIncomingHook";
/// delete outgoing webhook
pub const AUDIT_EVENT_DELETE_OUTGOING_HOOK: &str = "deleteOutgoingHook";
/// get incoming webhook details
pub const AUDIT_EVENT_GET_INCOMING_HOOK: &str = "getIncomingHook";
/// get outgoing webhook details
pub const AUDIT_EVENT_GET_OUTGOING_HOOK: &str = "getOutgoingHook";
/// create incoming webhook locally
pub const AUDIT_EVENT_LOCAL_CREATE_INCOMING_HOOK: &str = "localCreateIncomingHook";
/// regenerate authentication token
pub const AUDIT_EVENT_REGEN_OUTGOING_HOOK_TOKEN: &str = "regenOutgoingHookToken";
/// update incoming webhook
pub const AUDIT_EVENT_UPDATE_INCOMING_HOOK: &str = "updateIncomingHook";
/// update outgoing webhook
pub const AUDIT_EVENT_UPDATE_OUTGOING_HOOK: &str = "updateOutgoingHook";

// Content Flagging
/// flag post for review
pub const AUDIT_EVENT_FLAG_POST: &str = "flagPost";
/// get flagged post details
pub const AUDIT_EVENT_GET_FLAGGED_POST: &str = "getFlaggedPost";
/// permanently remove flagged post
pub const AUDIT_EVENT_PERMANENTLY_REMOVE_FLAGGED_POST: &str = "permanentlyRemoveFlaggedPost";
/// keep flagged post
pub const AUDIT_EVENT_KEEP_FLAGGED_POST: &str = "keepFlaggedPost";
/// update content flagging configuration
pub const AUDIT_EVENT_UPDATE_CONTENT_FLAGGING_CONFIG: &str = "updateContentFlaggingConfig";
/// assign reviewer for flagged post
pub const AUDIT_EVENT_SET_REVIEWER: &str = "setFlaggedPostReviewer";
/// generate flagged post data report
pub const AUDIT_EVENT_GENERATE_FLAGGED_POST_REPORT: &str = "generateFlaggedPostReport";
