//! Owner-only file modes for the secrets peppy keeps on disk: tokens, the
//! peer's private key, and the request headers of the log export. One
//! definition of "private" for every store of them.

use std::path::Path;

#[cfg(unix)]
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
pub fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
pub fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
