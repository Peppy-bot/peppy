mod add;
mod exclude;
mod list;
mod refresh;
mod remove;

pub use add::listen_for_repo_add;
pub use exclude::listen_for_repo_exclude;
pub use list::listen_for_repo_list;
pub use refresh::listen_for_repo_refresh;
pub use remove::listen_for_repo_remove;

/// Guards read-modify-write cycles on repositories.json5 and
/// excluded_repositories.json5 to prevent concurrent corruption.
pub(crate) fn repos_file_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}
