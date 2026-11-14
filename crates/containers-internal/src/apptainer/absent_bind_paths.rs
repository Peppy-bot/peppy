//! The `bind path` entries of `apptainer.conf` whose source the host does not
//! have.
//!
//! The `apptainer.conf` of an apptainer installation names files and
//! directories of the host that apptainer binds into each container, one in
//! each `bind path` entry. The bundled configuration has two entries:
//! `/etc/localtime` and `/etc/hosts`. Apptainer stops a container when the
//! source of an entry does not exist on the host ("mount source
//! /etc/localtime doesn't exist"), and a minimal host can be without one: the
//! `ubuntu` Docker image has no `/etc/localtime` until it gets the `tzdata`
//! package.
//!
//! So each `run` and each `exec` of the native backend passes
//! `--no-mount <destination>` for each entry whose source is absent, and the
//! container has at that path what its image has.

use std::fs;
use std::path::Path;

/// The flag of `apptainer run` and `apptainer exec` that skips one mount.
/// With an absolute path as its value, the mount is the `bind path` entry of
/// that destination.
pub(crate) const NO_MOUNT_FLAG: &str = "--no-mount";

/// Path of the configuration inside an apptainer install tree.
const APPTAINER_CONF_PATH: &str = "etc/apptainer/apptainer.conf";

/// The key of an entry of `apptainer.conf` that binds a path of the host.
const BIND_PATH_KEY: &str = "bind path";

/// One `bind path` entry: `<source>` or `<source>:<destination>`.
#[derive(Debug, PartialEq, Eq)]
struct BindPathEntry<'a> {
    /// The path on the host.
    source: &'a str,
    /// The path in the container, which is the source when the entry names
    /// none. `--no-mount` names an entry by it.
    destination: &'a str,
}

impl<'a> BindPathEntry<'a> {
    /// Reads the entry of one line of `apptainer.conf`, or `None` for a line
    /// that is not a `bind path` entry: another key, a comment or a blank
    /// line.
    fn parse(line: &'a str) -> Option<Self> {
        let (key, value) = line.split_once('=')?;
        if key.trim() != BIND_PATH_KEY {
            return None;
        }
        let mut fields = value.trim().split(':');
        let source = fields.next().filter(|source| !source.is_empty())?;
        let destination = fields.next().unwrap_or(source);
        Some(Self {
            source,
            destination,
        })
    }
}

/// The destinations of the `bind path` entries of the installation at
/// `apptainer_dir` whose source does not exist on this host.
///
/// A configuration that cannot be read gives no destination: apptainer
/// reports a broken configuration itself.
pub(crate) fn of_installation(apptainer_dir: &Path) -> Vec<String> {
    let Ok(config) = fs::read_to_string(apptainer_dir.join(APPTAINER_CONF_PATH)) else {
        return Vec::new();
    };
    // Apptainer stops on a source that does not exist. Any other failure to
    // read a source is left to apptainer, which names it.
    destinations_without_source(&config, |source| {
        matches!(Path::new(source).try_exists(), Ok(false))
    })
}

/// The destinations of the `bind path` entries of `config`, the text of an
/// `apptainer.conf`, whose source `is_absent` reports absent.
fn destinations_without_source(config: &str, is_absent: impl Fn(&str) -> bool) -> Vec<String> {
    config
        .lines()
        .filter_map(BindPathEntry::parse)
        .filter(|entry| is_absent(entry.source))
        .map(|entry| entry.destination.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptainer::scratch_dir;

    /// The `bind path` part of the bundled `apptainer.conf`.
    const BUNDLED_CONFIG: &str = "\
# BIND PATH: [STRING]
# DEFAULT: Undefined
#bind path = /etc/apptainer/default-nsswitch.conf:/etc/nsswitch.conf
#bind path = /opt
bind path = /etc/localtime
bind path = /etc/hosts

mount hostfs = no
";

    #[test]
    fn an_entry_with_one_path_binds_it_at_the_same_path() {
        assert_eq!(
            BindPathEntry::parse("bind path = /etc/localtime"),
            Some(BindPathEntry {
                source: "/etc/localtime",
                destination: "/etc/localtime",
            })
        );
    }

    #[test]
    fn an_entry_with_two_paths_names_the_source_then_the_destination() {
        assert_eq!(
            BindPathEntry::parse("bind path = /etc/default-nsswitch.conf:/etc/nsswitch.conf"),
            Some(BindPathEntry {
                source: "/etc/default-nsswitch.conf",
                destination: "/etc/nsswitch.conf",
            })
        );
    }

    #[test]
    fn a_line_that_is_not_a_bind_path_entry_gives_no_entry() {
        assert_eq!(BindPathEntry::parse("#bind path = /opt"), None);
        assert_eq!(BindPathEntry::parse("mount hostfs = no"), None);
        assert_eq!(BindPathEntry::parse("# BIND PATH: [STRING]"), None);
        assert_eq!(BindPathEntry::parse("bind path ="), None);
        assert_eq!(BindPathEntry::parse(""), None);
    }

    #[test]
    fn only_the_entries_with_an_absent_source_are_skipped() {
        let destinations =
            destinations_without_source(BUNDLED_CONFIG, |source| source == "/etc/localtime");

        assert_eq!(destinations, ["/etc/localtime"]);
    }

    #[test]
    fn no_entry_is_skipped_on_a_host_that_has_every_source() {
        let destinations = destinations_without_source(BUNDLED_CONFIG, |_| false);

        assert!(destinations.is_empty());
    }

    #[test]
    fn an_entry_is_skipped_by_its_destination() {
        let config = "bind path = /host/nsswitch.conf:/etc/nsswitch.conf\n";

        let destinations =
            destinations_without_source(config, |source| source == "/host/nsswitch.conf");

        assert_eq!(destinations, ["/etc/nsswitch.conf"]);
    }

    #[test]
    fn the_configuration_of_an_installation_is_read_against_the_host() {
        let install = scratch_dir();
        let present = install.path().join("present-on-the-host");
        let absent = install.path().join("absent-on-the-host");
        fs::write(&present, "").expect("write the present source");
        let config_dir = install.path().join("etc/apptainer");
        fs::create_dir_all(&config_dir).expect("create the configuration directory");
        fs::write(
            config_dir.join("apptainer.conf"),
            format!(
                "bind path = {}\nbind path = {}:/etc/localtime\n",
                present.display(),
                absent.display()
            ),
        )
        .expect("write the configuration");

        let destinations = of_installation(install.path());

        assert_eq!(destinations, ["/etc/localtime"]);
    }

    #[test]
    fn a_dangling_link_is_an_absent_source() {
        let install = scratch_dir();
        let link = install.path().join("localtime");
        std::os::unix::fs::symlink(install.path().join("no-such-zoneinfo"), &link)
            .expect("create the dangling link");
        let config_dir = install.path().join("etc/apptainer");
        fs::create_dir_all(&config_dir).expect("create the configuration directory");
        fs::write(
            config_dir.join("apptainer.conf"),
            format!("bind path = {}:/etc/localtime\n", link.display()),
        )
        .expect("write the configuration");

        let destinations = of_installation(install.path());

        assert_eq!(destinations, ["/etc/localtime"]);
    }

    #[test]
    fn an_installation_without_a_configuration_skips_no_entry() {
        let install = scratch_dir();

        assert!(of_installation(install.path()).is_empty());
    }
}
