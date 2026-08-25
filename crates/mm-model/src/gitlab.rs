//! Port of `model/gitlab.go` — one constant.

/// Port of `model.UserAuthServiceGitlab` (gitlab.go:4).
///
/// The value goes in `User.AuthService`, beside `USER_AUTH_SERVICE_EMAIL` and friends in
/// `user.rs`. Go keeps it in its own file because the GitLab OAuth provider used to live there;
/// the split is preserved so the origin stays legible.
pub const USER_AUTH_SERVICE_GITLAB: &str = "gitlab";
