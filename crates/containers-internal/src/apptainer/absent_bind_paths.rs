//! The paths of the host that apptainer binds into each container, and that
//! this host does not have.
//!
//! Apptainer binds files and directories of the host into each container:
//!
//! - For a container that is not contained, the `bind path` entries of the
//!   `apptainer.conf` of its installation. The bundled configuration has two:
//!   `/etc/localtime` and `/etc/hosts`.
//! - For a contained container (`--contain`, `--containall`), `/etc/hosts`
//!   and `/etc/localtime`, whatever the configuration holds.
//!
//! Apptainer stops a container when the source of such a bind does not exist
//! on the host ("mount source /etc/localtime doesn't exist"), and a minimal
//! host can be without one: the `ubuntu` Docker image has no `/etc/localtime`
//! until it gets the `tzdata` package.
//!
//! So each `run` and each `exec` of the native backend passes
//! `--no-mount <destination>` for each of its binds whose source is absent,
//! and the container has at that path what its image has.

use std::fs;
use std::path::Path;

/// The flag of `apptainer run` and `apptainer exec` that skips one mount.
/// With an absolute path as its value, the mount is the bind of that
/// destination.
pub(crate) const NO_MOUNT_FLAG: &str = "--no-mount";

/// Path of the configuration inside an apptainer install tree.
const APPTAINER_CONF_PATH: &str = "etc/apptainer/apptainer.conf";

/// The key of an entry of `apptainer.conf` that binds paths of the host.
const BIND_PATH_KEY: &str = "bind path";

/// What separates the binds of one `bind path` entry.
const BIND_PATH_SEPARATOR: char = ',';

/// The paths of the host that apptainer binds at the same path into a
/// contained container.
const CONTAINED_BIND_PATHS: [&str; 2] = ["/etc/hosts", "/etc/localtime"];

/// One bind of a `bind path` entry: `<source>` or `<source>:<destination>`.
#[derive(Debug, PartialEq, Eq)]
struct BindPath<'a> {
    /// The path on the host.
    source: &'a str,
    /// The path in the container, which is the source when the bind names
    /// none. `--no-mount` names a bind by it.
    destination: &'a str,
}

impl<'a> BindPath<'a> {
    /// Reads one bind of the value of a `bind path` entry, or `None` for a
    /// bind without a source.
    fn parse(bind: &'a str) -> Option<Self> {
        let mut fields = bind.trim().split(':');
        let source = fields.next().filter(|source| !source.is_empty())?;
        let destination = fields.next().unwrap_or(source);
        Some(Self {
            source,
            destination,
        })
    }

    /// The binds of one line of `apptainer.conf`: those of a `bind path`
    /// entry, which holds one bind or several with [`BIND_PATH_SEPARATOR`]
    /// between them, and none for every other line (another key, a comment,
    /// a blank line).
    fn of_line(line: &'a str) -> impl Iterator<Item = Self> {
        line.split_once('=')
            .filter(|(key, _)| key.trim() == BIND_PATH_KEY)
            .into_iter()
            .flat_map(|(_, value)| value.split(BIND_PATH_SEPARATOR))
            .filter_map(Self::parse)
    }
}

/// The destinations of the binds of apptainer whose source this host does
/// not have.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AbsentBindPaths {
    /// Those of the `bind path` entries of `apptainer.conf`.
    of_configuration: Vec<String>,
    /// Those of [`CONTAINED_BIND_PATHS`].
    of_contained_container: Vec<String>,
}

impl AbsentBindPaths {
    /// Those of the installation at `apptainer_dir` on this host.
    ///
    /// A configuration that cannot be read gives no entry: apptainer reports
    /// a broken configuration itself.
    pub(crate) fn of_installation(apptainer_dir: &Path) -> Self {
        let config =
            fs::read_to_string(apptainer_dir.join(APPTAINER_CONF_PATH)).unwrap_or_default();
        // Apptainer stops on a source that does not exist. Any other failure
        // to read a source is left to apptainer, which names it.
        Self::of_config(&config, |source| {
            matches!(Path::new(source).try_exists(), Ok(false))
        })
    }

    /// Those of `config`, the text of an `apptainer.conf`, on a host where
    /// `is_absent` reports the sources that do not exist.
    pub(crate) fn of_config(config: &str, is_absent: impl Fn(&str) -> bool) -> Self {
        Self {
            of_configuration: config
                .lines()
                .flat_map(BindPath::of_line)
                .filter(|bind| is_absent(bind.source))
                .map(|bind| bind.destination.to_string())
                .collect(),
            of_contained_container: CONTAINED_BIND_PATHS
                .into_iter()
                .filter(|path| is_absent(path))
                .map(str::to_string)
                .collect(),
        }
    }

    /// The destinations that a command skips: those of the binds that
    /// apptainer adds to its container, which `is_contained` tells apart.
    pub(crate) fn of_command(&self, is_contained: bool) -> &[String] {
        if is_contained {
            &self.of_contained_container
        } else {
            &self.of_configuration
        }
    }
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

    fn binds_of_line(line: &str) -> Vec<BindPath<'_>> {
        BindPath::of_line(line).collect()
    }

    /// Writes `config` as the `apptainer.conf` of the installation at
    /// `apptainer_dir`.
    fn write_apptainer_conf(apptainer_dir: &Path, config: &str) {
        let config_dir = apptainer_dir.join("etc/apptainer");
        fs::create_dir_all(&config_dir).expect("create the configuration directory");
        fs::write(config_dir.join("apptainer.conf"), config).expect("write the configuration");
    }

    #[test]
    fn an_entry_with_one_path_binds_it_at_the_same_path() {
        assert_eq!(
            binds_of_line("bind path = /etc/localtime"),
            [BindPath {
                source: "/etc/localtime",
                destination: "/etc/localtime",
            }]
        );
    }

    #[test]
    fn an_entry_with_two_paths_names_the_source_then_the_destination() {
        assert_eq!(
            binds_of_line("bind path = /etc/default-nsswitch.conf:/etc/nsswitch.conf"),
            [BindPath {
                source: "/etc/default-nsswitch.conf",
                destination: "/etc/nsswitch.conf",
            }]
        );
    }

    /// Apptainer splits the value of an entry at each comma, and its own
    /// default is one entry with two binds.
    #[test]
    fn an_entry_with_several_binds_gives_each_of_them() {
        assert_eq!(
            binds_of_line("bind path = /etc/localtime,/etc/hosts"),
            [
                BindPath {
                    source: "/etc/localtime",
                    destination: "/etc/localtime",
                },
                BindPath {
                    source: "/etc/hosts",
                    destination: "/etc/hosts",
                },
            ]
        );
        assert_eq!(
            binds_of_line("bind path = /host/zone:/etc/localtime , /etc/hosts,"),
            [
                BindPath {
                    source: "/host/zone",
                    destination: "/etc/localtime",
                },
                BindPath {
                    source: "/etc/hosts",
                    destination: "/etc/hosts",
                },
            ]
        );
    }

    #[test]
    fn a_line_that_is_not_a_bind_path_entry_gives_no_bind() {
        assert!(binds_of_line("#bind path = /opt").is_empty());
        assert!(binds_of_line("mount hostfs = no").is_empty());
        assert!(binds_of_line("# BIND PATH: [STRING]").is_empty());
        assert!(binds_of_line("bind path =").is_empty());
        assert!(binds_of_line("").is_empty());
    }

    #[test]
    fn only_the_entries_with_an_absent_source_are_skipped() {
        let absent =
            AbsentBindPaths::of_config(BUNDLED_CONFIG, |source| source == "/etc/localtime");

        assert_eq!(absent.of_command(false), ["/etc/localtime"]);
    }

    #[test]
    fn no_entry_is_skipped_on_a_host_that_has_every_source() {
        let absent = AbsentBindPaths::of_config(BUNDLED_CONFIG, |_| false);

        assert!(absent.of_command(false).is_empty());
        assert!(absent.of_command(true).is_empty());
    }

    #[test]
    fn an_entry_is_skipped_by_its_destination() {
        let config = "bind path = /host/nsswitch.conf:/etc/nsswitch.conf\n";

        let absent = AbsentBindPaths::of_config(config, |source| source == "/host/nsswitch.conf");

        assert_eq!(absent.of_command(false), ["/etc/nsswitch.conf"]);
    }

    /// One `--no-mount` value with a comma in it names two destinations, so
    /// each bind of an entry is skipped on its own.
    #[test]
    fn each_absent_bind_of_an_entry_with_several_binds_is_skipped_on_its_own() {
        let config = "bind path = /etc/localtime,/etc/hosts\n";

        let one_absent = AbsentBindPaths::of_config(config, |source| source == "/etc/localtime");
        let both_absent = AbsentBindPaths::of_config(config, |_| true);

        assert_eq!(one_absent.of_command(false), ["/etc/localtime"]);
        assert_eq!(
            both_absent.of_command(false),
            ["/etc/localtime", "/etc/hosts"]
        );
    }

    /// Apptainer binds `/etc/hosts` and `/etc/localtime` into a contained
    /// container without a look at the configuration.
    #[test]
    fn a_contained_container_skips_its_own_binds_whatever_the_configuration_holds() {
        let without_entries = "mount hostfs = no\n";

        let absent =
            AbsentBindPaths::of_config(without_entries, |source| source == "/etc/localtime");

        assert_eq!(absent.of_command(true), ["/etc/localtime"]);
        assert!(absent.of_command(false).is_empty());
    }

    /// An entry that binds another source at `/etc/localtime` does not
    /// depend on the `/etc/localtime` of the host.
    #[test]
    fn a_container_that_is_not_contained_keeps_an_entry_whose_own_source_exists() {
        let config = "bind path = /usr/share/zoneinfo/UTC:/etc/localtime\n";

        let absent = AbsentBindPaths::of_config(config, |source| source == "/etc/localtime");

        assert!(absent.of_command(false).is_empty());
        assert_eq!(absent.of_command(true), ["/etc/localtime"]);
    }

    #[test]
    fn the_configuration_of_an_installation_is_read_against_the_host() {
        let install = scratch_dir();
        let present = install.path().join("present-on-the-host");
        let absent = install.path().join("absent-on-the-host");
        fs::write(&present, "").expect("write the present source");
        write_apptainer_conf(
            install.path(),
            &format!(
                "bind path = {}\nbind path = {}:/etc/localtime\n",
                present.display(),
                absent.display()
            ),
        );

        let absent = AbsentBindPaths::of_installation(install.path());

        assert_eq!(absent.of_command(false), ["/etc/localtime"]);
    }

    #[test]
    fn a_dangling_link_is_an_absent_source() {
        let install = scratch_dir();
        let link = install.path().join("localtime");
        std::os::unix::fs::symlink(install.path().join("no-such-zoneinfo"), &link)
            .expect("create the dangling link");
        write_apptainer_conf(
            install.path(),
            &format!("bind path = {}:/etc/localtime\n", link.display()),
        );

        let absent = AbsentBindPaths::of_installation(install.path());

        assert_eq!(absent.of_command(false), ["/etc/localtime"]);
    }

    #[test]
    fn an_installation_without_a_configuration_skips_no_entry() {
        let install = scratch_dir();

        let absent = AbsentBindPaths::of_installation(install.path());

        assert!(absent.of_command(false).is_empty());
    }
}
