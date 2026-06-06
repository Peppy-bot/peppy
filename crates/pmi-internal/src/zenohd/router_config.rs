//! Generation of the zenohd router's own config file. This is the daemon's
//! deployment config (how to run the router), distinct from the client session
//! configs that live with the messaging adapter.

use super::ZenohNetProtocol;
use crate::error::{Error, Result};
use askama::Template;
use std::path::PathBuf;

#[derive(Template)]
#[template(
    source = r#"{
    "mode": "router",
    "listen": {
        "endpoints": {
            "router": ["{{ protocol }}/{{ host }}:{{ port }}"]
        }
    },
    "timestamping": {
        "enabled": { "router": true },
        "drop_future_timestamp": false
    }
}"#,
    ext = "txt"
)]
struct ZenohRouterConfigTemplate {
    host: String,
    port: u16,
    protocol: ZenohNetProtocol,
}

/// Resolves the zenohd router config path. Honors a `ZENOH_CONFIG` override;
/// otherwise renders a router config to a temp file keyed by messaging port and
/// returns its path.
pub(crate) fn router_config_path(
    protocol: ZenohNetProtocol,
    host: &str,
    messaging_port: u16,
) -> Result<PathBuf> {
    if let Ok(config_path) = std::env::var("ZENOH_CONFIG") {
        return Ok(PathBuf::from(config_path));
    }

    let config_path = std::env::temp_dir().join(format!("zenohd_config_{}.json5", messaging_port));

    let config_content = ZenohRouterConfigTemplate {
        host: host.to_string(),
        port: messaging_port,
        protocol,
    }
    .render()
    .map_err(|e| {
        Error::ConfigurationError(format!("Failed to render zenohd config template: {}", e))
    })?;

    std::fs::write(&config_path, config_content)
        .map_err(|e| Error::ConfigurationError(format!("Failed to write zenohd config: {}", e)))?;

    Ok(config_path)
}
