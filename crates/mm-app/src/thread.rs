//! Port of the threads read in `server/channels/app/user.go`.

use mm_model::thread::Threads;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::thread_store::ThreadStore;
use mm_store::user_store::UserStore;

use crate::App;

impl App {
    /// Port of `app.App.GetThreadsForUser` (app/user.go:2984), for the option set the api4
    /// handler serves — a team, the default page, and `?extended` — with everything else
    /// forwarded upstream. See `mm_api::users::get_threads_for_user`.
    ///
    /// # Four counters and a list, and Go runs all five at once
    ///
    /// `errgroup` fans them out and the first failure collapses the lot into **one** error id,
    /// `app.user.get_threads_for_user.app_error` / 500 — so a client cannot tell which query
    /// broke. Run sequentially here: the concurrency is a latency decision, not a wire one, and
    /// five borrowed futures over one pool is a complication with nothing to show for it on this
    /// deployment's data. Recorded rather than hidden.
    ///
    /// # `TotalUnreadUrgentMentions` is asked for only when post priority is on
    ///
    /// And `IncludeIsUrgent` — the per-thread `is_urgent` column — is set from the same config
    /// flag (app/user.go:2987). With the feature off, Go asks neither, and both answer Go's zero
    /// value; that is reproduced by passing the flag down rather than by always joining.
    ///
    /// # Participants are sanitised as a **non-admin**, whoever asks
    ///
    /// `sanitizeThreadResponse` calls `sanitizeProfiles(thread.Participants, false)` — the
    /// literal `false`, not `c.IsSystemAdmin()`. So a system admin reading their own threads
    /// sees participants sanitised exactly as an ordinary user would, which is the opposite of
    /// every other route that hydrates users.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, extended))]
    pub async fn get_threads_for_user(
        &self,
        user_id: &str,
        team_id: &str,
        page_size: i64,
        extended: bool,
    ) -> AppResult<Threads> {
        let include_is_urgent = self.config().post_priority;

        let wrap = |err: mm_store::error::StoreError| {
            tracing::error!(error = %err, "threads lookup failed");
            AppError::boxed(
                "GetThreadsForUser",
                "app.user.get_threads_for_user.app_error",
                None,
                String::new(),
                500,
            )
        };

        let total_unread_threads = self
            .store()
            .thread()
            .get_total_unread_threads(user_id, team_id)
            .await
            .map_err(wrap)?;
        let total = self
            .store()
            .thread()
            .get_total_threads(user_id, team_id)
            .await
            .map_err(wrap)?;
        let total_unread_mentions = self
            .store()
            .thread()
            .get_total_unread_mentions(user_id, team_id)
            .await
            .map_err(wrap)?;
        let total_unread_urgent_mentions = if include_is_urgent {
            self.store()
                .thread()
                .get_total_unread_urgent_mentions(user_id, team_id)
                .await
                .map_err(wrap)?
        } else {
            0
        };

        let mut threads = self
            .store()
            .thread()
            .get_threads_for_user(user_id, team_id, page_size, include_is_urgent)
            .await
            .map_err(wrap)?;

        if extended {
            self.hydrate_thread_participants(&mut threads).await?;
        }

        // `sanitizeThreadResponse`'s post half (app/user.go:3108). The participant half needs
        // the api layer's config-driven options and happens there, as it does for every other
        // route that hydrates users.
        for thread in &mut threads {
            if let Some(post) = thread.post.as_mut() {
                post.sanitize_props();
                post.strip_action_integrations();
            }
        }

        Ok(Threads {
            total,
            total_unread_threads,
            total_unread_mentions,
            total_unread_urgent_mentions,
            threads: Some(threads),
        })
    }

    /// `?extended=true`: replace the id-only participant stubs with real profiles.
    ///
    /// Go does this **inside the store**, with one `GetProfileByIds` over the de-duplicated ids
    /// of the whole page (thread_store.go:392). Done here instead because this crate is where
    /// the user store and the sanitiser already meet; the query and its de-duplication are the
    /// same, and a thread whose participant has since been hard-deleted loses that participant
    /// from its list on both servers — the map lookup simply misses.
    async fn hydrate_thread_participants(
        &self,
        threads: &mut [mm_model::thread::ThreadResponse],
    ) -> AppResult<()> {
        let mut ids: Vec<String> = threads
            .iter()
            .flat_map(|thread| {
                thread
                    .participants
                    .iter()
                    .flatten()
                    .map(|user| user.id.clone())
            })
            .collect();
        mm_model::utils::remove_duplicate_strings(&mut ids);
        if ids.is_empty() {
            return Ok(());
        }

        let profiles = self
            .store()
            .user()
            .get_profile_by_ids(&ids, 0)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "thread participant lookup failed");
                AppError::boxed(
                    "GetThreadsForUser",
                    "app.user.get_threads_for_user.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        let by_id: std::collections::HashMap<String, User> = profiles
            .into_iter()
            .map(|user| (user.id.clone(), user))
            .collect();

        for thread in threads.iter_mut() {
            if let Some(participants) = thread.participants.as_mut() {
                *participants = participants
                    .iter()
                    .filter_map(|stub| by_id.get(&stub.id).cloned())
                    .collect();
            }
        }
        Ok(())
    }
}
