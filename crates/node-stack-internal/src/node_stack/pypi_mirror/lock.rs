//! The PyPI files a `uv.lock` pins, and the rewrite of their URLs to a
//! mirror. No network.

use std::collections::HashSet;

use daemon_config::peppy_config::PackagesBaseUrl;
use toml_edit::{DocumentMut, Item, TableLike, Value};

/// The host PyPI serves the files of its packages from.
pub(super) const PYPI_FILES_HOST: &str = "files.pythonhosted.org";

/// Where PyPI keeps the files of its packages, on [`PYPI_FILES_HOST`].
pub(super) const PYPI_PACKAGES_BASE: &str = "https://files.pythonhosted.org/packages/";

/// A file that a lock pins by sha256 for a package from a registry, at a URL
/// below [`PYPI_PACKAGES_BASE`]. Only such a file can come from a mirror: uv
/// checks it against the sha256 of the lock, so a mirror can make a build
/// slow or fail but cannot change what it installs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct PypiFile {
    /// The path of the file below [`PYPI_PACKAGES_BASE`], which is also its
    /// path below the packages base URL of a mirror.
    path: String,
    /// The size in bytes the lock records for the file, when it records one.
    size: Option<u64>,
}

impl PypiFile {
    pub(super) fn size(&self) -> Option<u64> {
        self.size
    }

    /// The URL of the file on `mirror`.
    pub(super) fn url_on(&self, mirror: &PackagesBaseUrl) -> String {
        format!("{}{}", mirror.as_str(), self.path)
    }
}

/// A `uv.lock`, parsed, with the PyPI files it pins.
pub(super) struct UvLock {
    document: DocumentMut,
    pypi_files: Vec<PypiFile>,
}

/// The text of a lock whose PyPI files on the mirror point at the mirror.
pub(super) struct RedirectedLock {
    /// The text of the lock: the parsed text, except for the URLs of the
    /// files that moved to the mirror. `toml_edit` writes every other item
    /// as it parsed it, but ends each line with LF and drops a byte order
    /// mark; uv reads the lock the same either way.
    pub text: String,
    /// How many file entries of the lock moved to the mirror.
    pub redirected_files: usize,
}

impl UvLock {
    pub(super) fn parse(text: &str) -> Result<Self, toml_edit::TomlError> {
        let mut document: DocumentMut = text.parse()?;
        let mut pypi_files = Vec::new();
        for_each_pypi_file(&mut document, |file, _url| pypi_files.push(file));
        Ok(Self {
            document,
            pypi_files,
        })
    }

    /// The PyPI files of the lock, one per file entry, in lock order.
    pub(super) fn pypi_files(&self) -> &[PypiFile] {
        &self.pypi_files
    }

    /// Points the URL of each PyPI file of the lock that is in `on_mirror`
    /// at `mirror`. Every other file keeps its URL.
    pub(super) fn redirect(
        mut self,
        mirror: &PackagesBaseUrl,
        on_mirror: &HashSet<PypiFile>,
    ) -> RedirectedLock {
        let mut redirected_files = 0;
        for_each_pypi_file(&mut self.document, |file, url| {
            if on_mirror.contains(&file) {
                replace_string_keeping_decor(url, file.url_on(mirror));
                redirected_files += 1;
            }
        });
        RedirectedLock {
            text: self.document.to_string(),
            redirected_files,
        }
    }
}

/// Calls `visit` with each [`PypiFile`] of the lock and the `url` value of
/// its entry: each `sdist` and `wheels` entry of a package whose `source` is
/// a registry, with a `sha256:` hash and a URL below [`PYPI_PACKAGES_BASE`].
/// A package from any other source (a direct URL, git, a path, an editable
/// or virtual project) is skipped even when its files are on PyPI: uv keys
/// such a package by its URL, so a lock with another URL no longer matches
/// the project.
fn for_each_pypi_file(document: &mut DocumentMut, mut visit: impl FnMut(PypiFile, &mut Value)) {
    let Some(packages) = document
        .get_mut("package")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return;
    };
    for package in packages.iter_mut() {
        let from_registry = package
            .get("source")
            .and_then(Item::as_table_like)
            .is_some_and(|source| source.contains_key("registry"));
        if !from_registry {
            continue;
        }
        if let Some(sdist) = package.get_mut("sdist").and_then(Item::as_table_like_mut) {
            visit_file_entry(sdist, &mut visit);
        }
        if let Some(wheels) = package.get_mut("wheels") {
            for wheel in file_entries(wheels) {
                visit_file_entry(wheel, &mut visit);
            }
        }
    }
}

/// The entries of a `wheels` list: an array of inline tables, as uv writes
/// it, or an array of tables.
fn file_entries(wheels: &mut Item) -> Vec<&mut dyn TableLike> {
    match wheels {
        Item::Value(Value::Array(array)) => array
            .iter_mut()
            .filter_map(Value::as_inline_table_mut)
            .map(|entry| entry as &mut dyn TableLike)
            .collect(),
        Item::ArrayOfTables(tables) => tables
            .iter_mut()
            .map(|entry| entry as &mut dyn TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

fn visit_file_entry(entry: &mut dyn TableLike, visit: &mut impl FnMut(PypiFile, &mut Value)) {
    let Some(file) = pypi_file_of(entry) else {
        return;
    };
    let Some(Item::Value(url)) = entry.get_mut("url") else {
        return;
    };
    visit(file, url);
}

/// The [`PypiFile`] of a file entry, or `None` when the entry has no URL
/// below [`PYPI_PACKAGES_BASE`], no `sha256:` hash, or a `size` that is not
/// a number of bytes.
fn pypi_file_of(entry: &dyn TableLike) -> Option<PypiFile> {
    let path = entry
        .get("url")
        .and_then(Item::as_str)?
        .strip_prefix(PYPI_PACKAGES_BASE)
        .filter(|path| !path.is_empty())?;
    let pinned_by_sha256 = entry
        .get("hash")
        .and_then(Item::as_str)
        .is_some_and(|hash| hash.starts_with("sha256:"));
    if !pinned_by_sha256 {
        return None;
    }
    let size = match entry.get("size") {
        None => None,
        Some(size) => Some(size.as_integer().and_then(|n| u64::try_from(n).ok())?),
    };
    Some(PypiFile {
        path: path.to_string(),
        size,
    })
}

/// Replaces the string `value` with `new`, and keeps the whitespace and
/// comments around it.
fn replace_string_keeping_decor(value: &mut Value, new: String) {
    let decor = value.decor().clone();
    *value = Value::from(new);
    *value.decor_mut() = decor;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock as uv writes it, with one package of each source kind: the
    /// virtual project, a registry package on PyPI (`idna`), a direct URL
    /// whose file is on PyPI (`six`), a path, an editable project, git, and
    /// a registry that is not PyPI (`torch`).
    const LOCK: &str = r#"version = 1
revision = 3
requires-python = ">=3.12"

[[package]]
name = "fixture-node"
version = "0.1.0"
source = { virtual = "." }
dependencies = [
    { name = "idna" },
    { name = "localpkg" },
    { name = "six" },
]

[package.metadata]
requires-dist = [
    { name = "idna", specifier = "==3.10" },
    { name = "localpkg", directory = "localpkg" },
    { name = "six", url = "https://files.pythonhosted.org/packages/b7/ce/149a00dd41f10bc29e5921b496af8b574d8413afcd5e30dfa0ed46c2cc5e/six-1.17.0-py2.py3-none-any.whl" },
]

[[package]]
name = "idna"
version = "3.10"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://files.pythonhosted.org/packages/f1/70/7703c29685631f5a7590aa73f1f1d3fa9a380e654b86af429e0934a32f7d/idna-3.10.tar.gz", hash = "sha256:12f65c9b470abda6dc35cf8e63cc574b1c52b11df2c86030af0ac09b01b13ea9", size = 190490, upload-time = "2024-09-15T18:07:39.745Z" }
wheels = [
    { url = "https://files.pythonhosted.org/packages/76/c6/c88e154df9c4e1a2a66ccf0005a88dfb2650c1dffb6f5ce603dfbd452ce3/idna-3.10-py3-none-any.whl", hash = "sha256:946d195a0d259cbba61165e88e65941f16e9b36ea6ddb97f00452bae8b1287d3", size = 70442, upload-time = "2024-09-15T18:07:37.964Z" },
]

[[package]]
name = "localpkg"
version = "0.1.0"
source = { directory = "localpkg" }

[[package]]
name = "six"
version = "1.17.0"
source = { url = "https://files.pythonhosted.org/packages/b7/ce/149a00dd41f10bc29e5921b496af8b574d8413afcd5e30dfa0ed46c2cc5e/six-1.17.0-py2.py3-none-any.whl" }
wheels = [
    { url = "https://files.pythonhosted.org/packages/b7/ce/149a00dd41f10bc29e5921b496af8b574d8413afcd5e30dfa0ed46c2cc5e/six-1.17.0-py2.py3-none-any.whl", hash = "sha256:4721f391ed90541fddacab5acf947aa0d3dc7d27b2e1e8eda2be8970586c3274" },
]

[[package]]
name = "editable-pkg"
version = "0.2.0"
source = { editable = "editable_pkg" }

[[package]]
name = "lerobot"
version = "0.3.4"
source = { git = "https://github.com/huggingface/lerobot?rev=5cd2f8a#5cd2f8a1b2c3d4e5f60718293a4b5c6d7e8f9012" }

[[package]]
name = "torch"
version = "2.8.0+cu128"
source = { registry = "https://download.pytorch.org/whl/cu128" }
wheels = [
    { url = "https://download.pytorch.org/whl/cu128/torch-2.8.0%2Bcu128-cp312-cp312-manylinux_2_28_aarch64.whl", hash = "sha256:0d6b4a8e3c5f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b", size = 887254328 },
]
"#;

    const IDNA_SDIST: &str =
        "f1/70/7703c29685631f5a7590aa73f1f1d3fa9a380e654b86af429e0934a32f7d/idna-3.10.tar.gz";
    const IDNA_WHEEL: &str = "76/c6/c88e154df9c4e1a2a66ccf0005a88dfb2650c1dffb6f5ce603dfbd452ce3/idna-3.10-py3-none-any.whl";

    fn mirror() -> PackagesBaseUrl {
        PackagesBaseUrl::parse("https://pypi.tuna.tsinghua.edu.cn/packages/").unwrap()
    }

    fn file(path: &str, size: Option<u64>) -> PypiFile {
        PypiFile {
            path: path.to_string(),
            size,
        }
    }

    fn on_pypi(path: &str) -> String {
        format!("{PYPI_PACKAGES_BASE}{path}")
    }

    fn on_mirror(path: &str) -> String {
        format!("https://pypi.tuna.tsinghua.edu.cn/packages/{path}")
    }

    #[test]
    fn only_registry_files_on_pypi_pinned_by_sha256_are_collected() {
        let lock = UvLock::parse(LOCK).expect("the fixture parses");
        assert_eq!(
            lock.pypi_files(),
            [
                file(IDNA_SDIST, Some(190490)),
                file(IDNA_WHEEL, Some(70442))
            ]
        );
    }

    #[test]
    fn files_on_the_mirror_get_the_mirror_base_and_all_other_text_stays() {
        let lock = UvLock::parse(LOCK).unwrap();
        let on_mirror_set = lock.pypi_files().iter().cloned().collect();

        let redirected = lock.redirect(&mirror(), &on_mirror_set);

        let expected = LOCK
            .replacen(&on_pypi(IDNA_SDIST), &on_mirror(IDNA_SDIST), 1)
            .replacen(&on_pypi(IDNA_WHEEL), &on_mirror(IDNA_WHEEL), 1);
        assert_ne!(expected, LOCK, "the fixture has both idna files on PyPI");
        assert_eq!(redirected.text, expected);
        assert_eq!(redirected.redirected_files, 2);
    }

    #[test]
    fn only_the_files_on_the_mirror_change() {
        let lock = UvLock::parse(LOCK).unwrap();
        let wheel_only = HashSet::from([file(IDNA_WHEEL, Some(70442))]);

        let redirected = lock.redirect(&mirror(), &wheel_only);

        assert_eq!(
            redirected.text,
            LOCK.replacen(&on_pypi(IDNA_WHEEL), &on_mirror(IDNA_WHEEL), 1)
        );
        assert_eq!(redirected.redirected_files, 1);
    }

    #[test]
    fn a_file_the_mirror_has_with_another_size_does_not_change() {
        let lock = UvLock::parse(LOCK).unwrap();
        let other_size = HashSet::from([file(IDNA_WHEEL, Some(1))]);

        let redirected = lock.redirect(&mirror(), &other_size);

        assert_eq!(redirected.text, LOCK);
        assert_eq!(redirected.redirected_files, 0);
    }

    #[test]
    fn no_file_on_the_mirror_keeps_the_lock_byte_for_byte() {
        let redirected = UvLock::parse(LOCK)
            .unwrap()
            .redirect(&mirror(), &HashSet::new());
        assert_eq!(redirected.text, LOCK);
        assert_eq!(redirected.redirected_files, 0);
    }

    /// Every source but a registry stays as it is, even when its files are
    /// on PyPI, and so does a registry file on another host.
    #[test]
    fn other_sources_and_other_hosts_are_never_collected() {
        for (name, source, url) in [
            (
                "six",
                r#"{ url = "https://files.pythonhosted.org/packages/b7/ce/x/six-1.17.0-py2.py3-none-any.whl" }"#,
                "https://files.pythonhosted.org/packages/b7/ce/x/six-1.17.0-py2.py3-none-any.whl",
            ),
            (
                "lerobot",
                r#"{ git = "https://github.com/huggingface/lerobot?rev=5cd2f8a#5cd2f8a" }"#,
                "https://files.pythonhosted.org/packages/aa/bb/x/lerobot-0.3.4.tar.gz",
            ),
            (
                "localpkg",
                r#"{ path = "wheels/localpkg-0.1.0-py3-none-any.whl" }"#,
                "https://files.pythonhosted.org/packages/aa/bb/x/localpkg-0.1.0-py3-none-any.whl",
            ),
            (
                "localpkg",
                r#"{ directory = "localpkg" }"#,
                "https://files.pythonhosted.org/packages/aa/bb/x/localpkg-0.1.0.tar.gz",
            ),
            (
                "editable-pkg",
                r#"{ editable = "editable_pkg" }"#,
                "https://files.pythonhosted.org/packages/aa/bb/x/editable_pkg-0.2.0.tar.gz",
            ),
            (
                "fixture-node",
                r#"{ virtual = "." }"#,
                "https://files.pythonhosted.org/packages/aa/bb/x/fixture_node-0.1.0.tar.gz",
            ),
            (
                "torch",
                r#"{ registry = "https://download.pytorch.org/whl/cu128" }"#,
                "https://download.pytorch.org/whl/cu128/torch-2.8.0%2Bcu128-cp312-cp312-manylinux_2_28_aarch64.whl",
            ),
        ] {
            let lock = format!(
                "version = 1\n\n[[package]]\nname = \"{name}\"\nversion = \"1\"\nsource = {source}\n\
                 sdist = {{ url = \"{url}\", hash = \"sha256:00\", size = 1 }}\n\
                 wheels = [\n    {{ url = \"{url}\", hash = \"sha256:00\", size = 1 }},\n]\n"
            );
            let parsed = UvLock::parse(&lock).expect(&lock);
            assert!(parsed.pypi_files().is_empty(), "{lock}");
            let everything = HashSet::from([file("aa/bb/x/any", Some(1))]);
            assert_eq!(parsed.redirect(&mirror(), &everything).text, lock);
        }
    }

    /// The host of a file decides, not the registry: an index such as
    /// `download.pytorch.org/whl/cpu` lists some packages with files on PyPI.
    #[test]
    fn a_file_on_pypi_of_another_registry_is_collected() {
        let lock = "[[package]]\nname = \"mpmath\"\n\
                    source = { registry = \"https://download.pytorch.org/whl/cpu\" }\n\
                    sdist = { url = \"https://files.pythonhosted.org/packages/c5/b0/x/mpmath-1.4.1.tar.gz\", hash = \"sha256:00\" }\n";
        let parsed = UvLock::parse(lock).unwrap();
        assert_eq!(
            parsed.pypi_files(),
            [file("c5/b0/x/mpmath-1.4.1.tar.gz", None)]
        );
    }

    #[test]
    fn a_file_without_a_sha256_or_with_a_bad_size_is_not_collected() {
        for entry in [
            r#"{ url = "https://files.pythonhosted.org/packages/aa/bb/x/a.whl", size = 1 }"#,
            r#"{ url = "https://files.pythonhosted.org/packages/aa/bb/x/a.whl", hash = "md5:00", size = 1 }"#,
            r#"{ url = "https://files.pythonhosted.org/packages/aa/bb/x/a.whl", hash = "sha256:00", size = -1 }"#,
            r#"{ url = "https://files.pythonhosted.org/packages/aa/bb/x/a.whl", hash = "sha256:00", size = "1" }"#,
            r#"{ url = "https://files.pythonhosted.org/packages/", hash = "sha256:00", size = 1 }"#,
            r#"{ url = 7, hash = "sha256:00", size = 1 }"#,
        ] {
            let lock = format!(
                "[[package]]\nname = \"a\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n\
                 wheels = [\n    {entry},\n]\n"
            );
            assert!(
                UvLock::parse(&lock).expect(&lock).pypi_files().is_empty(),
                "{entry}"
            );
        }
    }

    #[test]
    fn a_file_with_no_size_is_collected_without_one() {
        let lock = "[[package]]\nname = \"a\"\nsource = { registry = \"https://pypi.org/simple\" }\n\
                    sdist = { url = \"https://files.pythonhosted.org/packages/aa/bb/x/a-1.tar.gz\", hash = \"sha256:00\" }\n";
        let parsed = UvLock::parse(lock).unwrap();
        assert_eq!(parsed.pypi_files(), [file("aa/bb/x/a-1.tar.gz", None)]);

        let on_mirror_set = parsed.pypi_files().iter().cloned().collect();
        assert_eq!(
            parsed.redirect(&mirror(), &on_mirror_set).text,
            lock.replacen(PYPI_PACKAGES_BASE, mirror().as_str(), 1)
        );
    }

    /// uv writes each list of wheels as an array of inline tables; the same
    /// files spelled as standard tables are found too.
    #[test]
    fn files_in_standard_tables_are_redirected_too() {
        let lock = "[[package]]\nname = \"a\"\n\n[package.source]\nregistry = \"https://pypi.org/simple\"\n\n\
                    [package.sdist]\nurl = \"https://files.pythonhosted.org/packages/aa/bb/x/a-1.tar.gz\" # the sdist\n\
                    hash = \"sha256:00\"\nsize = 3\n\n\
                    [[package.wheels]]\nurl = \"https://files.pythonhosted.org/packages/cc/dd/y/a-1-py3-none-any.whl\"\n\
                    hash = \"sha256:11\"\nsize = 4\n";
        let parsed = UvLock::parse(lock).unwrap();
        assert_eq!(
            parsed.pypi_files(),
            [
                file("aa/bb/x/a-1.tar.gz", Some(3)),
                file("cc/dd/y/a-1-py3-none-any.whl", Some(4))
            ]
        );

        let on_mirror_set = parsed.pypi_files().iter().cloned().collect();
        let redirected = parsed.redirect(&mirror(), &on_mirror_set);
        assert_eq!(
            redirected.text,
            lock.replace(PYPI_PACKAGES_BASE, mirror().as_str())
        );
        assert_eq!(redirected.redirected_files, 2);
    }

    #[test]
    fn a_lock_with_crlf_line_endings_comes_back_with_lf() {
        let lock = UvLock::parse(&LOCK.replace('\n', "\r\n")).unwrap();
        let on_mirror_set = lock.pypi_files().iter().cloned().collect();

        let redirected = lock.redirect(&mirror(), &on_mirror_set);

        assert_eq!(
            redirected.text,
            LOCK.replacen(&on_pypi(IDNA_SDIST), &on_mirror(IDNA_SDIST), 1)
                .replacen(&on_pypi(IDNA_WHEEL), &on_mirror(IDNA_WHEEL), 1)
        );
    }

    #[test]
    fn a_lock_with_no_package_has_no_file() {
        let parsed = UvLock::parse("version = 1\nrequires-python = \">=3.12\"\n").unwrap();
        assert!(parsed.pypi_files().is_empty());
    }

    #[test]
    fn a_lock_that_does_not_parse_is_an_error() {
        assert!(UvLock::parse("[[package]\nname = \"a\"\n").is_err());
        assert!(UvLock::parse("version = \n").is_err());
    }

    #[test]
    fn the_pypi_packages_base_is_on_the_pypi_files_host() {
        assert_eq!(
            PYPI_PACKAGES_BASE,
            format!("https://{PYPI_FILES_HOST}/packages/")
        );
    }

    #[test]
    fn the_mirror_url_of_a_file_is_its_path_below_the_mirror_base() {
        assert_eq!(
            file(IDNA_WHEEL, None).url_on(&mirror()),
            on_mirror(IDNA_WHEEL)
        );
    }
}
