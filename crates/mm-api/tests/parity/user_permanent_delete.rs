//! Cross-server parity for `App.PermanentDeleteUser` (app/user.go:2134), through the two routes
//! that reach it: `DELETE /api/v4/users/{user_id}?permanent=true` on the HTTP router (with
//! `ServiceSettings.EnableAPIUserDeletion` on) and `localDeleteUser` on the local socket.
//!
//! ```sh
//! scripts/parity.sh --test parity user_permanent_delete
//! ```
//!
//! # Twins, compared table by table
//!
//! Each test builds two identical subjects, one for each server, and erases one through Go and
//! the other through us. Every planted id and name carries a twin marker (`pdu<scenario><g|r>`);
//! [`snapshot`] collects every row of the tables `PermanentDeleteUser` can touch that mentions
//! it, and [`normalized`] maps the two twins onto one spelling. The assertion is that the two erasures leave **the same rows
//! behind** — which is the whole observable contract of the route, since its response is
//! `{"status":"OK"}` either way.
//!
//! Timestamps written *by the erasure* (`Posts.UpdateAt` from the reaction sweep, a new `Status`
//! row) cannot match across two runs a second apart, so such a column is compared as "changed
//! from before the erasure, or not"; a value recomputed from fixture times (`LastReplyAt`) is
//! still compared exactly. See [`normalized`].
//!
//! # Every branch the fixture makes observable
//!
//! - **Replies**: the subject's replies go, the threads they were in are recounted — a thread
//!   whose stale `ReplyCount` is 4 comes back as the live count, `LastReplyAt` skips a
//!   soft-deleted later reply, and the subject leaves `Participants`; a thread whose `ReplyCount`
//!   is already 0 is **not** touched (Go's `ReplyCount > 0` guard), participants included.
//! - **Roots**: the subject's root goes with every reply to it, by anyone, and its thread rows.
//! - **Reactions**: the subject's reactions go, soft-deleted ones included; a post keeps
//!   `HasReactions` when someone else's live reaction remains and loses it when only a
//!   soft-deleted one does.
//! - **Rows keyed on the post, not the user**: a read receipt *by the subject* on someone else's
//!   post survives, and one by someone else on the subject's reply does not.
//! - **Files**: the live file and its preview and thumbnail are removed from disk, the
//!   soft-deleted file's bytes are **not** (Go only reads live infos), and `users/<id>/` goes.
//! - **The 202**: the bot subject's `users/<id>` is a plain file, so the profile-image check
//!   fails with ENOTDIR and the erasure finishes every table and then answers 202 with an error
//!   body — on both servers.
//!
//! # Configuration
//!
//! The HTTP route needs `EnableAPIUserDeletion`, which is off in the live document. It is turned
//! on through this server's `PUT /config/patch` (forwarded to main Go, which saves it, and
//! reloaded here) under [`common::CONFIG_DOCUMENT`]'s write guard, and restored panic or not.
//! `user_deletes`' refusal test holds the read guard.

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures_util::FutureExt;
use serde_json::{Map, Value};

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token,
    logged_in_user_id, stack_enabled,
};

/// A fixed epoch for every planted timestamp, so the fixture orders itself.
pub(crate) const T0: i64 = 1_790_000_000_000;

/// The tables `PermanentDeleteUser` writes, or that hold a row the fixture needs to see survive.
pub(crate) const TABLES: &[&str] = &[
    "users",
    "bots",
    "sessions",
    "useraccesstokens",
    "oauthaccessdata",
    "oauthauthdata",
    "incomingwebhooks",
    "outgoingwebhooks",
    "commands",
    "preferences",
    "channelmembers",
    "groupmembers",
    "posts",
    "threads",
    "threadmemberships",
    "reactions",
    "temporaryposts",
    "readreceipts",
    "scheduledposts",
    "drafts",
    "fileinfo",
    "audits",
    "teammembers",
    "status",
];

/// A 26-character id carrying `prefix`, padded with `x`.
pub(crate) fn pid(prefix: &str, kind: &str) -> String {
    format!("{:x<26}", format!("{prefix}{kind}"))
}

/// The stack's file-store root: `reference/.build/mmroot<-k>/data`, which both servers use.
pub(crate) fn data_dir() -> PathBuf {
    let offset: u16 = std::env::var("MMRS_PORT_OFFSET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let suffix = if offset == 0 {
        String::new()
    } else {
        format!("-{}", offset / 100)
    };
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../reference/.build/mmroot{suffix}/data"))
}

pub(crate) async fn pool() -> sqlx::PgPool {
    common::fixture_pool()
        .await
        .expect("DATABASE_URL names the stack's database")
}

/// One subject: a human account with rows in every table, and a bot owned by the administrator.
#[derive(Debug, Clone)]
pub(crate) struct Subject {
    /// `pdu<scenario><twin>` — every planted id and name contains it.
    pub marker: String,
    pub user_id: String,
    pub bot_id: String,
}

impl Subject {
    /// The file-store paths the fixture writes, relative to [`data_dir`].
    pub fn files(&self) -> Vec<String> {
        let m = &self.marker;
        vec![
            format!("pdu/{m}/live.txt"),
            format!("pdu/{m}/live_preview.jpg"),
            format!("pdu/{m}/live_thumb.jpg"),
            format!("pdu/{m}/deleted.txt"),
            format!("pdu/{m}/admin.txt"),
            format!("users/{}/profile.png", self.user_id),
            format!("users/{}/extra.txt", self.user_id),
            format!("users/{}", self.bot_id),
        ]
    }

    /// Which of [`Subject::files`] exist.
    pub fn files_present(&self) -> Vec<(String, bool)> {
        let root = data_dir();
        self.files()
            .into_iter()
            .map(|path| {
                let present = root.join(&path).exists();
                (path.replace(&self.user_id, "<U>"), present)
            })
            .map(|(path, present)| (path.replace(&self.marker, "<M>"), present))
            .collect()
    }

    /// (Re)write the file-store half of the fixture.
    pub fn write_files(&self) {
        let root = data_dir();
        for path in self.files() {
            let full = root.join(&path);
            if path == format!("users/{}", self.bot_id) {
                // A plain **file** where Go expects a directory: `os.Stat` of
                // `users/<id>/profile.png` is ENOTDIR, which is not `IsNotExist` — the 202 arm.
                let _ = std::fs::remove_dir_all(&full);
                std::fs::create_dir_all(full.parent().expect("a parent")).expect("mkdir");
                std::fs::write(&full, b"not a directory").expect("the bot's users/ entry");
                continue;
            }
            std::fs::create_dir_all(full.parent().expect("a parent")).expect("mkdir");
            std::fs::write(&full, path.as_bytes()).expect("a fixture file");
        }
    }
}

/// Remove everything a previous run of this marker left behind: rows that mention it or a user
/// named after it, and its files.
pub(crate) async fn scrub(marker: &str) {
    let pool = pool().await;
    let username = format!("mmrs{marker}user");
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&username)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
    let mut needles = vec![format!("%{marker}%")];
    needles.extend(ids.iter().map(|id| format!("%{id}%")));
    for table in TABLES.iter().chain(["channels", "usergroups"].iter()) {
        let sql = format!("DELETE FROM {table} x WHERE to_jsonb(x)::text LIKE ANY($1)");
        let _ = sqlx::query(&sql).bind(&needles).execute(&pool).await;
    }
    for id in ids {
        let _ = std::fs::remove_dir_all(data_dir().join(format!("users/{id}")));
    }
    let _ = std::fs::remove_dir_all(data_dir().join(format!("pdu/{marker}")));
    let _ = std::fs::remove_file(data_dir().join(format!("users/{}", pid(marker, "bot"))));
}

/// Plant one subject — an account with rows in every table [`TABLES`] names, the channel it
/// posts in, a bot owned by the administrator — and its files.
pub(crate) async fn plant(admin_id: &str, team_id: &str, marker: &str) -> Subject {
    scrub(marker).await;
    let pool = pool().await;
    let id = |kind: &str| pid(marker, kind);

    // Everything is planted rather than created through Go's API, so the twins are identical
    // row for row: an API-created account carries a salted password hash and system posts with
    // random ids, which no normalisation could line up.
    let user_id = id("user");
    let channel_id = id("chan");
    let pre: Vec<(&str, Vec<String>)> = vec![
        (
            "INSERT INTO users
                (id, createat, updateat, deleteat, username, password, authdata, authservice,
                 email, emailverified, nickname, firstname, lastname, position, roles,
                 allowmarketing, props, notifyprops, lastpasswordupdate, lastpictureupdate,
                 failedattempts, locale, timezone, mfaactive, mfasecret, remoteid, lastlogin,
                 mfausedtimestamps)
             VALUES ($1, 1790000000000, 1790000000000, 0, $2, '', NULL, '', $2 || '@mmrs.invalid',
                     true, '', 'Pdu', 'Subject', '', 'system_user', false, '{}'::jsonb,
                     '{}'::jsonb, 1790000000000, 0, 0, 'en', '{}'::jsonb, false, '', NULL, 0,
                     'null'::jsonb)",
            vec![user_id.clone(), format!("mmrs{marker}user")],
        ),
        (
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, header, purpose, lastpostat, totalmsgcount,
                                   extraupdateat, creatorid, schemeid, groupconstrained, shared,
                                   totalmsgcountroot, lastrootpostat, bannerinfo,
                                   defaultcategoryname, autotranslation, discoverable)
             VALUES ($1, 1790000000000, 1790000000000, 0, $2, 'O', $3, $3, '', '',
                     1790000000013, 0, 0, $4, NULL, NULL, NULL, 0, 1790000000012, NULL, '',
                     false, false)",
            vec![
                channel_id.clone(),
                team_id.into(),
                marker.into(),
                admin_id.into(),
            ],
        ),
        (
            "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                         mentioncount, notifyprops, lastupdateat, schemeuser,
                                         schemeadmin, schemeguest, mentioncountroot, msgcountroot,
                                         urgentmentioncount, autotranslation,
                                         autotranslationdisabled)
             VALUES ($1, $2, '', 0, 0, 0, '{\"push\": \"default\"}'::jsonb, 1790000000000, true,
                     false, false, 0, 0, 0, false, false),
                    ($1, $3, '', 0, 0, 0, '{\"push\": \"default\"}'::jsonb, 1790000000000, true,
                     true, false, 0, 0, 0, false, false)",
            vec![channel_id.clone(), user_id.clone(), admin_id.into()],
        ),
        // The live membership, and a soft-deleted one in another team: `RemoveAllMembersByUser`
        // is a hard delete of both.
        (
            "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin,
                                      schemeguest, createat)
             VALUES ($1, $2, '', 0, true, false, false, 1790000000000),
                    ($3, $2, '', 1790000000005, true, false, false, 1790000000000)",
            vec![team_id.into(), user_id.clone(), id("oldteam")],
        ),
    ];
    for (sql, binds) in pre {
        let mut query = sqlx::query(sql);
        for bind in &binds {
            query = query.bind(bind);
        }
        query
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("planting {marker}: {e}\n{sql}"));
    }

    let bot_id = id("bot");
    let statements: Vec<(String, Vec<String>)> = vec![
        // The bot, owned by the administrator: erasing it removes its `Bots` row.
        (
            "INSERT INTO users
                (id, createat, updateat, deleteat, username, password, authdata, authservice,
                 email, emailverified, nickname, firstname, lastname, position, roles,
                 allowmarketing, props, notifyprops, lastpasswordupdate, lastpictureupdate,
                 failedattempts, locale, timezone, mfaactive, mfasecret, remoteid, lastlogin,
                 mfausedtimestamps)
             VALUES ($1, 1790000000000, 1790000000000, 0, $2, '', NULL, '', $2 || '@mmrs.invalid',
                     false, '', 'Pdu Bot', '', '', 'system_user', false, '{}'::jsonb,
                     '{}'::jsonb, 1790000000000, 0, 0, 'en', '{}'::jsonb, false, '', NULL, 0,
                     'null'::jsonb)"
                .into(),
            vec![bot_id.clone(), format!("{marker}bot")],
        ),
        (
            "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat,
                               lasticonupdate)
             VALUES ($1, 'planted by user_permanent_delete', $2, 1790000000000, 1790000000000,
                     0, 0)"
                .into(),
            vec![bot_id.clone(), admin_id.into()],
        ),
        // Sessions: one plain, one minted by the access token below.
        (
            "INSERT INTO sessions (id, token, createat, expiresat, lastactivityat, userid,
                                   deviceid, roles, isoauth, props, expirednotify, voipdeviceid)
             VALUES ($1, $2, 1790000000000, 4102444800000, 1790000000000, $4, '', 'system_user',
                     false, '{}'::jsonb, false, ''),
                    ($3, $5, 1790000000000, 4102444800000, 1790000000000, $4, '', 'system_user',
                     false, '{\"type\":\"UserAccessToken\"}'::jsonb, false, '')"
                .into(),
            vec![
                id("sess"),
                id("sesstok"),
                id("sess2"),
                user_id.clone(),
                id("uattok"),
            ],
        ),
        (
            "INSERT INTO useraccesstokens (id, token, userid, description, isactive, expiresat,
                                           lastnotifiedat)
             VALUES ($1, $2, $3, 'pdu', true, 0, 0)"
                .into(),
            vec![id("uat"), id("uattok"), user_id.clone()],
        ),
        (
            "INSERT INTO oauthaccessdata (token, refreshtoken, redirecturi, clientid, userid,
                                          expiresat, scope, audience)
             VALUES ($1, '', 'https://pdu.invalid', $2, $3, 4102444800000, 'user', '')"
                .into(),
            vec![id("oat"), id("oapp"), user_id.clone()],
        ),
        (
            "INSERT INTO oauthauthdata (clientid, userid, code, expiresin, createat, redirecturi,
                                        state, scope, codechallenge, codechallengemethod,
                                        resource)
             VALUES ($1, $2, $3, 600, 1790000000000, 'https://pdu.invalid', '', 'user', '', '',
                     '')"
                .into(),
            vec![id("oapp"), user_id.clone(), id("oac")],
        ),
        // Webhooks: the subject's go, the administrator's stay. Incoming is keyed on `UserId`,
        // outgoing on `CreatorId`.
        (
            "INSERT INTO incomingwebhooks (id, createat, updateat, deleteat, userid, channelid,
                                           teamid, displayname, description, username, iconurl,
                                           channellocked, lastused)
             VALUES ($1, 1790000000000, 1790000000000, 0, $3, $5, $6, '', '', '', '', false, 0),
                    ($2, 1790000000000, 1790000000000, 0, $4, $5, $6, '', '', '', '', false, 0)"
                .into(),
            vec![
                id("inc"),
                id("inca"),
                user_id.clone(),
                admin_id.into(),
                channel_id.clone(),
                team_id.into(),
            ],
        ),
        (
            "INSERT INTO outgoingwebhooks (id, token, createat, updateat, deleteat, creatorid,
                                           channelid, teamid, triggerwords, callbackurls,
                                           displayname, contenttype, triggerwhen, username,
                                           iconurl, description)
             VALUES ($1, $1, 1790000000000, 1790000000000, 0, $3, $5, $6, 'pdu',
                     'https://pdu.invalid', '', '', 0, '', '', ''),
                    ($2, $2, 1790000000000, 1790000000000, 0, $4, $5, $6, 'pdu',
                     'https://pdu.invalid', '', '', 0, '', '', '')"
                .into(),
            vec![
                id("out"),
                id("outa"),
                user_id.clone(),
                admin_id.into(),
                channel_id.clone(),
                team_id.into(),
            ],
        ),
        (
            "INSERT INTO commands (id, token, createat, updateat, deleteat, creatorid, teamid,
                                   trigger, method, username, iconurl, autocomplete,
                                   autocompletedesc, autocompletehint, displayname, description,
                                   url, pluginid)
             VALUES ($1, $1, 1790000000000, 1790000000000, 0, $3, $5, $1, 'P', '', '', false,
                     '', '', '', '', 'https://pdu.invalid', ''),
                    ($2, $2, 1790000000000, 1790000000000, 7, $4, $5, $2, 'P', '', '', false,
                     '', '', '', '', 'https://pdu.invalid', '')"
                .into(),
            vec![
                id("cmd"),
                id("cmda"),
                user_id.clone(),
                admin_id.into(),
                team_id.into(),
            ],
        ),
        (
            "INSERT INTO preferences (userid, category, name, value)
             VALUES ($1, 'pdu', $2, 'one'), ($1, 'display_settings', $2, 'two')"
                .into(),
            vec![user_id.clone(), marker.into()],
        ),
        // Groups: a live and a soft-deleted membership of the subject, and the administrator's.
        (
            "INSERT INTO usergroups (id, name, displayname, description, source, remoteid,
                                     createat, updateat, deleteat, allowreference)
             VALUES ($1, $1, $1, '', 'custom', $1, 1790000000000, 1790000000000, 0, false),
                    ($2, $2, $2, '', 'custom', $2, 1790000000000, 1790000000000, 0, false)"
                .into(),
            vec![id("grp"), id("grp2")],
        ),
        (
            "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
             VALUES ($1, $3, 1790000000000, 0), ($1, $4, 1790000000000, 0),
                    ($2, $3, 1790000000000, 1790000000005)"
                .into(),
            vec![id("grp"), id("grp2"), user_id.clone(), admin_id.into()],
        ),
        (
            "INSERT INTO audits (id, createat, userid, action, extrainfo, ipaddress, sessionid)
             VALUES ($1, 1790000000001, $4, '/pdu', '', '', ''),
                    ($2, 1790000000002, $4, '/pdu', '', '', ''),
                    ($3, 1790000000003, $5, '/pdu', '', '', '')"
                .into(),
            vec![
                id("aud1"),
                id("aud2"),
                id("auda"),
                user_id.clone(),
                admin_id.into(),
            ],
        ),
        (
            "INSERT INTO scheduledposts (id, createat, updateat, userid, channelid, rootid, message,
                                         props, fileids, priority, scheduledat, processedat,
                                         errorcode, type, repeattype, repeattimezone)
             VALUES ($1, 1790000000000, 1790000000000, $3, $5, '', 'pdu', '{}', '[]', '',
                     4102444800000, 0, '', '', '', ''),
                    ($2, 1790000000000, 1790000000000, $4, $5, '', 'pdu', '{}', '[]', '',
                     4102444800000, 0, '', '', '', '')"
                .into(),
            vec![
                id("sch"),
                id("scha"),
                user_id.clone(),
                admin_id.into(),
                channel_id.clone(),
            ],
        ),
        (
            "INSERT INTO drafts (createat, updateat, deleteat, userid, channelid, rootid, message,
                                 props, fileids, priority, type)
             VALUES (1790000000000, 1790000000000, 0, $1, $3, '', 'pdu', '{}', '[]', '', ''),
                    (1790000000000, 1790000000000, 1790000000009, $1, $3, $4, 'pdu', '{}', '[]',
                     '', ''),
                    (1790000000000, 1790000000000, 0, $2, $3, '', 'pdu', '{}', '[]', '', '')"
                .into(),
            vec![
                user_id.clone(),
                admin_id.into(),
                channel_id.clone(),
                id("p2"),
            ],
        ),
    ];
    for (sql, binds) in statements {
        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = query.bind(bind);
        }
        query
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("planting {marker}: {e}\n{sql}"));
    }

    // Posts. `(id, create_at, delete_at, author, root, has_reactions)`; U is the subject, A the
    // administrator, B the bot.
    let u = user_id.as_str();
    let a = admin_id;
    let b = bot_id.as_str();
    let posts: Vec<(String, i64, i64, &str, String, bool)> = vec![
        // The subject's root, with the administrator's reply: both go.
        (id("p1"), T0 + 1, 0, u, String::new(), true),
        (id("r1"), T0 + 2, 0, a, id("p1"), false),
        // The administrator's root: the subject's reply goes, the thread is recounted.
        (id("p2"), T0 + 3, 0, a, String::new(), true),
        (id("r2"), T0 + 4, 0, u, id("p2"), true),
        (id("r3"), T0 + 5, 0, a, id("p2"), false),
        (id("r4"), T0 + 6, T0 + 7, u, id("p2"), false),
        (id("r5"), T0 + 8, T0 + 9, a, id("p2"), false),
        // The administrator's root whose thread row already reads zero replies.
        (id("p3"), T0 + 10, 0, a, String::new(), true),
        (id("r6"), T0 + 11, 0, u, id("p3"), false),
        // The bot's root with the administrator's reply.
        (id("p4"), T0 + 12, 0, b, String::new(), false),
        (id("r7"), T0 + 13, 0, a, id("p4"), false),
    ];
    for (post_id, create_at, delete_at, author, root, has_reactions) in &posts {
        sqlx::query(
            "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid,
                                originalid, message, type, props, hashtags, filenames, fileids,
                                hasreactions, editat, ispinned, remoteid)
             VALUES ($1, $2, $2, $3, $4, $5, $6, '', 'pdu', '', '{}'::jsonb, '', '[]', '[]',
                     $7, 0, false, NULL)",
        )
        .bind(post_id)
        .bind(create_at)
        .bind(delete_at)
        .bind(*author)
        .bind(&channel_id)
        .bind(root)
        .bind(has_reactions)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("planting post {post_id}: {e}"));
    }

    let rest: Vec<(&str, Vec<String>)> = vec![
        (
            "INSERT INTO threads (postid, replycount, lastreplyat, participants, channelid,
                                  threaddeleteat, threadteamid)
             VALUES ($1, 1, 1790000000002, jsonb_build_array($4::text), $5, 0, $6),
                    ($2, 4, 1790000000009, jsonb_build_array($7::text, $4::text), $5, 0, $6),
                    ($3, 0, 1790000000011, jsonb_build_array($7::text), $5, 0, $6)",
            vec![
                id("p1"),
                id("p2"),
                id("p3"),
                a.into(),
                channel_id.clone(),
                team_id.into(),
                u.into(),
            ],
        ),
        (
            "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated,
                                            unreadmentions)
             VALUES ($1, $3, true, 0, 1790000000002, 0), ($1, $4, true, 0, 1790000000002, 0),
                    ($2, $3, true, 0, 1790000000004, 0), ($2, $4, true, 0, 1790000000004, 0)",
            vec![id("p1"), id("p2"), u.into(), a.into()],
        ),
        (
            "INSERT INTO reactions (userid, postid, emojiname, createat, updateat, deleteat,
                                    remoteid, channelid)
             VALUES ($5, $1, 'smile', 1790000000020, 1790000000020, 0, NULL, $7),
                    ($5, $2, 'smile', 1790000000021, 1790000000021, 0, NULL, $7),
                    ($6, $3, 'smile', 1790000000022, 1790000000022, 0, NULL, $7),
                    ($5, $3, 'smile', 1790000000023, 1790000000023, 0, NULL, $7),
                    ($6, $4, '+1', 1790000000024, 1790000000024, 0, NULL, $7),
                    ($6, $4, 'heart', 1790000000025, 1790000000025, 1790000000026, NULL, $7),
                    ($5, $4, 'wave', 1790000000027, 1790000000027, 1790000000028, NULL, $7)",
            vec![
                id("p1"),
                id("r2"),
                id("p2"),
                id("p3"),
                a.into(),
                u.into(),
                channel_id.clone(),
            ],
        ),
        (
            "INSERT INTO temporaryposts (postid, type, expireat, message, fileids)
             VALUES ($1, 'pdu', 4102444800000, '', ''), ($2, 'pdu', 4102444800000, '', ''),
                    ($3, 'pdu', 4102444800000, '', '')",
            vec![id("p1"), id("r2"), id("p2")],
        ),
        (
            "INSERT INTO readreceipts (postid, userid, expireat)
             VALUES ($1, $4, 4102444800000), ($2, $4, 4102444800000), ($3, $5, 4102444800000)",
            vec![id("p1"), id("r2"), id("p2"), a.into(), u.into()],
        ),
        (
            "INSERT INTO fileinfo (id, creatorid, postid, createat, updateat, deleteat, path,
                                   thumbnailpath, previewpath, name, extension, size, mimetype,
                                   width, height, haspreviewimage, minipreview, content, remoteid,
                                   archived, channelid)
             VALUES ($1, $5, $8, 1790000000030, 1790000000030, 0, $9, $10, $11, 'live.txt',
                     'txt', 1, 'text/plain', 0, 0, false, NULL, '', NULL, false, $7),
                    ($2, $5, $8, 1790000000031, 1790000000031, 1790000000032, $12, '', '',
                     'deleted.txt', 'txt', 1, 'text/plain', 0, 0, false, NULL, '', NULL, false,
                     $7),
                    ($3, $5, $8, 1790000000033, 1790000000033, 0, $13, '', '', 'missing.txt',
                     'txt', 1, 'text/plain', 0, 0, false, NULL, '', NULL, false, $7),
                    ($4, $6, $8, 1790000000034, 1790000000034, 0, $14, '', '', 'admin.txt',
                     'txt', 1, 'text/plain', 0, 0, false, NULL, '', NULL, false, $7)",
            vec![
                id("file"),
                id("filed"),
                id("filem"),
                id("filea"),
                u.into(),
                a.into(),
                channel_id.clone(),
                id("p2"),
                format!("pdu/{marker}/live.txt"),
                format!("pdu/{marker}/live_thumb.jpg"),
                format!("pdu/{marker}/live_preview.jpg"),
                format!("pdu/{marker}/deleted.txt"),
                format!("pdu/{marker}/missing.txt"),
                format!("pdu/{marker}/admin.txt"),
            ],
        ),
    ];
    for (sql, binds) in rest {
        let mut query = sqlx::query(sql);
        for bind in &binds {
            query = query.bind(bind);
        }
        query
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("planting {marker}: {e}\n{sql}"));
    }

    let subject = Subject {
        marker: marker.to_owned(),
        user_id,
        bot_id,
    };
    subject.write_files();
    subject
}

/// Every row of [`TABLES`] whose JSON mentions one of `needles`, keyed by table and then by the
/// row's primary key.
pub(crate) async fn snapshot(
    pool: &sqlx::PgPool,
    tables: &[&str],
    needles: &[&str],
) -> BTreeMap<String, BTreeMap<String, Map<String, Value>>> {
    let patterns: Vec<String> = needles.iter().map(|n| format!("%{n}%")).collect();
    let mut out = BTreeMap::new();
    for table in tables {
        let key_columns = primary_key(pool, table).await;
        let sql = if needles.is_empty() {
            format!("SELECT to_jsonb(x) FROM {table} x")
        } else {
            format!("SELECT to_jsonb(x) FROM {table} x WHERE to_jsonb(x)::text LIKE ANY($1)")
        };
        let rows: Vec<Value> = sqlx::query_scalar(&sql)
            .bind(&patterns)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| panic!("reading {table}: {e}"));
        let mut keyed = BTreeMap::new();
        for row in rows {
            let Value::Object(row) = row else { continue };
            let key = key_columns
                .iter()
                .map(|c| row.get(c).map(Value::to_string).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|");
            keyed.insert(key, row);
        }
        out.insert((*table).to_owned(), keyed);
    }
    out
}

async fn primary_key(pool: &sqlx::PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT a.attname::text
           FROM pg_index i
           JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
          WHERE i.indrelid = $1::regclass AND i.indisprimary
          ORDER BY a.attname",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| panic!("the primary key of {table}: {e}"))
}

/// A zero, or one of the planted times (`T0` to `T0 + 1000`): deterministic, compared exactly.
fn is_fixture_time(value: &Value) -> bool {
    value.is_null()
        || value
            .as_i64()
            .is_some_and(|v| v == 0 || (T0..T0 + 1000).contains(&v))
}

/// Whether a column holds a time a server writes at the moment of a write.
fn is_time_column(column: &str) -> bool {
    column.ends_with("at") || matches!(column, "lastviewed" | "lastupdated")
}

/// `after`, with every time column that is not a fixture time ([`is_fixture_time`]) replaced by
/// `"<same>"`, `"<changed>"` or `"<new>"` relative to `before`, then every `(from, to)`
/// substitution applied to the serialized row, one sorted list of rows per table.
///
/// "Moved or not" is exactly as much as two runs a second apart can agree on, and it still
/// catches the direction that matters: a port that forgot to stamp `Posts.UpdateAt`, or stamped
/// a row Go leaves alone, differs here. A value the erasure *recomputes from the fixture* — a
/// thread's `LastReplyAt` — stays a fixture time and is compared exactly.
pub(crate) fn normalized(
    before: &BTreeMap<String, BTreeMap<String, Map<String, Value>>>,
    after: &BTreeMap<String, BTreeMap<String, Map<String, Value>>>,
    substitutions: &[(&str, &str)],
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (table, rows) in after {
        let mut lines = Vec::new();
        for (key, row) in rows {
            let previous = before.get(table).and_then(|t| t.get(key));
            let mut row = row.clone();
            for (column, value) in row.iter_mut() {
                if !is_time_column(column) || is_fixture_time(value) {
                    continue;
                }
                match previous {
                    Some(previous) if previous.get(column) == Some(value) => {
                        *value = Value::String("<same>".into());
                    }
                    Some(_) => *value = Value::String("<changed>".into()),
                    None => *value = Value::String("<new>".into()),
                }
            }
            let mut line = Value::Object(row).to_string();
            for (from, to) in substitutions {
                line = line.replace(from, to);
            }
            lines.push(line);
        }
        lines.sort();
        out.insert(table.clone(), lines);
    }
    out
}

/// Drop the rows this server's *apparatus* writes rather than the route: the sessions
/// `mm_api::go_cache` mints to reach Go, and the `Audits` row Go writes when that session asks it
/// to revoke a probe (`/users/{id}/sessions/revoke`) — which is how a served deactivation makes Go
/// forget the user's sessions ([D-870]).
pub(crate) fn drop_apparatus(rows: &mut BTreeMap<String, Vec<String>>) {
    for (table, lines) in rows.iter_mut() {
        lines.retain(|line| {
            !line.contains("mmrs_peer_cache")
                && !(table == "audits" && line.contains("/sessions/revoke\""))
        });
    }
}

/// Assert two normalized snapshots agree table by table, naming the first table that does not.
pub(crate) fn assert_same_rows(
    go: &BTreeMap<String, Vec<String>>,
    rs: &BTreeMap<String, Vec<String>>,
    context: &str,
) {
    for (table, go_rows) in go {
        let rs_rows = rs.get(table).cloned().unwrap_or_default();
        assert_eq!(
            go_rows, &rs_rows,
            "{context}: `{table}` differs after the erasure\n  go:   {go_rows:#?}\n  rust: {rs_rows:#?}"
        );
    }
}

/// The two twins' substitutions onto one spelling.
fn substitutions(subject: &Subject) -> Vec<(&str, &str)> {
    vec![(subject.marker.as_str(), "<M>")]
}

async fn admin_and_team(http: &reqwest::Client) -> (String, String, String) {
    let admin = go_minted_token(http).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(http, &admin).await;
    (admin, logged_in_user_id().to_owned(), team_id)
}

/// `(status, body, served-by)` of one `DELETE` over HTTP.
async fn http_delete(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
) -> (u16, Vec<u8>, bool) {
    let response = http
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        response.bytes().await.expect("a body").to_vec(),
        served,
    )
}

/// `(status, body, served-by)` of one `DELETE` over a local socket.
async fn socket_delete(socket: &std::path::Path, path: &str) -> (u16, Vec<u8>, bool) {
    let (status, headers, body) = common::local_socket::over_socket(socket, "DELETE", path).await;
    let served = headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, body, served)
}

/// The whole comparison for one transport: plant twins, erase each on its own server, compare
/// the responses, the rows and the files.
async fn run_twins<F, Fut>(scenario: char, erase: F)
where
    F: Fn(bool, String) -> Fut,
    Fut: std::future::Future<Output = (u16, Vec<u8>, bool)>,
{
    let http = client();
    let (_, admin_id, team_id) = admin_and_team(&http).await;
    let pool = pool().await;
    let go = plant(&admin_id, &team_id, &format!("pdu{scenario}g")).await;
    let rs = plant(&admin_id, &team_id, &format!("pdu{scenario}r")).await;

    let go_refs = [go.marker.as_str()];
    let rs_refs = [rs.marker.as_str()];
    let go_before = snapshot(&pool, TABLES, &go_refs).await;
    let rs_before = snapshot(&pool, TABLES, &rs_refs).await;
    assert_same_rows(
        &normalized(&go_before, &go_before, &substitutions(&go)),
        &normalized(&rs_before, &rs_before, &substitutions(&rs)),
        "the twins before the erasure",
    );

    let outcome = std::panic::AssertUnwindSafe(async {
        // The human: a clean erasure.
        let (go_status, go_body, _) = erase(false, go.user_id.clone()).await;
        let (rs_status, rs_body, served) = erase(true, rs.user_id.clone()).await;
        assert!(served, "the subject's erasure is served here");
        assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
        assert_eq!(
            rs_status,
            go_status,
            "{}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(rs_body, go_body);

        // The bot: every table, and then the 202 for its profile directory.
        let (go_status, go_body, _) = erase(false, go.bot_id.clone()).await;
        let (rs_status, rs_body, served) = erase(true, rs.bot_id.clone()).await;
        assert!(served, "the bot's erasure is served here");
        assert_eq!(go_status, 202, "{}", String::from_utf8_lossy(&go_body));
        assert_eq!(
            rs_status,
            go_status,
            "{}",
            String::from_utf8_lossy(&rs_body)
        );
        let body = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "the 202");
        assert_eq!(
            body["id"],
            "app.file_info.permanent_delete_by_user.app_error"
        );

        let go_after = snapshot(&pool, TABLES, &go_refs).await;
        let rs_after = snapshot(&pool, TABLES, &rs_refs).await;
        let mut go_rows = normalized(&go_before, &go_after, &substitutions(&go));
        let mut rs_rows = normalized(&rs_before, &rs_after, &substitutions(&rs));
        drop_apparatus(&mut go_rows);
        drop_apparatus(&mut rs_rows);
        assert_same_rows(&go_rows, &rs_rows, "after the erasure");

        // The fixture is not degenerate: the survivors the branches above name are there.
        let survivors = &go_rows;
        assert!(survivors["users"].is_empty(), "both accounts are gone");
        let user = pid("<M>", "user");
        let user = user.trim_end_matches('x');
        assert!(
            survivors["threads"]
                .iter()
                .any(|t| t.contains("\"replycount\":0") && t.contains(user)),
            "the zero-reply thread keeps the subject as a participant: {:#?}",
            survivors["threads"]
        );
        assert!(
            survivors["readreceipts"].iter().any(|r| r.contains(user)),
            "the subject's receipt on the administrator's post survives"
        );
        let kept: Vec<String> = ["p2", "r3", "r5", "p3"]
            .iter()
            .map(|kind| pid(&go.marker, kind).replace(&go.marker, "<M>"))
            .collect();
        assert_eq!(
            survivors["posts"].len(),
            kept.len(),
            "the administrator's roots and replies to them: {:#?}",
            survivors["posts"]
        );
        for id in &kept {
            assert!(
                survivors["posts"].iter().any(|p| p.contains(id.as_str())),
                "{id} survives"
            );
        }

        assert_eq!(
            go.files_present(),
            rs.files_present(),
            "the file store after the erasure"
        );
        assert!(
            go.files_present()
                .iter()
                .any(|(path, present)| path.ends_with("deleted.txt") && *present),
            "a soft-deleted file's bytes are left behind"
        );
    })
    .catch_unwind()
    .await;

    scrub(&go.marker).await;
    scrub(&rs.marker).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// `PUT {RUST}/api/v4/config/patch` for `EnableAPIUserDeletion`.
async fn set_user_deletion(http: &reqwest::Client, token: &str, on: bool) {
    let response = http
        .put(format!("{RUST}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "ServiceSettings": { "EnableAPIUserDeletion": on } }))
        .send()
        .await
        .expect("the config patch is answered");
    assert_eq!(response.status(), 200, "EnableAPIUserDeletion={on}");
}

/// **`DELETE /users/{id}?permanent=true` with the API deletion flag on erases the same rows as
/// Go.** See the module doc for what the fixture makes observable.
#[tokio::test]
async fn a_permanent_delete_over_http_leaves_the_same_rows_as_go() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let _config = common::CONFIG_DOCUMENT.write().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    set_user_deletion(&http, &admin, true).await;

    let outcome = std::panic::AssertUnwindSafe(run_twins('h', |ours, id| {
        let http = http.clone();
        let admin = admin.clone();
        async move {
            let base = if ours { RUST } else { GO };
            http_delete(
                &http,
                base,
                &format!("/api/v4/users/{id}?permanent=true"),
                &admin,
            )
            .await
        }
    }))
    .catch_unwind()
    .await;

    set_user_deletion(&http, &admin, false).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// **`localDeleteUser` with `?permanent=true` erases the same rows as Go's socket**, with no
/// configuration gate at all.
#[tokio::test]
async fn a_permanent_delete_over_the_socket_leaves_the_same_rows_as_go() {
    if !common::local_socket::sockets_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let go = common::local_socket::go_socket().expect("checked");
    let rust = common::local_socket::rust_socket().expect("checked");
    run_twins('l', |ours, id| {
        let socket = if ours { rust.clone() } else { go.clone() };
        async move { socket_delete(&socket, &format!("/api/v4/users/{id}?permanent=true")).await }
    })
    .await;
}

/// **An owner of a live bot is forwarded whole, on both transports**, because the deactivation
/// `PermanentDeleteUser` opens with would reach the sysadmin DM ([D-472]). The erasure is Go's.
#[tokio::test]
async fn a_bot_owner_is_forwarded_whole() {
    if !common::local_socket::sockets_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (_, admin_id, team_id) = admin_and_team(&http).await;
    let subject = plant(&admin_id, &team_id, "pduofwd").await;
    let pool = pool().await;
    sqlx::query("UPDATE bots SET ownerid = $1 WHERE userid = $2")
        .bind(&subject.user_id)
        .bind(&subject.bot_id)
        .execute(&pool)
        .await
        .expect("the subject owns the bot");

    let rust = common::local_socket::rust_socket().expect("checked");
    let (status, body, served) = socket_delete(
        &rust,
        &format!("/api/v4/users/{}?permanent=true", subject.user_id),
    )
    .await;
    scrub(&subject.marker).await;
    assert!(!served, "an owner of a live bot is Go's to erase");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
}
