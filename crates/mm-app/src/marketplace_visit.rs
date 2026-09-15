//! The `FirstAdminVisitMarketplace` system row behind `GET` and `POST
//! /api/v4/plugins/marketplace/first_admin_visit` (api4/plugin.go:434-492).
//!
//! The two handlers read and write the `Systems` table directly — there is no `App` method in
//! Go — so what lives here is the store-error mapping they do inline, and the websocket event
//! the setter publishes. Neither touches the plugin host: the row records whether the first
//! administrator has opened the marketplace page, and that is all.

use mm_model::system::{SYSTEM_FIRST_ADMIN_VISIT_MARKETPLACE, System};
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{
    WEBSOCKET_FIRST_ADMIN_VISIT_MARKETPLACE_STATUS_RECEIVED, WebSocketEvent,
};
use mm_store::error::StoreError;
use mm_store::system_store::SystemStore;

use crate::App;

impl App {
    /// The read half of `getFirstAdminVisitMarketplaceStatus` (plugin.go:472-486).
    ///
    /// A missing row is **synthesised as `"false"`**, not reported: `store.ErrNotFound` is the
    /// one error the handler swallows, and everything else is the 500
    /// `api.error_get_first_admin_visit_marketplace_status`. The row's `Value` column is
    /// nullable and a NULL is returned by the store as an absent value, which Go's scanner would
    /// refuse; nothing on either server writes one, so the two collapse into the same `"false"`.
    #[tracing::instrument(skip_all, fields(value))]
    pub async fn first_admin_visit_marketplace_status(&self) -> AppResult<System> {
        let value = self
            .store()
            .system()
            .get_by_name(SYSTEM_FIRST_ADMIN_VISIT_MARKETPLACE)
            .await
            .map_err(|err| {
                AppError::boxed(
                    "getFirstAdminVisitMarketplaceStatus",
                    "api.error_get_first_admin_visit_marketplace_status",
                    None,
                    err.to_string(),
                    500,
                )
            })?
            .unwrap_or_else(|| "false".to_owned());
        tracing::Span::current().record("value", &value);
        Ok(System {
            name: SYSTEM_FIRST_ADMIN_VISIT_MARKETPLACE.to_owned(),
            value,
        })
    }

    /// The write half of `setFirstAdminVisitMarketplaceStatus` (plugin.go:443-457): the row is
    /// upserted to the literal `"true"` — the request body is never read — and every connected
    /// client is told so through `first_admin_visit_marketplace_status_received`, whose one
    /// datum is that same string.
    #[tracing::instrument(skip_all)]
    pub async fn set_first_admin_visit_marketplace_status(&self) -> AppResult<()> {
        let row = System {
            name: SYSTEM_FIRST_ADMIN_VISIT_MARKETPLACE.to_owned(),
            value: "true".to_owned(),
        };
        self.store()
            .system()
            .save_or_update(&row.name, &row.value)
            .await
            .map_err(|err: StoreError| {
                AppError::boxed(
                    "setFirstAdminVisitMarketplaceStatus",
                    "api.error_set_first_admin_visit_marketplace_status",
                    None,
                    err.to_string(),
                    500,
                )
            })?;

        // `NewWebSocketEvent(…, "", "", "", nil, "")`: no team, channel or user, so the hub
        // sends it to everyone.
        let mut message = WebSocketEvent::new(
            WEBSOCKET_FIRST_ADMIN_VISIT_MARKETPLACE_STATUS_RECEIVED,
            "",
            "",
            "",
            None,
            "",
        );
        message.add(
            "firstAdminVisitMarketplaceStatus",
            serde_json::Value::String(row.value),
        );
        self.publish(message).await;
        Ok(())
    }
}
