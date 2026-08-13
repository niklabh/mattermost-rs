//! REST API ported from `server/channels/api4/`.
//!
//! Fronts all traffic as the Strangler Fig proxy: routes that have not been migrated
//! yet are forwarded to the still-running Go server.
