//! This machine's platform enrollment: the record of the router peer `peppy
//! platform enroll` minted, plus the material the daemon needs to join the
//! project's cloud router. Everything lives under `<root>/conf/peer/`:
//!
//! * `enrollment.json5`: the typed [`EnrollmentDocument`] (`0600`).
//! * `peer.key`: the peer's private key, PKCS#8 PEM (`0600`).
//! * `peer.crt`: the signed leaf certificate followed by its issuing chain.
//! * `ca.crt`: the project CA the cloud router's certificate chains to.
//! * `zenohd.platform.json5`: the peer config the platform rendered, kept for
//!   inspection only. The daemon renders its own config from the document.
//!
//! The daemon reads the directory at startup and on every control-socket poke;
//! the CLI writes it on `enroll` and removes it on `unenroll`. An absent
//! `enrollment.json5` means "not enrolled", and a present one is parsed in
//! full before anything uses it: an unsupported version, an id zenoh would
//! refuse, or a missing key file fails the load rather than reaching a router.

use std::path::{Path, PathBuf};

use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use pmi::{ConnectIdentity, RouterId, TlsConfig, ZenohEndpoint, ZenohNetProtocol};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::client::RouterPeerEnrolled;
use crate::error::{Error, Result};
use crate::fs_perms::{restrict_dir, restrict_file};

/// On-disk schema version of `enrollment.json5`. Bumped on any shape change;
/// there is no reader for an older version. A file of another version is
/// rejected by [`load`], and the machine enrolls again.
pub const ENROLLMENT_VERSION: u32 = 1;
pub const ENROLLMENT_FILE: &str = "enrollment.json5";
pub const PEER_KEY_FILE: &str = "peer.key";
pub const PEER_CERTIFICATE_FILE: &str = "peer.crt";
pub const TRUST_ANCHOR_FILE: &str = "ca.crt";
pub const PLATFORM_ZENOH_CONFIG_FILE: &str = "zenohd.platform.json5";

/// Where the enrolled machine's router dials: the project's cloud router.
///
/// Constructed only through [`RouterEndpoint::parse`], which runs the pair
/// through pmi's locator parser, the single source of what a `tls/` endpoint
/// may look like, so [`RouterEndpoint::locator`] can never fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRouterEndpoint", into = "RawRouterEndpoint")]
pub struct RouterEndpoint {
    host: String,
    port: u16,
}

#[derive(Serialize, Deserialize)]
struct RawRouterEndpoint {
    host: String,
    port: u16,
}

impl RouterEndpoint {
    pub fn parse(host: &str, port: u16) -> Result<Self> {
        let locator = format!("{}/{host}:{port}", ZenohNetProtocol::Tls);
        let endpoint: ZenohEndpoint = locator
            .parse()
            .map_err(|e| Error::Auth(format!("invalid router endpoint {locator:?}: {e}")))?;
        Ok(Self {
            host: endpoint.host().to_string(),
            port: endpoint.port(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `tls/<host>:<port>` locator the router's `connect` block dials.
    pub fn locator(&self) -> String {
        format!("{}/{}:{}", ZenohNetProtocol::Tls, self.host, self.port)
    }
}

impl TryFrom<RawRouterEndpoint> for RouterEndpoint {
    type Error = Error;

    fn try_from(raw: RawRouterEndpoint) -> Result<Self> {
        Self::parse(&raw.host, raw.port)
    }
}

impl From<RouterEndpoint> for RawRouterEndpoint {
    fn from(endpoint: RouterEndpoint) -> Self {
        Self {
            host: endpoint.host,
            port: endpoint.port,
        }
    }
}

/// The persisted enrollment record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentDocument {
    /// Schema version (see [`ENROLLMENT_VERSION`]). Defaults to `0` when absent
    /// so an unversioned file is rejected rather than half-interpreted.
    #[serde(default)]
    pub version: u32,
    /// The platform API the peer was enrolled against.
    pub api_url: String,
    pub workspace_id: String,
    pub project_id: String,
    /// The platform's id for this peer, the handle `unenroll` deletes.
    pub peer_id: String,
    /// The certificate's common name, and the peer's display name.
    pub peer_name: String,
    /// The transport identity the daemon's router runs under. The platform
    /// marks the peer connected only while a session with exactly this id is
    /// present, so the daemon never mints its own while enrolled.
    pub zenoh_id: RouterId,
    /// The project's routing namespace, applied to every application session
    /// the daemon and its nodes open.
    pub namespace: Namespace,
    pub router: RouterEndpoint,
    /// The leaf certificate's expiry, unix seconds. The platform issues no
    /// renewal; past this the cloud router refuses the link.
    pub certificate_expires_at: i64,
    /// When this record was written, unix seconds.
    pub enrolled_at: i64,
}

impl EnrollmentDocument {
    /// Whether the certificate is at or past expiry at `now_unix`.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix >= self.certificate_expires_at
    }
}

/// A loaded enrollment: the document plus the absolute paths of the material,
/// each checked to exist at load so the daemon never renders a config naming a
/// file that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub document: EnrollmentDocument,
    pub peer_key: PathBuf,
    pub peer_certificate: PathBuf,
    pub trust_anchor: PathBuf,
}

impl Enrollment {
    /// The upstream the daemon's router federates to: the cloud router's
    /// locator and the mutual-TLS material to dial it with.
    pub fn federation_target(&self) -> (String, TlsConfig) {
        (
            self.document.router.locator(),
            TlsConfig::mtls_client(
                self.trust_anchor.clone(),
                ConnectIdentity {
                    certificate: self.peer_certificate.clone(),
                    private_key: self.peer_key.clone(),
                },
            ),
        )
    }
}

/// Everything `enroll` writes: the document and the PEM material.
pub struct EnrollmentBundle {
    pub document: EnrollmentDocument,
    pub peer_key_pem: SecretString,
    /// The leaf followed by its issuing chain, as the router presents it.
    pub peer_certificate_pem: String,
    pub trust_anchor_pem: String,
    pub platform_zenoh_config: String,
}

impl EnrollmentBundle {
    /// Builds the bundle from the platform's enrollment response. The router
    /// endpoint is read from the rendered config's single `connect` entry; the
    /// leaf and chain are joined into one PEM bundle so the router presents the
    /// whole path to the project CA.
    #[allow(clippy::too_many_arguments)]
    pub fn from_platform(
        api_url: &str,
        workspace_id: &str,
        project_id: &str,
        enrolled: RouterPeerEnrolled,
        peer_key_pem: SecretString,
        enrolled_at: i64,
    ) -> Result<Self> {
        let router = router_endpoint_from_zenoh_config(&enrolled.zenoh_config)?;
        let document = EnrollmentDocument {
            version: ENROLLMENT_VERSION,
            api_url: api_url.trim_end_matches('/').to_string(),
            workspace_id: workspace_id.to_string(),
            project_id: project_id.to_string(),
            peer_id: enrolled.peer.id,
            peer_name: enrolled.peer.name,
            zenoh_id: enrolled.zenoh_id,
            namespace: enrolled.namespace,
            router,
            certificate_expires_at: enrolled.peer.certificate_expires_at.timestamp(),
            enrolled_at,
        };
        let mut peer_certificate_pem = enrolled.certificate.trim_end().to_string();
        peer_certificate_pem.push('\n');
        let chain = enrolled.chain.trim();
        if !chain.is_empty() {
            peer_certificate_pem.push_str(chain);
            peer_certificate_pem.push('\n');
        }
        Ok(Self {
            document,
            peer_key_pem,
            peer_certificate_pem,
            trust_anchor_pem: enrolled.trust_anchor,
            platform_zenoh_config: enrolled.zenoh_config,
        })
    }
}

/// The cloud router named by the platform's rendered peer config: exactly one
/// `tls/` entry under `connect.endpoints`.
pub fn router_endpoint_from_zenoh_config(zenoh_config: &str) -> Result<RouterEndpoint> {
    let malformed = |reason: String| {
        Error::Auth(format!(
            "the platform's peer config names no usable router: {reason}"
        ))
    };
    let config: serde_json::Value = serde_json5::from_str(zenoh_config)
        .map_err(|e| malformed(format!("it does not parse as JSON5 ({e})")))?;
    let endpoints = config["connect"]["endpoints"]
        .as_array()
        .ok_or_else(|| malformed("it has no `connect.endpoints` list".to_string()))?;
    let [endpoint] = endpoints.as_slice() else {
        return Err(malformed(format!(
            "expected exactly one connect endpoint, found {}",
            endpoints.len()
        )));
    };
    let locator = endpoint
        .as_str()
        .ok_or_else(|| malformed("the connect endpoint is not a string".to_string()))?;
    let parsed: ZenohEndpoint = locator
        .parse()
        .map_err(|e| malformed(format!("{locator:?} is not a locator ({e})")))?;
    if parsed.protocol() != ZenohNetProtocol::Tls {
        return Err(malformed(format!("{locator:?} is not a tls/ locator")));
    }
    RouterEndpoint::parse(parsed.host(), parsed.port())
}

fn document_path(dirs: &PeppyDirs) -> PathBuf {
    dirs.peer_dir().join(ENROLLMENT_FILE)
}

/// Loads this machine's enrollment. `Ok(None)` when `enrollment.json5` is
/// absent (not enrolled). An error when it is present but unreadable, of
/// another version, or missing any of its three PEM files; every message names
/// `peppy platform enroll` as the remedy.
pub fn load(dirs: &PeppyDirs) -> Result<Option<Enrollment>> {
    let path = document_path(dirs);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let document: EnrollmentDocument = serde_json5::from_str(&content).map_err(|e| {
        Error::Auth(format!(
            "failed to parse {}: {e}; run `peppy platform enroll` again",
            path.display()
        ))
    })?;
    if document.version != ENROLLMENT_VERSION {
        return Err(Error::Auth(format!(
            "enrollment file {} is an unsupported format (v{}, expected v{}); \
             run `peppy platform enroll` again",
            path.display(),
            document.version,
            ENROLLMENT_VERSION
        )));
    }
    let peer_dir = dirs.peer_dir();
    let material = |name: &str| -> Result<PathBuf> {
        let file = peer_dir.join(name);
        if !file.is_file() {
            return Err(Error::Auth(format!(
                "enrollment material {} is missing; run `peppy platform enroll` again",
                file.display()
            )));
        }
        Ok(file)
    };
    Ok(Some(Enrollment {
        peer_key: material(PEER_KEY_FILE)?,
        peer_certificate: material(PEER_CERTIFICATE_FILE)?,
        trust_anchor: material(TRUST_ANCHOR_FILE)?,
        document,
    }))
}

/// Writes the bundle under `<root>/conf/peer/` (`0700`), the key and the
/// document owner-only. Each file is published atomically, and the document
/// last: a crash before it lands leaves no record, so the machine reads as
/// not enrolled and the next `enroll` overwrites the partial material.
pub fn save(dirs: &PeppyDirs, bundle: &EnrollmentBundle) -> Result<()> {
    let peer_dir = dirs.peer_dir();
    std::fs::create_dir_all(&peer_dir)?;
    restrict_dir(&peer_dir)?;
    // `conf/` holds the credentials too; keep it owner-only as `storage` does.
    if let Some(conf) = peer_dir.parent() {
        restrict_dir(conf)?;
    }

    publish(
        &peer_dir.join(PEER_KEY_FILE),
        bundle.peer_key_pem.expose_secret(),
        true,
    )?;
    publish(
        &peer_dir.join(PEER_CERTIFICATE_FILE),
        &bundle.peer_certificate_pem,
        false,
    )?;
    publish(
        &peer_dir.join(TRUST_ANCHOR_FILE),
        &bundle.trust_anchor_pem,
        false,
    )?;
    publish(
        &peer_dir.join(PLATFORM_ZENOH_CONFIG_FILE),
        &bundle.platform_zenoh_config,
        false,
    )?;
    let document = json5_pretty::to_string_pretty(&bundle.document)
        .map_err(|e| Error::Auth(format!("failed to serialize the enrollment: {e}")))?;
    publish(&peer_dir.join(ENROLLMENT_FILE), &document, true)
}

fn publish(path: &Path, content: &str, owner_only: bool) -> Result<()> {
    daemon_config::atomic_write::publish_atomic(path, |tmp| {
        std::fs::write(tmp, content)?;
        if owner_only {
            restrict_file(tmp)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Removes the whole peer directory. Absent is not an error: the machine is
/// then simply not enrolled.
pub fn remove(dirs: &PeppyDirs) -> Result<()> {
    match std::fs::remove_dir_all(dirs.peer_dir()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// The namespace the enrollment under `dirs` resolves to: the project's when
/// enrolled, `local` otherwise. The same resolution the daemon does at startup,
/// so the CLI can confirm the daemon came back under what it just wrote.
pub fn namespace(dirs: &PeppyDirs) -> Result<Namespace> {
    Ok(load(dirs)?
        .map(|enrollment| enrollment.document.namespace)
        .unwrap_or_else(Namespace::local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RouterPeer;
    use crate::storage::secret;

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ZID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";

    fn platform_config(endpoints: &str) -> String {
        format!(
            r#"{{ mode: "router", id: "{ZID}", connect: {{ endpoints: [{endpoints}],
                timeout_ms: -1 }}, listen: {{ endpoints: {{ router: ["tcp/127.0.0.1:7448"] }} }},
                adminspace: {{ enabled: false }} }}"#
        )
    }

    fn enrolled() -> RouterPeerEnrolled {
        RouterPeerEnrolled {
            peer: RouterPeer {
                id: "peer-1".into(),
                name: "robot-7".into(),
                certificate_cn: "robot-7".into(),
                status: "unknown".into(),
                certificate_expires_at: "2027-01-01T00:00:00Z".parse().unwrap(),
                created_at: "2026-10-03T00:00:00Z".parse().unwrap(),
            },
            certificate: "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n".into(),
            chain: "-----BEGIN CERTIFICATE-----\nissuer\n-----END CERTIFICATE-----\n\n".into(),
            trust_anchor: "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n".into(),
            zenoh_id: RouterId::parse(ZID).unwrap(),
            namespace: Namespace::parse(PROJECT).unwrap(),
            zenoh_config: platform_config(r#""tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447""#),
        }
    }

    fn bundle() -> EnrollmentBundle {
        EnrollmentBundle::from_platform(
            "https://api.example.test/",
            "ws-1",
            PROJECT,
            enrolled(),
            secret("-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n".into()),
            1_700_000_000,
        )
        .expect("a well-formed response converts")
    }

    #[test]
    fn the_bundle_is_built_from_the_platform_response() {
        let bundle = bundle();
        let doc = &bundle.document;
        assert_eq!(doc.version, ENROLLMENT_VERSION);
        assert_eq!(doc.api_url, "https://api.example.test");
        assert_eq!(doc.workspace_id, "ws-1");
        assert_eq!(doc.project_id, PROJECT);
        assert_eq!(doc.peer_id, "peer-1");
        assert_eq!(doc.peer_name, "robot-7");
        assert_eq!(doc.zenoh_id.as_str(), ZID);
        assert_eq!(doc.namespace.as_str(), PROJECT);
        assert_eq!(doc.router.host(), "rtr-p.us-east-1.robocloud.dev.peppy.bot");
        assert_eq!(doc.router.port(), 7447);
        assert_eq!(
            doc.router.locator(),
            "tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447"
        );
        assert_eq!(doc.certificate_expires_at, 1_798_761_600);
        assert_eq!(doc.enrolled_at, 1_700_000_000);
        assert_eq!(
            bundle.peer_certificate_pem,
            "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\nissuer\n-----END CERTIFICATE-----\n",
            "the leaf comes first, then the chain, with no blank lines between"
        );
    }

    #[test]
    fn the_router_endpoint_must_be_the_single_tls_connect_entry() {
        for (endpoints, reason) in [
            ("", "exactly one connect endpoint"),
            (r#""tls/a:1", "tls/b:2""#, "exactly one connect endpoint"),
            (r#""tcp/rtr.example:7447""#, "not a tls/ locator"),
            ("42", "not a string"),
        ] {
            let err = router_endpoint_from_zenoh_config(&platform_config(endpoints))
                .expect_err(endpoints);
            assert!(err.to_string().contains(reason), "{endpoints}: {err}");
        }
        assert!(router_endpoint_from_zenoh_config("not json").is_err());
    }

    #[test]
    fn save_then_load_round_trips_and_locks_down_the_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let bundle = bundle();

        save(&dirs, &bundle).expect("save");
        let loaded = load(&dirs).expect("load").expect("enrolled");
        assert_eq!(loaded.document, bundle.document);
        assert_eq!(loaded.peer_key, dirs.peer_dir().join(PEER_KEY_FILE));
        assert_eq!(
            std::fs::read_to_string(&loaded.peer_certificate).unwrap(),
            bundle.peer_certificate_pem
        );
        assert_eq!(
            std::fs::read_to_string(&loaded.trust_anchor).unwrap(),
            bundle.trust_anchor_pem
        );
        assert_eq!(
            std::fs::read_to_string(dirs.peer_dir().join(PLATFORM_ZENOH_CONFIG_FILE)).unwrap(),
            bundle.platform_zenoh_config
        );
        assert_eq!(namespace(&dirs).unwrap().as_str(), PROJECT);

        let (locator, tls) = loaded.federation_target();
        assert_eq!(locator, "tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447");
        assert_eq!(tls.root_ca_certificate, Some(loaded.trust_anchor.clone()));
        assert_eq!(
            tls.connect_identity,
            Some(ConnectIdentity {
                certificate: loaded.peer_certificate.clone(),
                private_key: loaded.peer_key.clone(),
            })
        );
        assert!(tls.verify_name_on_connect);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dirs.peer_dir()), 0o700);
            assert_eq!(mode(&loaded.peer_key), 0o600);
            assert_eq!(mode(&dirs.peer_dir().join(ENROLLMENT_FILE)), 0o600);
        }

        remove(&dirs).expect("remove");
        assert!(load(&dirs).unwrap().is_none());
        assert_eq!(namespace(&dirs).unwrap(), Namespace::local());
        remove(&dirs).expect("removing twice is fine");
    }

    #[test]
    fn an_absent_enrollment_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        assert!(load(&dirs).unwrap().is_none());
        assert_eq!(namespace(&dirs).unwrap(), Namespace::local());
    }

    /// Every way the directory can be present but unusable fails the load with
    /// the remedy named, rather than reaching the daemon's router.
    #[test]
    fn a_broken_enrollment_fails_the_load_naming_enroll() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        save(&dirs, &bundle()).unwrap();
        let document_file = dirs.peer_dir().join(ENROLLMENT_FILE);
        let good = std::fs::read_to_string(&document_file).unwrap();

        for (label, content) in [
            ("version", good.replace("version: 1", "version: 2")),
            ("leading-zero zid", good.replace(ZID, "0abc")),
            ("uppercase zid", good.replace(ZID, "ABC")),
            ("namespace", good.replace(PROJECT, "**")),
            (
                "host",
                good.replace("rtr-p.us-east-1.robocloud.dev.peppy.bot", ""),
            ),
            ("syntax", "{ not json5".to_string()),
        ] {
            std::fs::write(&document_file, content).unwrap();
            let err = load(&dirs).expect_err(label);
            assert!(
                err.to_string().contains("peppy platform enroll"),
                "{label}: {err}"
            );
        }

        std::fs::write(&document_file, &good).unwrap();
        std::fs::remove_file(dirs.peer_dir().join(PEER_KEY_FILE)).unwrap();
        let err = load(&dirs).expect_err("missing key");
        assert!(err.to_string().contains(PEER_KEY_FILE), "{err}");
    }

    #[test]
    fn expiry_is_a_plain_comparison_against_the_given_clock() {
        let doc = bundle().document;
        assert!(!doc.is_expired(doc.certificate_expires_at - 1));
        assert!(doc.is_expired(doc.certificate_expires_at));
    }

    #[test]
    fn router_endpoints_are_validated_on_read() {
        assert!(RouterEndpoint::parse("", 7447).is_err());
        assert!(serde_json::from_str::<RouterEndpoint>(r#"{"host":"","port":1}"#).is_err());
        let endpoint: RouterEndpoint =
            serde_json::from_str(r#"{"host":"rtr.example","port":7447}"#).unwrap();
        assert_eq!(endpoint.locator(), "tls/rtr.example:7447");
    }
}
