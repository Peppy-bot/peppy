//! `GET {api_url}/cli-config` — the public bootstrap endpoint that hands the CLI
//! the Zitadel `issuer`, the Native app `client_id`, the `project_id`, and the
//! exact `scopes` string to request (already including `offline_access` and the
//! project-audience scope — sent to Zitadel **verbatim**, never reassembled).

use serde::Deserialize;

use super::http::HttpClient;
use crate::error::{Error, Result};

/// The four fields the backend serves to the CLI (no endpoint URLs — those come
/// from OIDC discovery against `issuer`).
#[derive(Debug, Clone, Deserialize)]
pub struct CliConfig {
    pub issuer: String,
    pub client_id: String,
    pub project_id: String,
    pub scopes: String,
}

/// Fetches `/cli-config`. A `503` means the deployment hasn't provisioned the
/// CLI client yet (`PEPPY_CLI_CLIENT_ID` / `PEPPY_INTROSPECT_AUDIENCE` unset).
pub fn fetch(http: &HttpClient, api_url: &str) -> Result<CliConfig> {
    let url = format!("{}/cli-config", api_url.trim_end_matches('/'));
    let resp = http.get(&url, None)?;
    match resp.status {
        200 => resp.json("/cli-config"),
        503 => Err(Error::Auth(
            "CLI login isn't configured on this backend yet (the deployment hasn't provisioned the CLI client).".to_string(),
        )),
        s => Err(Error::Http(format!("GET {url} returned {s}"))),
    }
}
