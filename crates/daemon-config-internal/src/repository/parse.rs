use super::types::RepositoryIndex;
use crate::{error::Result, parsing::read_non_empty_file};
use std::path::Path;

/// Parser for a repository's `peppy_repository.json5` index.
///
/// Unlike the item documents, an index is identified by its filename and its
/// position at the root of the scanned tree, so a caller never has to try
/// parsing one speculatively: it is either there and readable or reading it
/// is an error.
pub struct PeppyRepositoryIndexParser;

impl PeppyRepositoryIndexParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<RepositoryIndex> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    pub fn from_content(content: &str) -> Result<RepositoryIndex> {
        crate::error::deserialize_json5_with_path(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::{Error, ParsingError},
        repository::RepoItemKind,
    };
    use tempfile::NamedTempFile;

    const EVERY_KIND: &str = r#"{
        peppy_schema: "repository/v1",
        nodes: {
          "uvc_camera": {
            "v1": { path: "uvc_camera/rust/peppy.json5" },
            "v2": { path: "uvc_camera/python/peppy.json5" },
          },
        },
        launchers: { "openarm_v1_teleop": { path: "openarm/openarm_v1_teleop.json5" } },
        contracts: { "rgb_camera": { "v1": { path: "cameras/rgb_camera.json5" } } },
        pairings: { "joint_link": { "v1": { path: "robot/joint_link.json5" } } },
        mcp_exposures: { "camera_surface": { "v1": { path: "exposures/camera_surface.json5" } } },
    }"#;

    #[test]
    fn parses_every_kind_in_a_deterministic_order() {
        let index = PeppyRepositoryIndexParser::from_content(EVERY_KIND).expect("parses");
        let declared: Vec<String> = index
            .declared_items()
            .map(|item| format!("{item} -> {}", item.path))
            .collect();
        assert_eq!(
            declared,
            vec![
                "node `uvc_camera:v1` -> uvc_camera/rust/peppy.json5",
                "node `uvc_camera:v2` -> uvc_camera/python/peppy.json5",
                "launcher `openarm_v1_teleop` -> openarm/openarm_v1_teleop.json5",
                "contract `rgb_camera:v1` -> cameras/rgb_camera.json5",
                "pairing `joint_link:v1` -> robot/joint_link.json5",
                "mcp_exposure `camera_surface:v1` -> exposures/camera_surface.json5",
            ]
        );
    }

    /// A repository that publishes nothing is a valid repository. Only a
    /// missing index means it has not adopted one, and the two need
    /// different answers.
    #[test]
    fn an_empty_index_is_valid() {
        let index =
            PeppyRepositoryIndexParser::from_content(r#"{ peppy_schema: "repository/v1" }"#)
                .expect("an empty index parses");
        assert_eq!(index.declared_count(), 0);
    }

    #[test]
    fn missing_file_rejected() {
        assert!(matches!(
            PeppyRepositoryIndexParser::from_path("/path/does/not/exist.json5").unwrap_err(),
            Error::Parsing(ParsingError::CannotRead(..))
        ));
    }

    #[test]
    fn empty_file_rejected() {
        let tmp = NamedTempFile::new().expect("temp file");
        std::fs::write(tmp.path(), b"").expect("write");
        assert!(matches!(
            PeppyRepositoryIndexParser::from_path(tmp.path()).unwrap_err(),
            Error::Parsing(ParsingError::EmptyContent(_))
        ));
    }

    #[test]
    fn from_path_loads_file() {
        let tmp = NamedTempFile::new().expect("temp file");
        std::fs::write(tmp.path(), EVERY_KIND).expect("write");
        let index = PeppyRepositoryIndexParser::from_path(tmp.path()).expect("parses");
        assert_eq!(index.declared_count(), 6);
    }

    /// An index peppy does not understand is refused rather than read on a
    /// best-effort basis, which would resolve whatever subset it recognized
    /// and call that the repository's contents.
    #[test]
    fn an_unknown_index_version_is_refused() {
        let err = PeppyRepositoryIndexParser::from_content(r#"{ peppy_schema: "repository/v2" }"#)
            .expect_err("an unknown version must be refused");
        assert!(err.to_string().contains("repository/v2"), "{err}");
    }

    #[test]
    fn a_node_document_is_not_an_index() {
        let err = PeppyRepositoryIndexParser::from_content(
            r#"{ peppy_schema: "node/v1", manifest: { name: "a", tag: "v1" } }"#,
        )
        .expect_err("a node document must not parse as an index");
        assert!(err.to_string().contains("repository/v1"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_section() {
        let err = PeppyRepositoryIndexParser::from_content(
            r#"{ peppy_schema: "repository/v1", plugins: {} }"#,
        )
        .expect_err("an unknown section must be refused");
        assert!(err.to_string().contains("plugins"), "{err}");
    }

    /// The error names the identity and both files, because the point of
    /// refusing here is that the author can see which two claims collide.
    #[test]
    fn rejects_one_identity_claimed_twice() {
        let err = PeppyRepositoryIndexParser::from_content(
            r#"{
                 peppy_schema: "repository/v1",
                 nodes: {
                   "uvc_camera": {
                     "v1": { path: "rust/peppy.json5" },
                     "v1": { path: "python/peppy.json5" },
                   },
                 },
               }"#,
        )
        .expect_err("a repeated tag must be refused");
        let message = err.to_string();
        assert!(message.contains("duplicate key `v1`"), "{message}");
        assert!(message.contains("rust/peppy.json5"), "{message}");
        assert!(message.contains("python/peppy.json5"), "{message}");
        assert!(message.contains("nodes.uvc_camera"), "{message}");
    }

    #[test]
    fn rejects_a_path_that_leaves_the_repository() {
        let err = PeppyRepositoryIndexParser::from_content(
            r#"{
                 peppy_schema: "repository/v1",
                 nodes: { "a": { "v1": { path: "../../etc/passwd" } } },
               }"#,
        )
        .expect_err("an escaping path must be refused");
        assert!(err.to_string().contains("leaves the repository"), "{err}");
    }

    #[test]
    fn declares_each_kind_into_its_own_section() {
        let mut index = RepositoryIndex::default();
        let name = |raw: &str| super::super::types::ItemName::parse(raw).expect("valid name");
        let tag = |raw: &str| super::super::types::ItemTag::parse(raw).expect("valid tag");
        let path =
            |raw: &str| super::super::types::RepoRelativePath::parse(raw).expect("valid path");

        index
            .declare(
                RepoItemKind::Node,
                name("a"),
                Some(tag("v1")),
                path("a.json5"),
            )
            .expect("declares a node");
        index
            .declare(RepoItemKind::Launcher, name("b"), None, path("b.json5"))
            .expect("declares a launcher");
        assert_eq!(index.declared_count(), 2);

        let err = index
            .declare(
                RepoItemKind::Launcher,
                name("c"),
                Some(tag("v1")),
                path("c.json5"),
            )
            .expect_err("a launcher cannot carry a tag");
        assert!(err.contains("cannot carry a tag"), "{err}");

        let err = index
            .declare(RepoItemKind::Node, name("d"), None, path("d.json5"))
            .expect_err("a node needs a tag");
        assert!(err.contains("has no tag"), "{err}");
    }
}
