//! Cross-server parity for `GET /api/v4/teams/{team_id}/channels/autocomplete`
//! (`autocompleteChannelsForTeam`) — the Ctrl+K quick switcher, which fires once per keystroke.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_autocomplete
//! ```
//!
//! # Three answers a reader would not predict
//!
//! **`?name=*` is not a wildcard.** `sanitizeSearchTerm` strips `*` before escaping, so a term of
//! `*` sanitises to the empty string and the search clause is *omitted* — the same answer as no
//! term at all. [`a_star_is_the_same_request_as_no_term`].
//!
//! **Archived channels are in the list.** `includeDeleted` is hardcoded `true` in the app layer,
//! so the `DeleteAt = 0` predicate is never added. [`archived_channels_are_listed`].
//!
//! **The full-text half is load-bearing.** `?name=quick switcher` matches a channel named
//! `quick-switcher` through `to_tsquery` and through nothing else — no single column holds the
//! string with a space in it, so every `LIKE` fails.
//! [`a_two_word_term_matches_only_through_the_fulltext_clause`].

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, delete_channel, fetch_both_raw,
    fetch_both_stable, go_minted_token, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A team the plain user is **not** in, for the 403.
    other_team_id: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// Every channel this suite creates, tagged so the assertions can name them.
const OPEN: &str = "mmrs-parity-acopen";
const PRIVATE_MEMBER: &str = "mmrs-parity-acpriv";
const PRIVATE_STRANGER: &str = "mmrs-parity-acshut";
const ARCHIVED: &str = "mmrs-parity-acgone";
const TWO_WORD: &str = "mmrs-parity-acquick-switcher";
/// Its **display name** carries `aczz`; it sorts *after* [`PURPOSE_MATCH`] alphabetically.
const NAME_MATCH: &str = "mmrs-parity-aczz";
/// Only its **purpose** carries `aczz`; it sorts *before* [`NAME_MATCH`] alphabetically.
const PURPOSE_MATCH: &str = "mmrs-parity-acaa";

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "acteam").await;
            let other_team_id = create_team(client, token, "acother").await;

            create_channel_typed(client, token, &team_id, "acopen", "O").await;
            let member_priv = create_channel_typed(client, token, &team_id, "acpriv", "P").await;
            create_channel_typed(client, token, &team_id, "acshut", "P").await;
            create_channel_typed(client, token, &team_id, "acquick-switcher", "O").await;

            let archived = create_channel_typed(client, token, &team_id, "acgone", "O").await;
            delete_channel(client, token, &archived).await;

            // The `ORDER BY CASE` needs two channels that a plain alphabetical sort would put the
            // other way round: `acaa` sorts first by display name, but only `aczz` *matches* the
            // term in its display name, so the match-first clause has to lift `aczz` above it.
            create_channel_typed(client, token, &team_id, "aczz", "O").await;
            create_channel_with_purpose(client, token, &team_id, "acaa", "aczz in the purpose")
                .await;

            // In the team, and in one of the two private channels. The other stays invisible.
            //
            // **Not `"autoc"`.** `create_plain_user` builds the username as `mmrsplain{tag}`,
            // and `parity/users_autocomplete.rs` searches for the prefix `mmrsplainautoc` — so
            // that tag put an extra user in the middle of *its* corpus and failed three of its
            // tests in the full-suite run while passing in isolation. Tags are a shared
            // namespace across every suite in this binary.
            let plain = create_plain_user(client, token, &team_id, "chanac").await;
            add_user_to_channel(client, token, &member_priv, &plain.id).await;

            Fixture {
                team_id,
                other_team_id,
                plain_token: plain.token,
            }
        })
        .await
}

/// `common::create_channel_typed` sets no purpose, and the purpose column is one of the three
/// the search clause looks at — so a channel that matches on purpose *only* needs its own maker.
async fn create_channel_with_purpose(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
    purpose: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "purpose": purpose,
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the purpose fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

fn path(team_id: &str, name: &str) -> String {
    format!("/api/v4/teams/{team_id}/channels/autocomplete?name={name}")
}

fn names(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("decodes")
        .as_array()
        .expect("an array")
        .iter()
        .map(|c| c["name"].as_str().expect("a name").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// A term that matches several of this suite's channels, byte for byte — which also pins the
/// ordering, the column set, and the trailing newline.
#[tokio::test]
async fn a_matching_term_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "mmrs-parity-ac");
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
    for expected in [OPEN, PRIVATE_MEMBER, PRIVATE_STRANGER, ARCHIVED, TWO_WORD] {
        assert!(
            listed.contains(&expected.to_owned()),
            "{expected} must be in {listed:?} — the admin created every one of them"
        );
    }

    // `FillInChannelsProps` is deliberately *not* called on this route, unlike every other
    // channel list. A `props` key would be the visible sign that it was.
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    for channel in parsed.as_array().expect("an array") {
        assert!(
            channel.get("props").is_none() || channel["props"].is_null(),
            "autocomplete does not fill channel props: {channel}"
        );
    }
}

/// `sanitizeSearchTerm` removes `*` before escaping anything, so a term of `*` is a term of
/// nothing — and a nothing term omits the search clause rather than emptying it.
#[tokio::test]
async fn a_star_is_the_same_request_as_no_term() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let bare = path(&f.team_id, "");
    let (go_bare, rs_bare) = fetch_both_stable(&client, &token, &bare).await;
    assert_eq!(go_bare, rs_bare, "{bare} must be byte-identical");

    for term in ["*", "***"] {
        let starred = path(&f.team_id, term);
        let (go_star, rs_star) = fetch_both_stable(&client, &token, &starred).await;
        assert_eq!(go_star, rs_star, "{starred} must be byte-identical");
        assert_eq!(
            go_star, go_bare,
            "{starred} must answer exactly what the empty term answers"
        );
    }

    // And the list is not empty, or the equality above would be vacuous.
    assert!(
        !names(&go_bare).is_empty(),
        "the fixture team must have channels in it"
    );
}

/// `includeDeleted` is hardcoded true in `AutocompleteChannelsForTeam`, so the switcher lists
/// channels that no longer exist to anyone else.
#[tokio::test]
async fn archived_channels_are_listed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "acgone");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(names(&go), vec![ARCHIVED.to_owned()]);

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert!(
        parsed[0]["delete_at"].as_i64().unwrap_or(0) > 0,
        "and the one it returned really is archived"
    );
}

/// The `to_tsquery` half of the search clause. No single column contains "quick switcher" with a
/// space, so every `LIKE` misses and only the concatenated `tsvector` can match.
#[tokio::test]
async fn a_two_word_term_matches_only_through_the_fulltext_clause() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "acquick%20switcher");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![TWO_WORD.to_owned()],
        "the hyphenated name is two lexemes, and the term is two prefixes ANDed"
    );

    // The same two words in the other order still match — `to_tsquery` ANDs them, it does not
    // require adjacency. A LIKE-only port cannot produce this.
    let reversed = path(&f.team_id, "switcher%20acquick");
    let (go_rev, rs_rev) = fetch_both_stable(&client, &token, &reversed).await;
    assert_eq!(go_rev, rs_rev, "{reversed} must be byte-identical");
    assert_eq!(names(&go_rev), vec![TWO_WORD.to_owned()]);
}

/// A private channel the caller is not in is absent; one they are in is present. The store's
/// membership subquery is the only thing between them.
#[tokio::test]
async fn a_private_channel_needs_membership() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "mmrs-parity-ac");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let listed = names(&go);
    assert!(
        listed.contains(&PRIVATE_MEMBER.to_owned()),
        "the plain user is in {PRIVATE_MEMBER}: {listed:?}"
    );
    assert!(
        !listed.contains(&PRIVATE_STRANGER.to_owned()),
        "and not in {PRIVATE_STRANGER}: {listed:?}"
    );
    assert!(
        listed.contains(&OPEN.to_owned()),
        "public channels need no membership: {listed:?}"
    );
}

/// `list_team_channels` on the team. A non-member does not hold it.
#[tokio::test]
async fn a_non_member_of_the_team_is_a_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.other_team_id, "mmrs");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "the plain user is not in the other team");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // The same actor in a team they *are* in is served, so the 403 is the gate and not the token.
    let allowed = path(&f.team_id, "mmrs");
    let (go_ok, rs_ok) = fetch_both_stable(&client, &f.plain_token, &allowed).await;
    assert_eq!(go_ok, rs_ok, "{allowed} must be byte-identical");
}

/// There is no term validation on this route: `%` and `_` are escaped into literals rather than
/// becoming wildcards, and a term of pure punctuation is a legal request that matches nothing.
#[tokio::test]
async fn wildcard_characters_are_escaped_not_honoured() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for term in ["%25", "_", "%22", "%26", "%25%25"] {
        let p = path(&f.team_id, term);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 200, "{p}: no term is ever a 400");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{p} must be byte-identical"
        );
        assert!(
            names(&go_body).is_empty(),
            "{p}: an escaped wildcard matches nothing, it does not match everything"
        );
    }
}

/// The term is matched case-insensitively, on both halves of the clause.
#[tokio::test]
async fn the_term_is_case_insensitive() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let lower = path(&f.team_id, "acopen");
    let upper = path(&f.team_id, "ACOPEN");
    let (go_lower, rs_lower) = fetch_both_stable(&client, &token, &lower).await;
    let (go_upper, rs_upper) = fetch_both_stable(&client, &token, &upper).await;

    assert_eq!(go_lower, rs_lower, "{lower} must be byte-identical");
    assert_eq!(go_upper, rs_upper, "{upper} must be byte-identical");
    assert_eq!(go_lower, go_upper, "case must not change the answer");
    assert_eq!(names(&go_lower), vec![OPEN.to_owned()]);
}

/// `RequireTeamId`: alphanumeric, so the router's charset lets it through, but the wrong length.
#[tokio::test]
async fn a_team_id_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for id in ["short", "aaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let p = format!("/api/v4/teams/{id}/channels/autocomplete?name=x");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = format!("/api/v4/teams/{}/channels/autocomplete", f.team_id);
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
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
    let p = "/api/v4/teams/aaaaaaaaaaaaaaaaaaaaaaaaaa/channels/autocomplete?name=x";

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

/// `orderByDisplayNameMatch` (channel_store.go:3933): channels whose **display name** matches the
/// term come first, and only then does the alphabetical sort apply. Without the `CASE` the two
/// fixtures below come back the other way round, which is exactly what a mutation dropping it
/// produced — and what nothing in this suite noticed until this test existed.
#[tokio::test]
async fn a_display_name_match_sorts_above_an_alphabetically_earlier_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "aczz");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    assert_eq!(
        names(&go),
        vec![NAME_MATCH.to_owned(), PURPOSE_MATCH.to_owned()],
        "the display-name match leads, even though the other sorts first alphabetically"
    );

    // And the plain alphabetical order really is the other way, or the assertion above would
    // hold for a query with no `CASE` at all.
    let both = path(&f.team_id, "mmrs-parity-ac");
    let (go_all, _rs_all) = fetch_both_stable(&client, &token, &both).await;
    let listed = names(&go_all);
    let aa = listed.iter().position(|n| n == PURPOSE_MATCH);
    let zz = listed.iter().position(|n| n == NAME_MATCH);
    assert!(
        aa < zz,
        "with a term that matches neither display name, alphabetical order puts {PURPOSE_MATCH} \
         first: {listed:?}"
    );
}

/// The `LIKE` half of the search clause, exercised by a term the **full-text** half cannot
/// match: `to_tsquery` matches lexeme *prefixes*, so a term from the middle of a word reaches
/// the answer only through `LIKE '%…%'`.
///
/// Without this, dropping all three `LIKE` columns survives every other test in the file — the
/// full-text clause matches `acopen` for `?name=acopen` just as well, because `ac` is a prefix.
#[tokio::test]
async fn a_mid_word_term_matches_only_through_the_like_clause() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "copen");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![OPEN.to_owned()],
        "`copen` is inside `acopen` but starts no lexeme, so only the LIKE can find it"
    );
}

/// `strings.TrimSpace` in `AutocompleteChannelsForTeam` (channel.go:3402).
///
/// The term must be one only `LIKE` can match, for the same reason as the test above: an
/// untrimmed `  acopen  ` still matches through `to_tsquery`, because the fulltext term is built
/// by splitting on whitespace and the padding simply disappears. `  copen  ` matches through
/// neither clause unless the trim happens.
#[tokio::test]
async fn the_term_is_trimmed_before_it_reaches_the_store() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let padded = path(&f.team_id, "%20%20copen%20%20");
    let (go, rs) = fetch_both_stable(&client, &token, &padded).await;
    assert_eq!(go, rs, "{padded} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![OPEN.to_owned()],
        "the surrounding spaces are trimmed, so this is the same request as `?name=copen`"
    );
}
