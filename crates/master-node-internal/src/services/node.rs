mod add;
mod info;
mod init;
mod remove;
mod start;
mod stop;
mod sync;
mod templates;

use crate::encoding::NodeSource;
use config::node::NodeConfig;

pub use add::listen_for_node_add;
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
pub use remove::listen_for_node_remove;
pub use start::listen_for_node_start;
pub use stop::listen_for_node_stop;
pub use sync::listen_for_node_sync;

pub(super) async fn resolve_node_config(
    source: NodeSource,
) -> std::result::Result<NodeConfig, String> {
    info::resolve_node_config(source).await
}
