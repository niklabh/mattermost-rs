//! Cross-server parity for `POST /api/v4/file/test` and `POST /api/v4/file/s3_test` — the
//! refusals, the local test over a directory both servers can and cannot write, and the S3
//! hand-over.
//!
//! ```sh
//! scripts/parity.sh --test parity file_store_test
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

/// The fixture's `FileSettings`, all sixty-three fields set, with the given overrides.
fn settings(overrides: &[(&str, serde_json::Value)]) -> String {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../../fixtures/config.json")).expect("JSON");
    let mut section = fixture["FileSettings"].clone();
    for (key, value) in overrides {
        section[*key] = value.clone();
    }
    serde_json::json!({ "FileSettings": section }).to_string()
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &str,
) -> (u16, Option<String>, Vec<u8>) {
    let response = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

async fn both(
    client: &reqwest::Client,
    path: &str,
    token: &str,
    body: &str,
    status: u16,
    id: Option<&str>,
) {
    let (go_status, _, go) = post(client, GO, path, token, body).await;
    let (rs_status, served, rs) = post(client, RUST, path, token, body).await;
    assert_eq!(
        go_status,
        status,
        "Go {path} {body}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {path} {body}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(served.as_deref(), Some("rust"), "{path}: served here");
    match id {
        Some(id) => {
            let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
            assert_eq!(parsed["id"], id, "{path} {body}");
            assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        }
        None => assert_eq!(go, rs, "{path}: byte for byte"),
    }
}

/// On both paths: the permission first, the nil-field 400, the four driver refusals, the
/// local test over a writable and an unwritable directory, and the S3 hand-over.
#[tokio::test]
async fn the_local_test_is_served_and_the_cloud_backends_are_gos() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "fst").await;
    let user = create_plain_user(&client, &admin, &team, "fst").await;
    let local = |dir: &str| settings(&[("DriverName", "local".into()), ("Directory", dir.into())]);

    for path in ["/api/v4/file/test", "/api/v4/file/s3_test"] {
        both(
            &client,
            path,
            &user.token,
            &local("/tmp"),
            403,
            Some("api.context.permissions.app_error"),
        )
        .await;
        both(
            &client,
            path,
            &user.token,
            r#"{"FileSettings":{}}"#,
            403,
            Some("api.context.permissions.app_error"),
        )
        .await;
        both(
            &client,
            path,
            &admin,
            r#"{"FileSettings":{"DriverName":"local"}}"#,
            400,
            Some("api.file.test_connection_settings_nil.app_error"),
        )
        .await;
        both(
            &client,
            path,
            &admin,
            &settings(&[("DriverName", "gcs".into())]),
            400,
            Some("api.file.test_connection_unsupported_driver.app_error"),
        )
        .await;
        both(
            &client,
            path,
            &admin,
            &settings(&[
                ("DriverName", "amazons3".into()),
                ("AmazonS3Bucket", "".into()),
            ]),
            400,
            Some("api.admin.test_s3.missing_s3_bucket"),
        )
        .await;
        both(
            &client,
            path,
            &admin,
            &settings(&[
                ("DriverName", "azureblob".into()),
                ("AzureStorageAccount", "".into()),
            ]),
            400,
            Some("api.admin.test_azure.missing_azure_field"),
        )
        .await;
        both(
            &client,
            path,
            &admin,
            &settings(&[
                ("DriverName", "azureblob".into()),
                ("AzureStorageAccount", "acct".into()),
                ("AzureContainer", "".into()),
            ]),
            400,
            Some("api.admin.test_azure.missing_azure_field"),
        )
        .await;

        both(&client, path, &admin, &local("/tmp"), 200, None).await;
        both(
            &client,
            path,
            &admin,
            &local("/proc/mmrs-nowhere"),
            500,
            Some("api.file.test_connection.app_error"),
        )
        .await;

        // A bucket named: forwarded, and Go's SDK answers whatever it answers.
        let (_, served, _) = post(
            &client,
            RUST,
            path,
            &admin,
            &settings(&[
                ("DriverName", "amazons3".into()),
                ("AmazonS3Bucket", "mmrs".into()),
            ]),
        )
        .await;
        assert_eq!(served.as_deref(), Some("go"), "{path}: S3 is Go's");
        let (_, served, _) = post(&client, RUST, path, &admin, "null").await;
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "{path}: the live config is Go's"
        );
    }
}
