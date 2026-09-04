//! Cross-server parity for `GET /api/v4/emoji/autocomplete` (`autocompleteEmojis`) — the `:`
//! picker, which fires once per keystroke.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity emoji_autocomplete
//! ```
//!
//! # Three answers a reader would not predict
//!
//! **It is case-sensitive.** There is no `LOWER` on either side of the `LIKE`, unlike the channel
//! autocomplete beside it. [`the_match_is_case_sensitive`].
//!
//! **It is a prefix match, not a substring one** — `prefixOnly` is hardcoded `true`, so the
//! pattern is `name%` and never `%name%`. [`it_is_a_prefix_match_not_a_substring`].
//!
//! **`?name=\` matches everything.** The sanitiser strips the escape character before escaping
//! `%` and `_` with it, so a lone backslash reduces to nothing and the pattern becomes the bare
//! `%`. [`a_lone_backslash_matches_everything`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_custom_emoji,
    delete_custom_emoji, fetch_both_raw, fetch_both_stable, go_minted_token, logged_in_user_id,
    purge_api_fixtures, stack_enabled,
};

struct Fixture {
    /// Three live emoji sharing [`Fixture::prefix`], in name order.
    first: String,
    second: String,
    third: String,
    /// A fourth that shares the prefix and has been deleted.
    deleted: String,
    /// The prefix all four share.
    prefix: String,
    /// `<prefix>u_<stamp>` — a literal underscore in a known position.
    underscored: String,
    /// `<prefix>ux<stamp>` — the same position, a letter. Distinguishes an escaped `_` from a
    /// wildcard one.
    lettered: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let creator = logged_in_user_id();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            // The `mmrsparity` prefix is what the SQL purge collects on; the stamp is what keeps
            // Go's thirty-minute name cache from refusing the next run.
            let prefix = format!("mmrsparityac{stamp}");

            // Created out of alphabetical order, so `ORDER BY Name` has something to do.
            let third = format!("{prefix}ccc");
            let first = format!("{prefix}aaa");
            let second = format!("{prefix}bbb");
            for name in [&third, &first, &second] {
                create_custom_emoji(client, token, creator, name).await;
            }

            // Deleted after creation. Go soft-deletes, so the row survives to be wrongly
            // returned by a query missing its `DeleteAt = 0`.
            let deleted = format!("{prefix}ddd");
            let deleted_id = create_custom_emoji(client, token, creator, &deleted).await;
            delete_custom_emoji(client, token, &deleted_id).await;

            // The underscore pair. `_` is inside the emoji-name validator, so both are legal
            // names — which is what makes the escaping observable at all.
            let underscored = format!("{prefix}u_x");
            let lettered = format!("{prefix}uyx");
            for name in [&underscored, &lettered] {
                create_custom_emoji(client, token, creator, name).await;
            }

            Fixture {
                first,
                second,
                third,
                deleted,
                prefix,
                underscored,
                lettered,
            }
        })
        .await
}

fn path(name: &str) -> String {
    format!("/api/v4/emoji/autocomplete?name={name}")
}

fn names(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("decodes")
        .as_array()
        .expect("an array")
        .iter()
        .map(|e| e["name"].as_str().expect("a name").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion: three matches in name order, the deleted one absent, and
/// the trailing newline `Encode` appends.
#[tokio::test]
async fn a_prefix_search_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.prefix);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );

    let listed = names(&go);
    assert_eq!(
        listed,
        vec![
            f.first.clone(),
            f.second.clone(),
            f.third.clone(),
            f.underscored.clone(),
            f.lettered.clone(),
        ],
        "ORDER BY Name — created out of order, returned in it"
    );
    assert!(
        !listed.contains(&f.deleted),
        "the soft-deleted emoji must not be a completion: {listed:?}"
    );
}

/// `SetInvalidURLParam("name")` — the *URL* variant, even though the value is a query parameter.
/// Absent and present-but-empty are the same thing to Go, which tests the string.
#[tokio::test]
async fn an_absent_or_empty_name_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [
        "/api/v4/emoji/autocomplete",
        "/api/v4/emoji/autocomplete?name=",
        "/api/v4/emoji/autocomplete?other=x",
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
        assert!(
            go["message"].as_str().unwrap_or_default().contains("name"),
            "the refused parameter is named: {}",
            go["message"]
        );
    }
}

/// No `LOWER` on either side of the `LIKE`. The channel autocomplete one route over *does* lower
/// both sides, which is exactly the kind of difference a port irons out by reflex.
#[tokio::test]
async fn the_match_is_case_sensitive() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let upper = path(&f.prefix.to_uppercase());
    let (go, rs) = fetch_both_stable(&client, &token, &upper).await;
    assert_eq!(go, rs, "{upper} must be byte-identical");
    assert_eq!(
        String::from_utf8_lossy(&go).trim_end(),
        "[]",
        "an upper-cased prefix matches nothing, and the empty answer is `[]` not `null`"
    );

    // The same prefix in its own case does match, so the assertion above is about the case and
    // not about a missing fixture.
    let lower = path(&f.prefix);
    let (go_lower, _rs_lower) = fetch_both_stable(&client, &token, &lower).await;
    assert!(!names(&go_lower).is_empty());
}

/// `prefixOnly` is hardcoded `true`, so the pattern is `name%`. A fragment from the middle of a
/// name matches nothing.
#[tokio::test]
async fn it_is_a_prefix_match_not_a_substring() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Drop the first character: still a substring of every fixture name, no longer a prefix.
    let fragment = &f.prefix[1..];
    let p = path(fragment);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        String::from_utf8_lossy(&go).trim_end(),
        "[]",
        "`{fragment}` is inside every fixture name and starts none of them"
    );
}

/// `sanitizeSearchTerm` escapes `_` with a backslash, and `sq.Like` emits no `ESCAPE` clause —
/// so Postgres' default backslash escape is what turns it into a literal.
#[tokio::test]
async fn an_underscore_is_escaped_rather_than_treated_as_a_wildcard() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Unescaped, `_` is a single-character wildcard and this would match `…uyx` too.
    let p = path(&format!("{}u_", f.prefix));
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![f.underscored.clone()],
        "the underscore is a literal; `{}` must not match",
        f.lettered
    );

    // And the wildcard interpretation really would have matched both, so the assertion above is
    // about the escaping rather than about a fixture that does not exist.
    let both = path(&format!("{}u", f.prefix));
    let (go_both, _rs) = fetch_both_stable(&client, &token, &both).await;
    assert_eq!(
        names(&go_both),
        vec![f.underscored.clone(), f.lettered.clone()],
        "one character earlier, the prefix matches both"
    );
}

/// The escape character is stripped *before* the wildcards are escaped with it, so a lone
/// backslash sanitises to nothing and the prefix pattern becomes the bare `%`.
#[tokio::test]
async fn a_lone_backslash_matches_everything() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path("%5C");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let listed = names(&go);
    assert!(
        listed.contains(&f.first),
        "a bare `%` pattern lists every live emoji, this suite's included: {listed:?}"
    );
    assert!(
        !listed.contains(&f.deleted),
        "still not the deleted one — that predicate is not part of the term"
    );
}

/// A prefix nothing carries: `[]`, never `null`.
#[tokio::test]
async fn an_unmatched_prefix_is_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let p = path("zzznosuchemojiprefix");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        "[]\n",
        "the store initialises the slice, so the empty answer is not `null`"
    );
    assert_eq!(go, rs, "{p} must be byte-identical");
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(
                method.clone(),
                format!("{RUST}/api/v4/emoji/autocomplete?name=x"),
            )
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} /emoji/autocomplete must be forwarded"
        );
    }
}

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = "/api/v4/emoji/autocomplete?name=x";

    let go = client
        .get(format!("{GO}{p}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{p}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{p}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, p);
}
