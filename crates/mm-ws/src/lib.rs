//! WebSocket hub ported from `server/channels/app/web_hub.go` and `web/websocket_*.go`.
//!
//! Depends on `mm-app`. Separate binary so socket fan-out can scale independently of REST.
