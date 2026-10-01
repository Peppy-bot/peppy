//! Owner-only file modes for the secrets this crate writes (tokens, the peer's
//! private key). Shared by the credential and enrollment stores so the two
//! cannot drift on what "private" means.

use std::path::Path;

#[cfg(unix)]
pub(crate) fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
pub(crate) fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
pub(crate) fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
