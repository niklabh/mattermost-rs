//! Port of `app/usage.go` — the three counters behind `GET /api/v4/usage/{posts,storage,teams}`.
//!
//! All three are cloud billing telemetry that the webapp's admin console also renders, and all
//! three are deliberately imprecise: two of them push their answer through
//! [`crate::utils::round_off_to_zeroes_resolution`], and the third reads a materialized view.
//!
//! # The rounding is applied in two different layers, and that is not a mistake to tidy up
//!
//! `GetPostsUsage` rounds **in the app layer** (usage.go:21) at resolution 3. `getStorageUsage`
//! rounds **in the handler** (api4/usage.go:47) at resolution 8, over a value the app layer
//! returned raw. So `App::get_storage_usage` here returns bytes that have not been rounded, and
//! the handler in `mm-api` does it — same split, same place, because a caller added later must
//! meet the same behaviour Go's would.

use mm_model::usage::TeamsUsage;
use mm_model::utils::{AppError, AppResult};
use mm_store::{FileInfoStore, PostStore, TeamStore};

use crate::App;

/// The resolution `App.GetPostsUsage` rounds at (usage.go:21).
///
/// **Three, against the storage route's eight.** Named rather than inlined because the two are
/// applied in different layers and a reader comparing them has to find both; and because a
/// mutation swapping this for 8 is invisible on any server with fewer than ten thousand posts —
/// `min(zeroes, resolution)` clamps both to the same value — so the constant needs a unit test of
/// its own, which an inline literal cannot have.
const POSTS_RESOLUTION: i32 = 3;

/// Go's archived-team predicate (usage.go:49): `team.DeleteAt > 0 && team.CloudLimitsArchived`.
///
/// Two conditions, and on any server that has never been a cloud installation the corpus is
/// all-false — so a mutation dropping either half returns the same zero. It is a named function
/// so the truth table can be asserted directly; the parity fixture now also plants a deleted,
/// archived team so the route itself can tell the difference.
fn is_cloud_archived(team: &mm_model::team::Team) -> bool {
    team.delete_at > 0 && team.cloud_limits_archived
}

impl App {
    /// Port of `App.GetPostsUsage` (usage.go:15).
    ///
    /// The store call is `AnalyticsPostCount{ExcludeDeleted, UsersPostsOnly, AllowFromCache}`;
    /// only the first two reach the SQL. See
    /// [`mm_store::PostStore::analytics_posts_usage_count`].
    #[tracing::instrument(skip_all, fields(count, rounded))]
    pub async fn get_posts_usage(&self) -> AppResult<i64> {
        let count = self
            .store()
            .post()
            .analytics_posts_usage_count()
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "post count failed");
                AppError::boxed(
                    "GetPostsUsage",
                    "app.post.analytics_posts_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let rounded = crate::utils::round_off_to_zeroes_resolution(count as f64, POSTS_RESOLUTION);
        tracing::Span::current().record("count", count);
        tracing::Span::current().record("rounded", rounded);
        Ok(rounded)
    }

    /// Port of `App.GetStorageUsage` (usage.go:25).
    ///
    /// **Unrounded.** Go rounds this one in the handler, not here — see the module note.
    #[tracing::instrument(skip_all, fields(bytes))]
    pub async fn get_storage_usage(&self) -> AppResult<i64> {
        let bytes = self
            .store()
            .file_info()
            .get_storage_usage(false)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "storage usage lookup failed");
                AppError::boxed(
                    "GetStorageUsage",
                    "app.usage.get_storage_usage.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("bytes", bytes);
        Ok(bytes)
    }

    /// Port of `App.GetTeamsUsage` (usage.go:33).
    ///
    /// # Two numbers from two queries that disagree about deleted teams
    ///
    /// `active` is `AnalyticsTeamCount{IncludeDeleted: false}` — a `COUNT(*)` that excludes
    /// soft-deleted teams. `cloud_archived` is then computed by loading **every** team,
    /// deleted ones included, and counting those with `DeleteAt > 0 && CloudLimitsArchived`. The
    /// two are disjoint by construction, and a port that filtered the second query the way the
    /// first is filtered would report zero archived teams forever.
    ///
    /// # `GetAllTeams` is a full table read, and it is Go's
    ///
    /// A `COUNT(*) WHERE deleteat > 0 AND cloudlimitsarchived` would give the same integer with
    /// none of the rows. It is not written that way because `App.GetAllTeams` is the function Go
    /// calls, the loop below is the rule as Go states it, and the next caller of the archived
    /// count will want the teams and not the number. If this ever shows up in a profile, the
    /// place to fix it is the store, with a note saying which Go line it replaces.
    ///
    /// # The error id says `app.post.`
    ///
    /// Not `app.teams.` — `GetTeamsUsage` reports a team-count failure under the *post* analytics
    /// translation key (usage.go:38). The api4 handler beside it uses
    /// `app.teams.analytics_teams_count.app_error` for the same failure, so the two layers
    /// disagree. Reproduced as written; a client matching on the id sees Go's string.
    #[tracing::instrument(skip_all, fields(active, cloud_archived))]
    pub async fn get_teams_usage(&self) -> AppResult<TeamsUsage> {
        let opts = mm_model::team_search::TeamSearch {
            include_deleted: Some(false),
            ..Default::default()
        };
        let active = self
            .store()
            .team()
            .analytics_team_count(&opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "team count failed");
                AppError::boxed(
                    "GetTeamsUsage",
                    "app.post.analytics_teams_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let all = self.get_all_teams().await?;
        let cloud_archived = all.iter().filter(|team| is_cloud_archived(team)).count() as i64;

        tracing::Span::current().record("active", active);
        tracing::Span::current().record("cloud_archived", cloud_archived);
        Ok(TeamsUsage {
            active,
            cloud_archived,
        })
    }

    /// Port of `App.GetAllTeams` (app/team.go) — `Store().Team().GetAll()` and an error wrap.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_all_teams(&self) -> AppResult<Vec<mm_model::team::Team>> {
        let teams = self.store().team().get_all().await.map_err(|err| {
            tracing::error!(error = ?err, "team listing failed");
            AppError::boxed(
                "GetAllTeams",
                "app.team.get_all.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        tracing::Span::current().record("found", teams.len());
        Ok(teams)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::team::Team;

    fn team(delete_at: i64, archived: bool) -> Team {
        Team {
            delete_at,
            cloud_limits_archived: archived,
            ..Default::default()
        }
    }

    /// The resolution the posts route rounds at, pinned against the storage route's.
    ///
    /// `min(zeroes, resolution)` makes 3 and 8 indistinguishable for any count below ten thousand,
    /// which is every count this deployment can produce — so a mutation swapping them survives the
    /// whole parity suite. This is the only place that can catch it.
    #[test]
    fn the_posts_resolution_is_three_and_not_the_storage_route_s_eight() {
        assert_eq!(POSTS_RESOLUTION, 3);
        assert_eq!(
            crate::utils::round_off_to_zeroes_resolution(12_345_678.0, POSTS_RESOLUTION),
            12_345_000,
            "three trailing zeroes"
        );
        assert_ne!(
            crate::utils::round_off_to_zeroes_resolution(12_345_678.0, POSTS_RESOLUTION),
            crate::utils::round_off_to_zeroes_resolution(12_345_678.0, 8),
            "and eight would be a different number entirely"
        );
    }

    #[test]
    fn both_halves_of_the_archived_predicate_are_required() {
        assert!(is_cloud_archived(&team(1_788_000_000_000, true)));
        assert!(
            !is_cloud_archived(&team(0, true)),
            "a live team flagged archived does not count"
        );
        assert!(
            !is_cloud_archived(&team(1_788_000_000_000, false)),
            "an ordinary deleted team does not count"
        );
        assert!(!is_cloud_archived(&team(0, false)));
    }
}
