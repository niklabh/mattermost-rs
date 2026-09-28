//! Port of `app/toast.go`: `App.SendToastMessage`, which only the plugin API calls.

use mm_model::utils::{AppError, AppResult, StringInterface};
use mm_model::websocket_message::{WEBSOCKET_EVENT_SHOW_TOAST, WebSocketEvent, WebsocketBroadcast};

use crate::App;

impl App {
    /// Port of `App.SendToastMessage` (app/toast.go:13): a `show_toast` event to one user,
    /// narrowed to one of their connections when `connection_id` is not empty. The data is the
    /// message and the position, and nothing is checked but that the user and the message are
    /// not empty — each a 400 of its own.
    pub async fn send_toast_message(
        &self,
        user_id: &str,
        connection_id: &str,
        message: &str,
        position: &str,
    ) -> AppResult {
        if user_id.is_empty() {
            return Err(AppError::boxed(
                "SendToastMessage",
                "app.toast.send_toast_message.user_id.app_error",
                None,
                "",
                400,
            ));
        }
        if message.is_empty() {
            return Err(AppError::boxed(
                "SendToastMessage",
                "app.toast.send_toast_message.message.app_error",
                None,
                "",
                400,
            ));
        }
        let mut payload = StringInterface::new();
        payload.insert("message".to_owned(), message.into());
        payload.insert("position".to_owned(), position.into());
        let event = WebSocketEvent::new(WEBSOCKET_EVENT_SHOW_TOAST, "", "", user_id, None, "")
            .set_broadcast(WebsocketBroadcast {
                user_id: user_id.to_owned(),
                connection_id: connection_id.to_owned(),
                ..WebsocketBroadcast::default()
            })
            .set_data(payload);
        self.publish(event).await;
        Ok(())
    }
}
