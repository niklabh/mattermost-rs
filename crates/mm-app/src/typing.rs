//! Port of `app.App.PublishUserTyping` (app/user.go:2899), behind
//! `POST /users/{user_id}/typing`.

use std::collections::BTreeMap;

use mm_model::utils::AppResult;
use mm_model::websocket_message::{WEBSOCKET_EVENT_TYPING, WebSocketEvent};

use crate::App;

impl App {
    /// Port of `app.App.PublishUserTyping` (app/user.go:2899).
    ///
    /// One event and no store call. It is broadcast to the **channel**, with the typing user
    /// in `omit_users` so their own tabs do not see themselves typing, and carries exactly two
    /// keys: `parent_id` (the thread root, or `""`) and `user_id`. No team, no user targeting.
    ///
    /// Go's signature returns an `*AppError` that is always `nil`; the type is kept so the
    /// handler reads like its Go counterpart.
    #[tracing::instrument(
        skip(self),
        fields(user_id = %user_id, channel_id = %channel_id, parent_id = %parent_id)
    )]
    pub async fn publish_user_typing(
        &self,
        user_id: &str,
        channel_id: &str,
        parent_id: &str,
    ) -> AppResult<()> {
        let mut omit_users = BTreeMap::new();
        omit_users.insert(user_id.to_owned(), true);

        let mut event = WebSocketEvent::new(
            WEBSOCKET_EVENT_TYPING,
            "",
            channel_id,
            "",
            Some(omit_users),
            "",
        );
        event.add("parent_id", serde_json::Value::String(parent_id.to_owned()));
        event.add("user_id", serde_json::Value::String(user_id.to_owned()));
        self.publish(event).await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mm_model::websocket_message::{WEBSOCKET_EVENT_TYPING, WebSocketEvent};

    /// The event as `PublishUserTyping` builds it, serialised the way the hub sends it. Pinned
    /// here because the parity suite compares it against Go's frame, and this is the shape
    /// that comparison expects: channel broadcast, self omitted, two data keys.
    #[test]
    fn the_typing_event_omits_the_typist_and_carries_two_keys() {
        let mut omit = std::collections::BTreeMap::new();
        omit.insert("u".to_owned(), true);
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_TYPING, "", "c", "", Some(omit), "");
        event.add("parent_id", serde_json::Value::String("p".to_owned()));
        event.add("user_id", serde_json::Value::String("u".to_owned()));

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event"], "typing");
        assert_eq!(value["data"]["parent_id"], "p");
        assert_eq!(value["data"]["user_id"], "u");
        assert_eq!(value["data"].as_object().unwrap().len(), 2);
        assert_eq!(value["broadcast"]["channel_id"], "c");
        assert_eq!(value["broadcast"]["team_id"], "");
        assert_eq!(value["broadcast"]["user_id"], "");
        assert_eq!(value["broadcast"]["omit_users"]["u"], true);
    }
}
