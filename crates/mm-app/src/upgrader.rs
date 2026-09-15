//! Port of `platform/services/upgrader` — the checks behind `POST /api/v4/upgrade_to_enterprise`
//! and its `/allowed` and `/status` reads.
//!
//! # What is ported, and what cannot be
//!
//! `CanIUpgradeToE0` is the whole of the decision the three routes make on this host: the
//! operating system and architecture, then whether the process may replace its own binary —
//! the directory's owner and its write bits, with the two usernames the error carries — then the
//! `BuildEnterpriseReady` constant. Every one of those is a fact about *this* process and is
//! answered about this process.
//!
//! `UpgradeToE0` itself — download `mattermost-<version>-linux-amd64.tar.gz`, verify the
//! detached signature against the embedded key, swap `mattermost/bin/mattermost` over the
//! running executable — is a Go-binary procedure with no meaning for this server: there is no
//! enterprise build of it to fetch. So the upgrade is never started here, the percentage stays
//! at zero and the status never carries an error, and the handler forwards the one arm that
//! would have started it (see `mm_api::sysops::upgrade_to_enterprise`). The `!linux` build of the
//! Go package answers `InvalidArch` for all three functions; the Linux build is what is ported.

use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// `upgrader.InvalidPermissions.ErrType`, the three spellings `upgradeToEnterprise` switches on.
pub const INVALID_USER_AND_PERMISSION: &str = "invalid-user-and-permission";
pub const INVALID_USER: &str = "invalid-user";
pub const INVALID_PERMISSION: &str = "invalid-permission";

/// The errors of `upgrader/errors.go` plus the plain `errors.New` ones the checks return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpgradeError {
    /// `upgrader.InvalidArch`.
    #[error("invalid operating system or processor architecture")]
    InvalidArch,
    /// `upgrader.InvalidPermissions`.
    #[error("the user {mattermost_username} is unable to update the {path} file")]
    InvalidPermissions {
        err_type: &'static str,
        path: String,
        file_username: String,
        mattermost_username: String,
    },
    /// The `errors.New` strings of `canIWriteTheExecutable` and `CanIUpgradeToE0`.
    #[error("{0}")]
    Other(String),
}

/// `CanIUpgradeToE0`'s result: the error, wrapped as Go wraps it.
///
/// `errors.Wrap(err, "unable to upgrade from TE to E0")` prefixes the message of everything
/// `canIUpgrade` returns; the already-enterprise refusal is a bare `errors.New` and is not
/// prefixed. `isAllowedToUpgradeToEnterprise` puts the **whole rendered string** on the wire
/// as the error id, which is why the wrapping is modelled rather than logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CannotUpgrade {
    pub cause: UpgradeError,
    wrapped: bool,
}

impl CannotUpgrade {
    /// `err.Error()` — the string Go's `switch` falls through to as an error id.
    pub fn message(&self) -> String {
        if self.wrapped {
            format!("unable to upgrade from TE to E0: {}", self.cause)
        } else {
            self.cause.to_string()
        }
    }
}

/// Port of `upgrader.CanIUpgradeToE0` (upgrader_linux.go:197): the arch and OS, the executable's
/// directory, then the build flag — in that order, so an arm64 host never reads a directory.
pub fn can_i_upgrade_to_e0() -> Result<(), CannotUpgrade> {
    if let Err(cause) = can_i_upgrade() {
        return Err(CannotUpgrade {
            cause,
            wrapped: true,
        });
    }
    if mm_model::version::BUILD_ENTERPRISE_READY == "true" {
        tracing::warn!("Unable to upgrade from TE to E0. The server is already running E0.");
        return Err(CannotUpgrade {
            cause: UpgradeError::Other(
                "you cannot upgrade your server from TE to E0 because you are already running Mattermost Enterprise Edition".to_owned(),
            ),
            wrapped: false,
        });
    }
    Ok(())
}

/// Port of `upgrader.UpgradeToE0Status` (upgrader_linux.go:246): the percentage and the last
/// error of an upgrade — and no upgrade ever runs here, so `(0, None)`.
pub fn upgrade_to_e0_status() -> (i64, Option<String>) {
    (0, None)
}

/// Port of `canIUpgrade` (upgrader_linux.go:188). `runtime.GOARCH == "amd64"` is Rust's
/// `x86_64`; the OS check is second.
fn can_i_upgrade() -> Result<(), UpgradeError> {
    if std::env::consts::ARCH != "x86_64" {
        return Err(UpgradeError::InvalidArch);
    }
    if std::env::consts::OS != "linux" {
        return Err(UpgradeError::InvalidArch);
    }
    can_i_write_the_executable()
}

/// Port of `canIWriteTheExecutable` (upgrader_linux.go:150).
///
/// It stats the executable's **directory**, not the file: a rename-and-create needs the
/// directory writable. The three refusals are, in order, "not the owner and neither the owner
/// nor everyone may write", "not the owner, everyone may not but the owner may", and "the
/// owner, who may not". `1<<7` is the owner's write bit and `1<<1` everyone's.
fn can_i_write_the_executable() -> Result<(), UpgradeError> {
    let executable = std::env::current_exe()
        .map_err(|_| UpgradeError::Other("error getting the path of the executable".to_owned()))?;
    let dir = executable
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| executable.clone());
    let info = std::fs::metadata(&dir)
        .map_err(|_| UpgradeError::Other("error getting the executable info".to_owned()))?;
    let file_uid = info.uid();
    let file_user = username_of(file_uid)
        .ok_or_else(|| UpgradeError::Other("error getting the executable info".to_owned()))?;
    // SAFETY: `getuid` takes no arguments and cannot fail.
    let mattermost_uid = unsafe { libc::getuid() };
    let mattermost_user = username_of(mattermost_uid)
        .ok_or_else(|| UpgradeError::Other("error getting the executable info".to_owned()))?;

    let mode = info.mode();
    let other_write = mode & (1 << 1) != 0;
    let owner_write = mode & (1 << 7) != 0;
    let path = dir.to_string_lossy().into_owned();
    let refuse = |err_type: &'static str| UpgradeError::InvalidPermissions {
        err_type,
        path: path.clone(),
        file_username: file_user.clone(),
        mattermost_username: mattermost_user.clone(),
    };
    if file_uid != mattermost_uid && !other_write && !owner_write {
        return Err(refuse(INVALID_USER_AND_PERMISSION));
    }
    if file_uid != mattermost_uid && !other_write && owner_write {
        return Err(refuse(INVALID_USER));
    }
    if file_uid == mattermost_uid && !owner_write {
        return Err(refuse(INVALID_PERMISSION));
    }
    Ok(())
}

/// Port of `user.LookupId(strconv.Itoa(uid)).Username`: `getpwuid_r`, `None` when the id has no
/// passwd entry — Go's `UnknownUserIdError`.
fn username_of(uid: libc::uid_t) -> Option<String> {
    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: every pointer is to memory this function owns and outlives the call; `buffer`'s
    // length is passed as its size; `result` is written with either null or `&passwd`.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut passwd,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || passwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` is a NUL-terminated string inside `buffer`, which is still alive.
    let name = unsafe { CStr::from_ptr(passwd.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapped form is `errors.Wrap`'s `"<msg>: <cause>"`; the bare form is the cause.
    #[test]
    fn the_message_is_wrapped_only_for_can_i_upgrade_errors() {
        let wrapped = CannotUpgrade {
            cause: UpgradeError::InvalidArch,
            wrapped: true,
        };
        assert_eq!(
            wrapped.message(),
            "unable to upgrade from TE to E0: invalid operating system or processor architecture"
        );
        let bare = CannotUpgrade {
            cause: UpgradeError::Other("x".to_owned()),
            wrapped: false,
        };
        assert_eq!(bare.message(), "x");
    }

    /// `InvalidPermissions.Error()` names the process's user and the directory, not the file's
    /// owner.
    #[test]
    fn the_permissions_error_reads_as_gos() {
        let err = UpgradeError::InvalidPermissions {
            err_type: INVALID_USER,
            path: "/opt/mattermost/bin".to_owned(),
            file_username: "root".to_owned(),
            mattermost_username: "mattermost".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "the user mattermost is unable to update the /opt/mattermost/bin file"
        );
    }

    /// On this host the answer is the architecture, before any directory is read.
    #[test]
    fn a_non_amd64_linux_host_is_invalid_arch() {
        let result = can_i_upgrade_to_e0();
        if std::env::consts::ARCH != "x86_64" || std::env::consts::OS != "linux" {
            assert_eq!(result.map_err(|e| e.cause), Err(UpgradeError::InvalidArch));
        }
    }

    /// The running user always has a passwd entry; `u32::MAX` never does.
    #[test]
    fn the_username_lookup_is_getpwuid() {
        // SAFETY: no arguments, cannot fail.
        let me = unsafe { libc::getuid() };
        assert!(username_of(me).is_some_and(|name| !name.is_empty()));
        assert_eq!(username_of(u32::MAX), None);
    }
}
