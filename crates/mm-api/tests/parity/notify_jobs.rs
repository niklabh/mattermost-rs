//! Cross-server parity for the jobs whose worker is a plain `SimpleWorker` over one app function:
//! `product_notices` (`UpdateProductNotices`) and the notify-admin three
//! (`DoCheckForAdminNotifications`).
//!
//! Each case plants the same rows twice, lets the stack's own Go run the job on the first set
//! (a pending row its watcher claims — `common::insert_pending_job`) and a second mm-api with
//! the workers on run it on the second (`common::job_server`), and compares what each left.

use crate::common;

use common::{
    PRODUCT_NOTICE_VIEWS, client, create_plain_user, delete_plain_user, fixture_pool,
    go_minted_token, insert_pending_job, job_server, stack_enabled, wait_for_job,
};

/// The job-running mm-apis, one per test so the two can run at once; see `second_server_ports`.
const NOTICES_JOB_SERVER_PORT: u16 = 8114;
const NOTIFY_ADMIN_JOB_SERVER_PORT: u16 = 8123;

async fn plant_views(pool: &sqlx::PgPool, user_id: &str) {
    for notice in ["mmrs-gone-notice-a", "mmrs-gone-notice-b"] {
        sqlx::query(
            "INSERT INTO productnoticeviewstate (userid, noticeid, viewed, \"timestamp\") \
             VALUES ($1, $2, 1, 1) ON CONFLICT DO NOTHING",
        )
        .bind(user_id)
        .bind(notice)
        .execute(pool)
        .await
        .expect("a view row");
    }
}

async fn views(pool: &sqlx::PgPool, user_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM productnoticeviewstate WHERE userid = $1 AND noticeid LIKE 'mmrs-gone-%'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("a count")
}

/// `UpdateProductNotices`: the feed is fetched and every view of a notice it no longer carries is
/// deleted (`ClearOldNotices`). Both runs succeed and both clear the planted views.
#[tokio::test]
async fn the_product_notices_job_matches_gos_run() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let _views = PRODUCT_NOTICE_VIEWS.lock().await;
    let _jobs = common::JOB_RUNS.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "njviews").await;

    plant_views(&pool, &user.id).await;
    assert_eq!(views(&pool, &user.id).await, 2);
    let job = insert_pending_job(&pool, "product_notices", serde_json::json!({})).await;
    let (go_status, go_data) = wait_for_job(&pool, &job).await;
    let go_left = views(&pool, &user.id).await;

    let server = job_server(NOTICES_JOB_SERVER_PORT).await;
    plant_views(&pool, &user.id).await;
    assert_eq!(views(&pool, &user.id).await, 2);
    let job = insert_pending_job(&pool, "product_notices", serde_json::json!({})).await;
    let (rs_status, rs_data) = wait_for_job(&pool, &job).await;
    let rs_left = views(&pool, &user.id).await;
    assert!(
        server.ran(&job),
        "this server's worker ran the job, not Go's"
    );
    drop(server);

    assert_eq!(go_status, "success", "{go_data}");
    assert_eq!((rs_status.as_str(), rs_left), (go_status.as_str(), go_left));
    assert_eq!(
        go_left, 0,
        "ClearOldNotices removed the views of notices the feed lacks"
    );
    assert_eq!(rs_data, go_data, "the job's data");

    delete_plain_user(&http, &admin, &user.id).await;
}

/// One notify-admin fixture: the requesting user, and the rows they are made to have asked for.
async fn plant_notify_admin_rows(pool: &sqlx::PgPool, user_id: &str) {
    // Every unsent row goes first: the send reads the whole table, and a row another suite left
    // behind would be one more requester on one server's run and not the other's.
    sqlx::query("DELETE FROM notifyadmin WHERE sentat IS NULL OR userid = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("unsent rows cleared");
    sqlx::query("DELETE FROM systems WHERE name = 'LAST_UPGRADE_NOTIFICATION_TIMESTAMP'")
        .execute(pool)
        .await
        .expect("the cool-off cleared");
    for (feature, plan) in [
        ("mattermost.feature.guest_accounts", "professional"),
        ("mattermost.feature.custom_user_groups", "enterprise"),
        (
            "mattermost.feature.plugin.mmrsnj",
            "com.mmrs.alpha,com.mmrs.beta",
        ),
    ] {
        sqlx::query(
            "INSERT INTO notifyadmin (userid, createat, requiredplan, requiredfeature, trial) \
             VALUES ($1, $2, $3, $4, false)",
        )
        .bind(user_id)
        .bind(mm_model::utils::get_millis() - 60_000)
        .bind(plan)
        .bind(feature)
        .execute(pool)
        .await
        .expect("a notify-admin row");
    }
}

/// `(plan, feature, sent)` of the user's rows, sorted.
async fn notify_admin_rows(pool: &sqlx::PgPool, user_id: &str) -> Vec<(String, String, bool)> {
    let mut rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT requiredplan, requiredfeature, sentat IS NOT NULL FROM notifyadmin WHERE userid = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the rows");
    rows.sort();
    rows
}

/// The `custom_up_notification` posts in the admin's direct channel with the system bot since
/// `since`, as `(type, message, props, user is the bot)`.
async fn up_notifications(
    pool: &sqlx::PgPool,
    admin_id: &str,
    since: i64,
) -> Vec<(String, String, serde_json::Value)> {
    let bot: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'system-bot'")
        .fetch_one(pool)
        .await
        .expect("the system bot");
    let mut ids = [bot.as_str(), admin_id];
    ids.sort_unstable();
    let name = format!("{}__{}", ids[0], ids[1]);
    sqlx::query_as(
        "SELECT p.type, p.message, p.props FROM posts p JOIN channels c ON c.id = p.channelid \
         WHERE c.name = $1 AND p.createat >= $2 AND p.userid = $3 ORDER BY p.createat",
    )
    .bind(&name)
    .bind(since)
    .bind(&bot)
    .fetch_all(pool)
    .await
    .expect("the posts")
}

/// `install_plugin_notify_admin` — `DoCheckForAdminNotifications(false)`: every admin gets one
/// `custom_up_notification` post from the system bot naming the non-plugin features asked for
/// (the plugin one is not in it), the send is stamped in `Systems`, the plugin row is marked sent
/// and the others are deleted. The `posted` event the admin sees is compared too.
#[tokio::test]
async fn the_install_plugin_notify_admin_job_matches_gos_run() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let _rows = common::NOTIFY_ADMIN_ROWS.lock().await;
    let _jobs = common::JOB_RUNS.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let admin_id = common::logged_in_user_id().to_owned();
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let requester = create_plain_user(&http, &admin, &team, "njrequest").await;

    let mut outcomes = Vec::new();
    for on_rust in [false, true] {
        let server = if on_rust {
            Some(job_server(NOTIFY_ADMIN_JOB_SERVER_PORT).await)
        } else {
            None
        };
        let base = server
            .as_ref()
            .map_or(common::GO.to_owned(), |s| s.server.base.clone());
        plant_notify_admin_rows(&pool, &requester.id).await;
        let mut probe = common::SocketProbe::connect(&base, &admin).await;
        let since = mm_model::utils::get_millis();
        let job =
            insert_pending_job(&pool, "install_plugin_notify_admin", serde_json::json!({})).await;
        let (status, data) = wait_for_job(&pool, &job).await;
        if let Some(server) = &server {
            assert!(
                server.ran(&job),
                "this server's worker ran the job, not Go's"
            );
        }
        probe
            .collect_until(std::time::Duration::from_secs(5), |frames| {
                frames.iter().any(|f| {
                    f["event"] == "posted"
                        && f["data"]["post"]
                            .as_str()
                            .is_some_and(|p| p.contains("custom_up_notification"))
                })
            })
            .await;
        let events: Vec<serde_json::Value> = probe
            .events_named("posted")
            .into_iter()
            .filter(|f| {
                f["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains("custom_up_notification"))
            })
            .map(|mut f| {
                let mut post: serde_json::Value =
                    serde_json::from_str(f["data"]["post"].as_str().expect("a post"))
                        .expect("JSON");
                for key in ["id", "create_at", "update_at", "channel_id"] {
                    post[key] = serde_json::json!(key);
                }
                blank_create_at(&mut post["props"]);
                f["data"]["post"] = post;
                f.as_object_mut().map(|o| o.remove("seq"));
                f["broadcast"]["channel_id"] = serde_json::json!("<channel>");
                f
            })
            .collect();
        let stamped: Option<String> = sqlx::query_scalar(
            "SELECT value FROM systems WHERE name = 'LAST_UPGRADE_NOTIFICATION_TIMESTAMP'",
        )
        .fetch_optional(&pool)
        .await
        .expect("the stamp");
        let mut posts = up_notifications(&pool, &admin_id, since).await;
        for (_, _, props) in &mut posts {
            blank_create_at(props);
        }
        outcomes.push((
            status,
            data,
            posts,
            notify_admin_rows(&pool, &requester.id).await,
            stamped.is_some_and(|v| v.parse::<i64>().is_ok_and(|t| t >= since)),
            events,
        ));
        drop(server);
    }

    let (go, rs) = (&outcomes[0], &outcomes[1]);
    assert_eq!(go.0, "success", "{}", go.1);
    assert_eq!(go.2.len(), 1, "one post to this admin: {:?}", go.2);
    assert_eq!(
        go.3,
        vec![(
            "com.mmrs.alpha,com.mmrs.beta".to_owned(),
            "mattermost.feature.plugin.mmrsnj".to_owned(),
            true
        )],
        "the plugin row is kept and marked sent; the others are deleted"
    );
    assert!(go.4, "the send is stamped");
    assert_eq!(go.5.len(), 1, "the admin sees the post");
    assert_eq!(rs.0, go.0, "status");
    assert_eq!(rs.1, go.1, "job data");
    assert_eq!(rs.2, go.2, "the post");
    assert_eq!(rs.3, go.3, "the rows");
    assert_eq!(rs.4, go.4, "the stamp");
    assert_eq!(rs.5, go.5, "the posted event");

    sqlx::query("DELETE FROM notifyadmin WHERE userid = $1")
        .bind(&requester.id)
        .execute(&pool)
        .await
        .expect("cleanup");
    delete_plain_user(&http, &admin, &requester.id).await;
}

/// The planted rows' `create_at`, which each run plants afresh, inside the post's props.
fn blank_create_at(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if key == "create_at" {
                    *inner = serde_json::json!(0);
                } else {
                    blank_create_at(inner);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(blank_create_at),
        _ => {}
    }
}
