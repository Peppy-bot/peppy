//! The paths of the host that the raw flags of a `run` or an `exec` hand to
//! apptainer (see [`ApptainerCommand::raw_flag`](super::facade::ApptainerCommand::raw_flag)).
//!
//! A node gives such flags in `apptainer_run_extra_args` of its manifest.
//! Apptainer puts each of these paths into the container, as it does the
//! source of a bind of [`ApptainerCommand::bind`](super::facade::ApptainerCommand::bind),
//! so the checks that read the bind sources of a command read these paths
//! also:
//!
//! | Flag              | Value                                           | Path          |
//! |-------------------|-------------------------------------------------|---------------|
//! | `--bind`, `-B`    | `src[:dest[:opts]]`, several divided by a comma | Each `src`.   |
//! | `--mount`         | `type=bind,source=src,destination=dest`         | `src`.        |
//! | `--home`, `-H`    | `src[:dest]`                                    | `src`.        |
//! | `--workdir`, `-W` | A directory.                                    | The directory.|
//! | `--overlay`, `-o` | `path[:ro]`                                     | `path`.       |
//!
//! A flag gives its value in the next argument (`--bind /data`), after an
//! equals sign (`--bind=/data`, `-B=/data`), or after its one letter
//! (`-B/data`).

use std::path::{Path, PathBuf};

/// A flag of `apptainer run` and `apptainer exec` whose value names a path of
/// the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathFlag {
    Bind,
    Mount,
    Home,
    Workdir,
    Overlay,
}

impl PathFlag {
    const ALL: [Self; 5] = [
        Self::Bind,
        Self::Mount,
        Self::Home,
        Self::Workdir,
        Self::Overlay,
    ];

    /// The long form of the flag.
    const fn long(self) -> &'static str {
        match self {
            Self::Bind => "--bind",
            Self::Mount => "--mount",
            Self::Home => "--home",
            Self::Workdir => "--workdir",
            Self::Overlay => "--overlay",
        }
    }

    /// The form of the flag with one letter, for a flag that has one.
    const fn short(self) -> Option<&'static str> {
        match self {
            Self::Bind => Some("-B"),
            Self::Mount => None,
            Self::Home => Some("-H"),
            Self::Workdir => Some("-W"),
            Self::Overlay => Some("-o"),
        }
    }

    /// The paths of the host that `value`, the value of this flag, names.
    fn host_paths(self, value: &str) -> Vec<&str> {
        /// The part of `spec` before its first colon.
        fn before_colon(spec: &str) -> Option<&str> {
            spec.split(':').next()
        }
        let paths: Vec<&str> = match self {
            Self::Bind => value.split(',').filter_map(before_colon).collect(),
            Self::Mount => value
                .split(',')
                .filter_map(|field| {
                    field
                        .strip_prefix("source=")
                        .or_else(|| field.strip_prefix("src="))
                })
                .collect(),
            Self::Home | Self::Overlay => before_colon(value).into_iter().collect(),
            Self::Workdir => vec![value],
        };
        paths.into_iter().filter(|path| !path.is_empty()).collect()
    }
}

/// How one argument of a command gives the value of a [`PathFlag`].
#[derive(Debug, PartialEq, Eq)]
enum FlagValue<'a> {
    /// The argument holds the value.
    Here(&'a str),
    /// The next argument is the value.
    InNextArgument,
}

/// The [`PathFlag`] that `argument` is, with how it gives its value, or
/// `None` for an argument that is no such flag.
fn path_flag_of(argument: &str) -> Option<(PathFlag, FlagValue<'_>)> {
    PathFlag::ALL.into_iter().find_map(|flag| {
        let after_long = argument.strip_prefix(flag.long());
        let after_short = || argument.strip_prefix(flag.short()?);
        let value = match (after_long, after_short()) {
            (Some(""), _) | (None, Some("")) => FlagValue::InNextArgument,
            (Some(rest), _) => FlagValue::Here(rest.strip_prefix('=')?),
            (None, Some(rest)) => FlagValue::Here(rest.strip_prefix('=').unwrap_or(rest)),
            (None, None) => return None,
        };
        Some((flag, value))
    })
}

/// The paths of the host that `flags`, the raw flags of a command, hand to
/// apptainer. Apptainer reads a path that is not absolute from the directory
/// of its process: `working_dir` when the command names one, or else the
/// working directory of the daemon, which reads such a path the same way.
pub(crate) fn host_paths_of_flags(flags: &[String], working_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut arguments = flags.iter().map(String::as_str);
    while let Some(argument) = arguments.next() {
        let Some((flag, value)) = path_flag_of(argument) else {
            continue;
        };
        let value = match value {
            FlagValue::Here(value) => Some(value),
            FlagValue::InNextArgument => arguments.next(),
        };
        let Some(value) = value else { continue };
        paths.extend(
            flag.host_paths(value)
                .into_iter()
                .map(|path| match working_dir {
                    Some(dir) => dir.join(path),
                    None => PathBuf::from(path),
                }),
        );
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_of(flags: &[&str]) -> Vec<PathBuf> {
        let flags: Vec<String> = flags.iter().map(ToString::to_string).collect();
        host_paths_of_flags(&flags, None)
    }

    fn paths(expected: &[&str]) -> Vec<PathBuf> {
        expected.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_bind_flag_names_the_source_of_each_of_its_binds() {
        assert_eq!(paths_of(&["--bind", "/data"]), paths(&["/data"]));
        assert_eq!(
            paths_of(&["--bind", "/data:/mnt/data:ro,/models:/models"]),
            paths(&["/data", "/models"])
        );
    }

    #[test]
    fn a_flag_gives_its_value_in_each_of_its_forms() {
        for flags in [
            &["--bind", "/data:/data"][..],
            &["--bind=/data:/data"][..],
            &["-B", "/data:/data"][..],
            &["-B=/data:/data"][..],
            &["-B/data:/data"][..],
        ] {
            assert_eq!(paths_of(flags), paths(&["/data"]), "{flags:?}");
        }
    }

    #[test]
    fn each_flag_that_names_a_path_of_the_host_is_read() {
        let cases = [
            (
                &["--mount", "type=bind,source=/data,destination=/mnt,ro"][..],
                "/data",
            ),
            (&["--mount", "type=bind,src=/data,dst=/mnt"][..], "/data"),
            (&["--home", "/srv/home:/home/node"][..], "/srv/home"),
            (&["-H", "/srv/home"][..], "/srv/home"),
            (&["--workdir", "/srv/work"][..], "/srv/work"),
            (&["-W", "/srv/work"][..], "/srv/work"),
            (&["--overlay", "/srv/overlay:ro"][..], "/srv/overlay"),
            (&["-o", "/srv/overlay.img"][..], "/srv/overlay.img"),
        ];
        for (flags, path) in cases {
            assert_eq!(paths_of(flags), paths(&[path]), "{flags:?}");
        }
    }

    /// The paths of each flag are read, between the flags that name no path
    /// of the host.
    #[test]
    fn the_paths_of_several_flags_are_in_the_order_of_the_flags() {
        assert_eq!(
            paths_of(&[
                "--nv",
                "--bind",
                "/data",
                "--pwd",
                "/work",
                "--home=/srv/home",
                "--env",
                "MODE=--bind",
            ]),
            paths(&["/data", "/srv/home"])
        );
    }

    #[test]
    fn a_flag_that_names_no_path_of_the_host_is_not_read() {
        assert_eq!(
            paths_of(&["--nv", "--scratch", "/var/cache", "--binding", "/data"]),
            paths(&[])
        );
        // A flag without its value, and a value that names no path.
        assert_eq!(paths_of(&["--bind"]), paths(&[]));
        assert_eq!(paths_of(&["--bind", ""]), paths(&[]));
        assert_eq!(
            paths_of(&["--mount", "type=bind,destination=/mnt"]),
            paths(&[])
        );
    }

    /// Apptainer reads a path that is not absolute from the directory of
    /// its process.
    #[test]
    fn a_relative_path_is_in_the_working_directory_of_the_command() {
        let flags = ["--bind".to_string(), "data:/data,/models".to_string()];

        assert_eq!(
            host_paths_of_flags(&flags, Some(Path::new("/srv/instance"))),
            paths(&["/srv/instance/data", "/models"])
        );
        assert_eq!(
            host_paths_of_flags(&flags, None),
            paths(&["data", "/models"])
        );
    }
}
