//! The app half of `manualtesting` (channels/manualtesting) — `GET /manualtest`, registered only
//! under `ServiceSettings.EnableTesting`. The handler is `mm_api::manualtest`.

use mm_model::channel::ChannelSearchOpts;
use mm_store::channel_store::ChannelStore;

use crate::App;

impl App {
    /// Port of `manualtesting.getChannelID` (manual_testing.go:190): the id of the first channel
    /// named `channel_name` among `GetChannels(team_id, user_id, {IncludeDeleted: false})`.
    ///
    /// `None` for a store failure as well as for no match — Go logs and returns `false` for both,
    /// and `GetChannels` reports zero rows as an error of its own. With the empty team and user
    /// ids `ManualTest` passes when it created neither, the team filter is dropped and only a
    /// membership row whose `UserId` is `''` can match.
    #[tracing::instrument(skip(self))]
    pub async fn manual_test_channel_id(
        &self,
        channel_name: &str,
        team_id: &str,
        user_id: &str,
    ) -> Option<String> {
        let opts = ChannelSearchOpts {
            include_deleted: false,
            last_delete_at: 0,
            ..Default::default()
        };
        let channels = match self
            .store()
            .channel()
            .get_channels(team_id, user_id, &opts)
            .await
        {
            Ok(channels) => channels,
            Err(err) => {
                tracing::debug!(error = %err, "Unable to get channels");
                return None;
            }
        };
        let found = channels
            .0
            .into_iter()
            .find(|channel| channel.name == channel_name)
            .map(|channel| channel.id);
        if found.is_none() {
            tracing::debug!(channel_name, "Could not find channel");
        }
        found
    }
}
