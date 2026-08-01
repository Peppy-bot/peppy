//! The pin model a launch coordinator ships to every daemon in a launch.
//!
//! A pin states which bytes to run and where they can be read: the
//! fingerprint of the declaring file, plus the [`EntryOrigin`] the
//! coordinator resolved it from. A daemon holding content with the same
//! fingerprint reuses its own copy whatever repository it arrived from; one
//! that does not fetches the pinned commit. Nothing on the receiving side
//! resolves a name, so a machine's own cache freshness, repository
//! priorities and exclusions never influence a launch it participates in.
//!
//! A git origin is a fact about a shared repository that any machine can
//! fetch. A filesystem origin names a tree on the machine that minted the
//! pin, so it is valid only for that machine's own in-process adds; a pin
//! that crosses a machine boundary must carry a git origin, and the
//! receiving side refuses one that does not.
//!
//! Every field is a validating newtype, so serde-decoding a pin that crossed
//! a machine boundary IS the receiving side's structural validation: a
//! traversal path, a truncated commit or a malformed fingerprint fails at
//! decode, before any filesystem or network is touched.

use super::origin::EntryOrigin;
use super::types::{ItemName, ItemTag, ManifestFingerprint};
use serde::{Deserialize, Serialize, de};
use std::fmt;

/// Which section of a repository index a pinned identity lives in.
///
/// Launchers are absent on purpose: a launcher document is read once, by the
/// coordinator, and never crosses a machine boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinKind {
    Node,
    Contract,
    Pairing,
}

impl fmt::Display for PinKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Node => "node",
            Self::Contract => "contract",
            Self::Pairing => "pairing",
        })
    }
}

/// One pinned identity: what it is called, which bytes it is, and where to
/// read them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedItem {
    pub kind: PinKind,
    pub name: ItemName,
    pub tag: ItemTag,
    /// Fingerprint of the declaring file's bytes. Content is what authorises
    /// reuse: a daemon holding these bytes under any identity or repository
    /// uses its own copy and never fetches.
    pub sha256: ManifestFingerprint,
    /// Where the coordinator read the bytes, in the same vocabulary the
    /// repo caches record: the remote, the exact commit, and the path the
    /// repository's index declares for the identity.
    pub origin: EntryOrigin,
}

impl PinnedItem {
    /// How refusals name a pin: `node camera:v1`.
    pub fn label(&self) -> String {
        format!("{} `{}:{}`", self.kind, self.name, self.tag)
    }
}

/// Everything one deployment runs: the root node the launcher names and the
/// closure the coordinator resolved underneath it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentPins {
    /// The node the deployment names. Always [`PinKind::Node`], enforced at
    /// decode: a document whose root is a contract describes no deployment,
    /// and refusing it at the boundary beats tripping over it mid-add.
    pub root: PinnedItem,
    /// Every transitive node dependency of the root, and every contract and
    /// pairing document any manifest in the deployment names. A daemon adds
    /// from this list alone; an identity missing from it is a refusal, not a
    /// lookup.
    pub closure: Vec<PinnedItem>,
}

impl DeploymentPins {
    pub fn new(root: PinnedItem, closure: Vec<PinnedItem>) -> Result<Self, String> {
        if root.kind != PinKind::Node {
            return Err(format!(
                "a deployment's root pin must be a node, got {}",
                root.label()
            ));
        }
        // One identity, one content. The minting side already refuses a
        // closure that needs `name:tag` at two different fingerprints; stating
        // it here too means a document that arrived over the wire cannot carry
        // an ambiguity the adds would resolve by whichever pin they read
        // first. Repeated identical pins are fine: they are the same bytes.
        for (index, pin) in closure.iter().enumerate() {
            if let Some(other) = closure[..index].iter().find(|other| {
                other.kind == pin.kind && other.name == pin.name && other.tag == pin.tag
            }) && other.sha256 != pin.sha256
            {
                return Err(format!(
                    "{} is pinned at two different contents in one closure (`{}` and `{}`)",
                    pin.label(),
                    other.sha256,
                    pin.sha256
                ));
            }
        }
        Ok(Self { root, closure })
    }

    /// The root and its closure, root first.
    pub fn items(&self) -> impl Iterator<Item = &PinnedItem> {
        std::iter::once(&self.root).chain(self.closure.iter())
    }
}

impl<'de> Deserialize<'de> for DeploymentPins {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        /// The same shape without the root-kind rule, so the rule lives in
        /// [`DeploymentPins::new`] and decoding cannot bypass it.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            root: PinnedItem,
            #[serde(default)]
            closure: Vec<PinnedItem>,
        }

        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.root, raw.closure).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::repository::types::GitCommit;
    use crate::internal::repository::types::RepoRelativePath;

    fn pin(kind: PinKind, name: &str, tag: &str) -> PinnedItem {
        PinnedItem {
            kind,
            name: ItemName::parse(name).expect("valid name"),
            tag: ItemTag::parse(tag).expect("valid tag"),
            sha256: ManifestFingerprint::parse(&"a".repeat(64)).expect("valid sha"),
            origin: EntryOrigin::Git {
                repo_url: "https://github.com/acme/nodes-hub".to_owned(),
                repo_ref: Some("main".to_owned()),
                commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
                path: RepoRelativePath::parse("camera/peppy.json5").expect("valid path"),
            },
        }
    }

    #[test]
    fn deployment_pins_round_trip_through_json5() {
        let pins = DeploymentPins::new(
            pin(PinKind::Node, "camera", "v1"),
            vec![
                pin(PinKind::Node, "driver", "v2"),
                pin(PinKind::Contract, "frames", "v1"),
                pin(PinKind::Pairing, "joint_link", "v1"),
            ],
        )
        .expect("a node root");
        let encoded = serde_json5::to_string(&pins).expect("serialize");
        let decoded: DeploymentPins = serde_json5::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, pins);
    }

    /// A root pin as raw JSON5, with the identity fields filled in. The hex
    /// widths are generated rather than typed so the tests exercise the
    /// rules, not the author's counting.
    fn raw_root(kind: &str, path: &str, commit: &str, sha: &str, url: &str) -> String {
        format!(
            r#"{{ root: {{ kind: "{kind}", name: "camera", tag: "v1", sha256: "{sha}",
                      origin: {{ source_type: "git", repo_url: "{url}", commit: "{commit}",
                                 path: "{path}" }} }} }}"#
        )
    }

    #[test]
    fn an_absent_closure_decodes_as_empty() {
        let raw = raw_root(
            "node",
            "camera/peppy.json5",
            &"d".repeat(40),
            &"c".repeat(64),
            "https://example.com/hub",
        );
        let decoded: DeploymentPins = serde_json5::from_str(&raw).expect("decodes");
        assert!(decoded.closure.is_empty());
        assert!(matches!(
            &decoded.root.origin,
            EntryOrigin::Git { repo_ref: None, .. }
        ));
    }

    /// Decoding is the receiving side's structural validation, so a
    /// malformed pin from a hostile or buggy sender must fail here, before
    /// any filesystem or network is touched.
    #[test]
    fn decoding_refuses_a_pin_that_does_not_validate() {
        let good_sha = "c".repeat(64);
        let good_commit = "d".repeat(40);
        let cases = [
            (
                "a traversal path",
                raw_root(
                    "node",
                    "../../etc/passwd",
                    &good_commit,
                    &good_sha,
                    "https://x",
                ),
            ),
            (
                "an absolute path",
                raw_root("node", "/etc/passwd", &good_commit, &good_sha, "https://x"),
            ),
            (
                "an abbreviated commit",
                raw_root(
                    "node",
                    "camera/peppy.json5",
                    "dddddd",
                    &good_sha,
                    "https://x",
                ),
            ),
            (
                "a malformed fingerprint",
                raw_root(
                    "node",
                    "camera/peppy.json5",
                    &good_commit,
                    "not-a-sha",
                    "https://x",
                ),
            ),
        ];
        for (label, raw) in cases {
            assert!(
                serde_json5::from_str::<DeploymentPins>(&raw).is_err(),
                "{label} must be refused"
            );
        }
    }

    #[test]
    fn decoding_refuses_a_root_that_is_not_a_node() {
        let raw = raw_root(
            "contract",
            "frames.json5",
            &"d".repeat(40),
            &"c".repeat(64),
            "https://example.com/hub",
        );
        let err = serde_json5::from_str::<DeploymentPins>(&raw).expect_err("must be refused");
        assert!(err.to_string().contains("must be a node"), "{err}");
    }

    #[test]
    fn decoding_refuses_an_unknown_field() {
        let raw = raw_root(
            "node",
            "camera/peppy.json5",
            &"d".repeat(40),
            &"c".repeat(64),
            "https://example.com/hub",
        )
        .replacen('{', "{ extra: true,", 1);
        let err = serde_json5::from_str::<DeploymentPins>(&raw).expect_err("must be refused");
        assert!(err.to_string().contains("extra"), "{err}");
    }

    /// A filesystem origin decodes: it is how a machine pins its own
    /// in-process adds. What refuses it for a pin that crossed a machine
    /// boundary is the receiving daemon's reserve validation, not the codec.
    #[test]
    fn a_filesystem_origin_decodes() {
        let raw = format!(
            r#"{{ root: {{ kind: "node", name: "camera", tag: "v1", sha256: "{}",
                      origin: {{ source_type: "fs", path: "/nodes/camera/peppy.json5" }} }} }}"#,
            "c".repeat(64)
        );
        let decoded: DeploymentPins = serde_json5::from_str(&raw).expect("decodes");
        assert!(matches!(&decoded.root.origin, EntryOrigin::Fs { .. }));
    }

    #[test]
    fn items_yields_the_root_first() {
        let pins = DeploymentPins::new(
            pin(PinKind::Node, "camera", "v1"),
            vec![pin(PinKind::Contract, "frames", "v1")],
        )
        .expect("a node root");
        let labels: Vec<String> = pins.items().map(PinnedItem::label).collect();
        assert_eq!(labels, vec!["node `camera:v1`", "contract `frames:v1`"]);
    }
}
