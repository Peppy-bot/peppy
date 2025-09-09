use crate::error::Result;
use config::{NodeConfig, NodeConfigParser};
use std::future::Future;
use std::path::Path;

pub fn spin_from_config_file(configuration_file: impl AsRef<Path>) -> Result<()> {
    let content = std::fs::read_to_string(configuration_file)?;
    spin_from_config_content(&content)
}

pub fn spin_from_config_content(content: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(spin_from_config_content_async(content))
}

pub async fn spin_from_config_content_async(content: &str) -> Result<()> {
    let config = NodeConfigParser::from_content(content)?;
    // until Ctrl-C
    spin_node_until(config, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

async fn spin_node_until<F>(_config: NodeConfig, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::pin!(shutdown);
    // Placeholder: when node wiring exists, launch tasks and select! with shutdown
    shutdown.await;
    Ok(())
}

#[cfg(test)]
mod tests {}
