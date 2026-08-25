use config::schema::PeppySchema;
use core_node_api::encoding::RepoItemKind;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    fmt,
    marker::PhantomData,
    path::{Component, Path},
};
use thiserror::Error;

/// A repository-relative path to the file that declares one item.
///
/// Every path peppy resolved from used to be produced by its own directory
/// walk. An index path arrives instead from a file somebody merged, so it is
/// untrusted input and the rules below are enforced at parse time: a value of
/// this type is already known to stay inside the repository.
///
/// Parsing is canonical, which is what lets `peppy repo index --check`
/// compare a committed path against a freshly generated one with `==`
/// instead of reporting `./a.json5` as a move of `a.json5`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RepoRelativePath(String);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RepoPathError {
    #[error("path is empty")]
    Empty,
    #[error(
        "path contains a backslash, which separates directories on Windows \
         and is an ordinary filename character elsewhere: {0}"
    )]
    Backslash(String),
    #[error("path leaves the repository: {0}")]
    NotInside(String),
    #[error("path is not normalized (it contains `.` or a repeated separator): {0}")]
    NotNormalized(String),
    #[error("path names a directory, but an entry must name the file that declares the item: {0}")]
    NotAFile(String),
}

impl RepoRelativePath {
    /// The one statement of the path rules. Both the generator and
    /// `Deserialize` go through it, so a generated index can never hold a
    /// path that reading an index would refuse.
    pub fn parse(raw: &str) -> Result<Self, RepoPathError> {
        let path = raw.trim();
        if path.is_empty() {
            return Err(RepoPathError::Empty);
        }
        if path.contains('\\') {
            return Err(RepoPathError::Backslash(path.to_owned()));
        }
        if path.starts_with('/') {
            return Err(RepoPathError::NotInside(path.to_owned()));
        }
        if path.ends_with('/') {
            return Err(RepoPathError::NotAFile(path.to_owned()));
        }
        for segment in path.split('/') {
            match segment {
                ".." => return Err(RepoPathError::NotInside(path.to_owned())),
                "" | "." => return Err(RepoPathError::NotNormalized(path.to_owned())),
                _ => {}
            }
        }
        // Backstop for platforms where a root or a drive prefix is not
        // spelled with a leading `/`. The textual checks above cover the
        // hosts peppy runs on; this covers the type's promise everywhere.
        if Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(RepoPathError::NotInside(path.to_owned()));
        }
        Ok(Self(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for RepoRelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoRelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}

pub use config::fingerprint::{ManifestFingerprint, ManifestFingerprintError};

config::hex_identity!(
    /// The commit a repository was read at.
    ///
    /// A branch name says which line of development to follow, not which
    /// bytes to read; two machines reading `main` a week apart hold different
    /// trees. A commit identifies one tree for as long as the repository
    /// exists, which is what lets one machine tell another where to look and
    /// expect the same answer.
    ///
    /// An abbreviated hash is rejected rather than expanded: expanding one
    /// needs the repository, and the whole point of the value is to be
    /// readable by a machine that does not have it yet.
    GitCommit,
    GitCommitError {
        Empty = "commit is empty",
        NotAFullHash = "commit is not 40 hexadecimal characters: {0}",
    },
    40
);

/// Declares a validated string newtype whose `Deserialize` runs `$validate`,
/// so an index cannot state an identity the repository caches could never
/// key on. The two identity halves differ only in which shared validator
/// they call, so the surrounding boilerplate is written once.
macro_rules! validated_identity {
    ($(#[$meta:meta])* $name:ident, $validate:path, $label:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(raw: &str) -> Result<Self, String> {
                $validate(raw, $label)?;
                Ok(Self(raw.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: de::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(de::Error::custom)
            }
        }
    };
}

validated_identity!(
    /// The name half of an item's identity, held to the same charset the
    /// rest of peppy resolves `name:tag` with.
    ItemName,
    config::repo_node_id::validate_repo_node_name,
    "name"
);

validated_identity!(
    /// The tag half of an item's identity. Launchers carry no tag and are
    /// indexed one level deep instead of borrowing an empty one.
    ItemTag,
    config::repo_node_id::validate_repo_node_tag,
    "tag"
);

config::compares_to_str!(RepoRelativePath);
config::compares_to_str!(ItemName);
config::compares_to_str!(ItemTag);

/// Where one published identity is declared.
///
/// An object rather than a bare path string so an entry reads without
/// knowing the format, and so a field can be added later as an additive
/// diff instead of a rewrite of every entry in every repository. Only facts
/// about the repository's publication of an identity belong here; facts
/// about the item itself live in the file `path` names and are read from
/// there, because a copy of them here would drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexedItem {
    pub path: RepoRelativePath,
}

/// A section keyed by name and then by tag. The nesting is the uniqueness
/// rule: a second claim on one identity has nowhere to go except a key that
/// is already taken.
pub type TaggedSection = UniqueMap<ItemName, UniqueMap<ItemTag, IndexedItem>>;

/// Reject any `peppy_schema` value other than `repository/v1`, so a node or
/// launcher document dropped at a repository root is not read as an index.
fn deserialize_repository_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::RepositoryV1)
}

/// What a repository publishes and where each item is declared.
///
/// Lives at the root of the tree peppy is configured to scan, so two entries
/// pointing at two subtrees of one repository are two independently indexed
/// repositories. A repository that publishes nothing has empty sections: an
/// index that is present and empty is valid, while a missing one is not,
/// because "publishes nothing" and "has no index" need different answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIndex {
    #[serde(deserialize_with = "deserialize_repository_v1_schema")]
    pub peppy_schema: PeppySchema,
    #[serde(default, skip_serializing_if = "UniqueMap::is_empty")]
    pub nodes: TaggedSection,
    /// One level deep, because a launcher is identified by its file stem and
    /// carries no tag.
    #[serde(default, skip_serializing_if = "UniqueMap::is_empty")]
    pub launchers: UniqueMap<ItemName, IndexedItem>,
    #[serde(default, skip_serializing_if = "UniqueMap::is_empty")]
    pub contracts: TaggedSection,
    #[serde(default, skip_serializing_if = "UniqueMap::is_empty")]
    pub pairings: TaggedSection,
    #[serde(default, skip_serializing_if = "UniqueMap::is_empty")]
    pub mcp_exposures: TaggedSection,
}

/// Flattens one two-level section into items. A free function rather than a
/// closure so the borrow of `section` is tied to the items it yields.
fn tagged_items(
    kind: RepoItemKind,
    section: &TaggedSection,
) -> impl Iterator<Item = DeclaredItem<'_>> {
    section.iter().flat_map(move |(name, tags)| {
        tags.iter().map(move |(tag, item)| DeclaredItem {
            kind,
            name,
            tag: Some(tag),
            path: &item.path,
        })
    })
}

/// One identity a repository publishes, flattened out of whichever section
/// declares it so callers iterate items rather than sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredItem<'a> {
    pub kind: RepoItemKind,
    pub name: &'a ItemName,
    /// `None` for a launcher, which is the only kind without a tag.
    pub tag: Option<&'a ItemTag>,
    pub path: &'a RepoRelativePath,
}

impl fmt::Display for DeclaredItem<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tag {
            Some(tag) => write!(f, "{} `{}:{}`", self.kind, self.name, tag),
            None => write!(f, "{} `{}`", self.kind, self.name),
        }
    }
}

impl Default for RepositoryIndex {
    fn default() -> Self {
        Self {
            peppy_schema: PeppySchema::RepositoryV1,
            nodes: UniqueMap::default(),
            launchers: UniqueMap::default(),
            contracts: UniqueMap::default(),
            pairings: UniqueMap::default(),
            mcp_exposures: UniqueMap::default(),
        }
    }
}

impl RepositoryIndex {
    /// Records one published identity, refusing a second claim on it.
    ///
    /// `tag` must be `Some` for every kind except a launcher. A mismatch is
    /// a caller bug rather than something a file can express, so it is
    /// reported the same way a duplicate is instead of being silently
    /// coerced into the wrong section.
    pub fn declare(
        &mut self,
        kind: RepoItemKind,
        name: ItemName,
        tag: Option<ItemTag>,
        path: RepoRelativePath,
    ) -> Result<(), String> {
        let item = IndexedItem { path };
        let section = match kind {
            RepoItemKind::Launcher => {
                if tag.is_some() {
                    return Err(format!("launcher `{name}` cannot carry a tag"));
                }
                return self.launchers.try_insert(name, item);
            }
            RepoItemKind::Node => &mut self.nodes,
            RepoItemKind::Contract => &mut self.contracts,
            RepoItemKind::Pairing => &mut self.pairings,
            RepoItemKind::McpExposure => &mut self.mcp_exposures,
        };
        let Some(tag) = tag else {
            return Err(format!("{kind} `{name}` has no tag"));
        };
        section.entry_or_default(name).try_insert(tag, item)
    }

    /// Every identity the repository publishes, in a deterministic order that
    /// never inherits the order a directory tree happened to be read in.
    pub fn declared_items(&self) -> impl Iterator<Item = DeclaredItem<'_>> {
        tagged_items(RepoItemKind::Node, &self.nodes)
            .chain(self.launchers.iter().map(|(name, item)| DeclaredItem {
                kind: RepoItemKind::Launcher,
                name,
                tag: None,
                path: &item.path,
            }))
            .chain(tagged_items(RepoItemKind::Contract, &self.contracts))
            .chain(tagged_items(RepoItemKind::Pairing, &self.pairings))
            .chain(tagged_items(RepoItemKind::McpExposure, &self.mcp_exposures))
    }

    pub fn declared_count(&self) -> usize {
        self.declared_items().count()
    }
}

/// Every path a value declares, so a duplicate key can be reported by the
/// files it points at rather than by the key alone.
pub trait DeclaredPaths {
    fn declared_paths(&self) -> Vec<&RepoRelativePath>;
}

impl DeclaredPaths for IndexedItem {
    fn declared_paths(&self) -> Vec<&RepoRelativePath> {
        vec![&self.path]
    }
}

impl<K, V: DeclaredPaths> DeclaredPaths for UniqueMap<K, V> {
    fn declared_paths(&self) -> Vec<&RepoRelativePath> {
        self.0
            .values()
            .flat_map(DeclaredPaths::declared_paths)
            .collect()
    }
}

/// A map that refuses a key stated twice.
///
/// A conventional map deserializer accepts a repeated key and silently keeps
/// the last one. For an index that is exactly the mistake worth failing on:
/// the file would then say two things about one identity while peppy quietly
/// believed one of them, which is harder to find than a loud refusal because
/// nothing reports and nothing diverges.
///
/// The `BTreeMap` inside also gives generation its deterministic order, so a
/// generated index is byte-identical for a given tree on every machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UniqueMap<K, V>(BTreeMap<K, V>);

// Written out rather than derived: a derived `Default` would demand
// `K: Default, V: Default`, which an empty map does not need.
impl<K, V> Default for UniqueMap<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Ord + fmt::Display, V: DeclaredPaths> UniqueMap<K, V> {
    /// Inserts `key`, refusing a second claim on it. Generation and reading
    /// share this so both report a duplicate the same way.
    pub fn try_insert(&mut self, key: K, value: V) -> Result<(), String> {
        match self.0.entry(key) {
            Entry::Occupied(existing) => Err(duplicate_key_message(
                existing.key(),
                existing.get(),
                &value,
            )),
            Entry::Vacant(slot) => {
                slot.insert(value);
                Ok(())
            }
        }
    }
}

impl<K: Ord, V: Default> UniqueMap<K, V> {
    /// The nested section a name owns, created empty on first use. Only the
    /// two-level sections need it, where a name is a container rather than a
    /// claim in its own right.
    fn entry_or_default(&mut self, key: K) -> &mut V {
        self.0.entry(key).or_default()
    }
}

impl<K, V> UniqueMap<K, V> {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.0.iter()
    }
}

fn duplicate_key_message<K: fmt::Display, V: DeclaredPaths>(
    key: &K,
    first: &V,
    second: &V,
) -> String {
    format!(
        "duplicate key `{key}`: already declared at {}, declared again at {}",
        describe_paths(&first.declared_paths()),
        describe_paths(&second.declared_paths())
    )
}

fn describe_paths(paths: &[&RepoRelativePath]) -> String {
    if paths.is_empty() {
        return "no path".to_owned();
    }
    paths
        .iter()
        .map(|path| path.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
where
    K: Deserialize<'de> + Ord + fmt::Display,
    V: Deserialize<'de> + DeclaredPaths,
{
    type Value = UniqueMap<K, V>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a map with no repeated key")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut map = UniqueMap::default();
        while let Some(key) = access.next_key::<K>()? {
            let value = access.next_value::<V>()?;
            map.try_insert(key, value).map_err(de::Error::custom)?;
        }
        Ok(map)
    }
}

impl<'de, K, V> Deserialize<'de> for UniqueMap<K, V>
where
    K: Deserialize<'de> + Ord + fmt::Display,
    V: Deserialize<'de> + DeclaredPaths,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every entry the deserializer hands it, repeats included.
    /// Exists only to pin the third-party behavior `UniqueMap` rests on.
    #[derive(Debug, Default)]
    struct CountingMap(Vec<(String, u32)>);

    impl<'de> Deserialize<'de> for CountingMap {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            struct CountingVisitor;

            impl<'de> Visitor<'de> for CountingVisitor {
                type Value = CountingMap;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("any map")
                }

                fn visit_map<A: MapAccess<'de>>(
                    self,
                    mut access: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut seen = Vec::new();
                    while let Some(entry) = access.next_entry::<String, u32>()? {
                        seen.push(entry);
                    }
                    Ok(CountingMap(seen))
                }
            }

            deserializer.deserialize_map(CountingVisitor)
        }
    }

    /// Pins the one third-party behavior `UniqueMap` rests on: `serde_json5`
    /// must hand both halves of a repeated key to the visitor rather than
    /// collapsing them before serde sees them. If this ever fails, duplicate
    /// rejection is silently dead everywhere and a syntactic pre-pass over
    /// the raw text has to take over.
    #[test]
    fn serde_json5_streams_duplicate_object_keys_to_the_visitor() {
        let seen: CountingMap = serde_json5::from_str("{ a: 1, a: 2 }").expect("parses");
        assert_eq!(
            seen.0,
            vec![("a".to_owned(), 1), ("a".to_owned(), 2)],
            "serde_json5 collapsed a duplicate key before the visitor saw it"
        );
    }

    fn item(path: &str) -> IndexedItem {
        IndexedItem {
            path: RepoRelativePath::parse(path).expect("valid path"),
        }
    }

    #[test]
    fn rejects_a_duplicate_key_naming_both_paths() {
        let err = serde_json5::from_str::<UniqueMap<ItemTag, IndexedItem>>(
            r#"{ "v1": { path: "a/peppy.json5" }, "v1": { path: "b/peppy.json5" } }"#,
        )
        .expect_err("a repeated tag must be refused");
        let message = err.to_string();
        assert!(message.contains("duplicate key `v1`"), "{message}");
        assert!(message.contains("a/peppy.json5"), "{message}");
        assert!(message.contains("b/peppy.json5"), "{message}");
    }

    #[test]
    fn rejects_a_duplicate_name_naming_every_path_below_it() {
        let err = serde_json5::from_str::<UniqueMap<ItemName, UniqueMap<ItemTag, IndexedItem>>>(
            r#"{
                 "uvc": { "v1": { path: "a/peppy.json5" } },
                 "uvc": { "v2": { path: "b/peppy.json5" } },
               }"#,
        )
        .expect_err("a repeated name must be refused");
        let message = err.to_string();
        assert!(message.contains("duplicate key `uvc`"), "{message}");
        assert!(message.contains("a/peppy.json5"), "{message}");
        assert!(message.contains("b/peppy.json5"), "{message}");
    }

    #[test]
    fn accepts_distinct_keys() {
        let map: UniqueMap<ItemTag, IndexedItem> = serde_json5::from_str(
            r#"{ "v1": { path: "a/peppy.json5" }, "v2": { path: "b/peppy.json5" } }"#,
        )
        .expect("distinct tags parse");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn try_insert_refuses_a_second_claim() {
        let mut map: UniqueMap<ItemTag, IndexedItem> = UniqueMap::default();
        let tag = ItemTag::parse("v1").expect("valid tag");
        map.try_insert(tag.clone(), item("a/peppy.json5"))
            .expect("first claim");
        let err = map
            .try_insert(tag, item("b/peppy.json5"))
            .expect_err("second claim");
        assert!(
            err.contains("a/peppy.json5") && err.contains("b/peppy.json5"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_unknown_field_on_an_entry() {
        let err =
            serde_json5::from_str::<IndexedItem>(r#"{ path: "a/peppy.json5", description: "no" }"#)
                .expect_err("an unknown field must be refused");
        assert!(err.to_string().contains("description"), "{err}");
    }

    #[test]
    fn parses_a_repository_relative_path() {
        let path = RepoRelativePath::parse("uvc_camera/rust/peppy.json5").expect("valid");
        assert_eq!(path.as_str(), "uvc_camera/rust/peppy.json5");
    }

    #[test]
    fn trims_surrounding_whitespace_so_parsing_is_canonical() {
        let path = RepoRelativePath::parse("  a/peppy.json5  ").expect("valid");
        assert_eq!(path.as_str(), "a/peppy.json5");
    }

    #[test]
    fn refuses_paths_that_do_not_locate_a_file_inside_the_repository() {
        let cases = [
            ("", RepoPathError::Empty),
            ("   ", RepoPathError::Empty),
            (
                "a\\b.json5",
                RepoPathError::Backslash("a\\b.json5".to_owned()),
            ),
            (
                "/etc/passwd",
                RepoPathError::NotInside("/etc/passwd".to_owned()),
            ),
            (
                "../../etc/passwd",
                RepoPathError::NotInside("../../etc/passwd".to_owned()),
            ),
            (
                "a/../b.json5",
                RepoPathError::NotInside("a/../b.json5".to_owned()),
            ),
            (
                "./a.json5",
                RepoPathError::NotNormalized("./a.json5".to_owned()),
            ),
            (
                "a/./b.json5",
                RepoPathError::NotNormalized("a/./b.json5".to_owned()),
            ),
            (
                "a//b.json5",
                RepoPathError::NotNormalized("a//b.json5".to_owned()),
            ),
            ("a/", RepoPathError::NotAFile("a/".to_owned())),
        ];
        for (raw, expected) in cases {
            let err =
                RepoRelativePath::parse(raw).expect_err(&format!("expected `{raw}` to be refused"));
            assert_eq!(err, expected, "wrong error for `{raw}`");
        }
    }

    #[test]
    fn refuses_an_identity_the_caches_could_never_key_on() {
        assert!(ItemName::parse("foo/bar").is_err());
        assert!(ItemName::parse("").is_err());
        assert!(ItemTag::parse("").is_err());
        assert!(ItemTag::parse("1v").is_err());
        assert!(ItemTag::parse("v0.1.0").is_err());
        assert!(ItemName::parse("uvc_camera").is_ok());
        assert!(ItemTag::parse("v1").is_ok());
    }
}
