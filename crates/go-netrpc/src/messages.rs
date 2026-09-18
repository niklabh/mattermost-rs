//! The two headers of net/rpc (server.go:172-186).

use gobwire::Gob;

/// Written before every call's argument.
#[derive(Gob, Debug, Default, Clone, PartialEq, Eq)]
pub struct Request {
    /// `"Service.Method"`.
    #[gob(name = "ServiceMethod")]
    pub service_method: String,
    /// Chosen by the client, echoed by the server.
    #[gob(name = "Seq")]
    pub seq: u64,
}

/// Written before every reply.
#[derive(Gob, Debug, Default, Clone, PartialEq, Eq)]
pub struct Response {
    #[gob(name = "ServiceMethod")]
    pub service_method: String,
    #[gob(name = "Seq")]
    pub seq: u64,
    /// Non-empty when the call failed; the reply is then an [`InvalidRequest`].
    #[gob(name = "Error")]
    pub error: String,
}

/// The reply sent in place of a failed call's result: Go's `invalidRequest = struct{}{}`.
#[derive(Gob, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRequest {}
