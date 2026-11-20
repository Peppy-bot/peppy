//! The flags that a container node adds to `apptainer run`
//! (`execution.container.apptainer_run_extra_args` of its manifest).
//!
//! A container node gets the paths of the host that the daemon gives it: the
//! sources of its `mount_paths`, which the daemon checks (no top-level system
//! directory, nothing that touches the data root of peppy), and an empty
//! directory over the data root, which holds the keys of the clients of the
//! daemon and the certificate of the machine. A flag of `apptainer run` that
//! mounts a path of the host, reads a file of the host, or changes the mounts
//! of the container goes around these checks. [`ApptainerRunArgs`] refuses
//! each such flag when the manifest is read, so no node starts with one.

use serde::{Deserialize, Serialize};

/// A flag of `apptainer run` that a manifest cannot give.
struct RefusedFlag {
    /// The long form of the flag, without its two dashes.
    long: &'static str,
    /// The form of the flag with one letter, for a flag that has one.
    short: Option<char>,
    /// What the flag does to the container, for the refusal.
    effect: &'static str,
}

/// The refused flags of `apptainer run`.
const REFUSED_FLAGS: &[RefusedFlag] = &[
    RefusedFlag {
        long: "bind",
        short: Some('B'),
        effect: "binds a path of the host into the container",
    },
    RefusedFlag {
        long: "mount",
        short: None,
        effect: "mounts a path of the host into the container",
    },
    RefusedFlag {
        long: "home",
        short: Some('H'),
        effect: "binds a directory of the host as the home directory of the container",
    },
    RefusedFlag {
        long: "workdir",
        short: Some('W'),
        effect: "keeps the temporary directories of the container in a directory of the host",
    },
    RefusedFlag {
        long: "overlay",
        short: Some('o'),
        effect: "mounts an overlay of the host over the container",
    },
    RefusedFlag {
        long: "fusemount",
        short: None,
        effect: "mounts a FUSE file system of the host into the container",
    },
    RefusedFlag {
        long: "env-file",
        short: None,
        effect: "reads a file of the host into the environment of the container",
    },
    RefusedFlag {
        long: "no-mount",
        short: None,
        effect: "removes mounts that the daemon gives the container",
    },
];

/// The one-letter flag of `apptainer run` that takes a value and that a
/// manifest can give: `-S`, the `--scratch` directories of the container. In
/// a group of one-letter flags (`-cS/scratch`), apptainer gives the rest of
/// the argument after this letter to the flag as its value. The other
/// one-letter flags that take a value are refused flags.
const ALLOWED_SHORT_FLAG_WITH_VALUE: char = 'S';

/// The flags that a container node adds to `apptainer run`, in the order of
/// the manifest. None of them is a [refused flag](REFUSED_FLAGS): a manifest
/// that gives one does not parse.
///
/// A manifest gives the paths of the host that its node needs in
/// `mount_paths`, which the daemon checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ApptainerRunArgs(Vec<String>);

impl TryFrom<Vec<String>> for ApptainerRunArgs {
    type Error = String;

    fn try_from(arguments: Vec<String>) -> Result<Self, Self::Error> {
        let refusal = arguments.iter().find_map(|argument| {
            let flag = refused_flag_of(argument)?;
            Some(format!(
                "`apptainer_run_extra_args` cannot hold `{argument}`: `--{}` {}. Give the \
                 paths of the host that the node needs in `mount_paths`, which keep the data \
                 root of peppy out of the container",
                flag.long, flag.effect
            ))
        });
        match refusal {
            Some(refusal) => Err(refusal),
            None => Ok(Self(arguments)),
        }
    }
}

impl From<ApptainerRunArgs> for Vec<String> {
    fn from(arguments: ApptainerRunArgs) -> Self {
        arguments.0
    }
}

impl std::ops::Deref for ApptainerRunArgs {
    type Target = [String];

    fn deref(&self) -> &[String] {
        &self.0
    }
}

/// The refused flag that `argument` gives, or `None` when it gives none.
///
/// Apptainer reads its flags as follows:
/// - A long flag is `--name` or `--name=value`.
/// - A group of one-letter flags is one dash and their letters (`-cB/data` is
///   `-c` and `-B /data`). The first letter that takes a value takes the rest
///   of the argument as its value.
///
/// An argument that is the value of the flag before it (`--hostname -B`) is
/// read as a flag also: the check refuses it, and the manifest gives the
/// value after an equals sign instead (`--hostname=-B`).
fn refused_flag_of(argument: &str) -> Option<&'static RefusedFlag> {
    if let Some(long_flag) = argument.strip_prefix("--") {
        let name = long_flag.split('=').next().unwrap_or_default();
        return REFUSED_FLAGS.iter().find(|flag| flag.long == name);
    }
    let letters = argument.strip_prefix('-')?;
    letters
        .chars()
        .take_while(|letter| *letter != ALLOWED_SHORT_FLAG_WITH_VALUE)
        .find_map(|letter| REFUSED_FLAGS.iter().find(|flag| flag.short == Some(letter)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<ApptainerRunArgs, String> {
        ApptainerRunArgs::try_from(
            arguments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn each_refused_flag_is_refused_in_its_long_forms() {
        for flag in REFUSED_FLAGS {
            for arguments in [
                vec![format!("--{}", flag.long), "/data".to_owned()],
                vec![format!("--{}=/data", flag.long)],
            ] {
                let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
                let refusal = parse(&arguments).expect_err("a refused flag does not parse");
                assert!(
                    refusal.contains(&format!("cannot hold `{}`", arguments[0])),
                    "{refusal}"
                );
                assert!(refusal.contains(flag.effect), "{refusal}");
            }
        }
    }

    #[test]
    fn each_refused_flag_is_refused_in_its_one_letter_forms() {
        for (flag, letter) in REFUSED_FLAGS
            .iter()
            .filter_map(|flag| Some((flag, flag.short?)))
        {
            for arguments in [
                vec![format!("-{letter}"), "/data".to_owned()],
                vec![format!("-{letter}=/data")],
                vec![format!("-{letter}/data")],
            ] {
                let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
                let refusal = parse(&arguments).expect_err("a refused flag does not parse");
                assert!(refusal.contains(&format!("`--{}`", flag.long)), "{refusal}");
            }
        }
    }

    /// `-cB/root` is `-c` (`--contain`) and `-B /root`.
    #[test]
    fn a_refused_flag_in_a_group_of_one_letter_flags_is_refused() {
        for argument in ["-cB/root", "-eH", "-cuo/srv/overlay", "-pW/srv/work"] {
            assert!(parse(&[argument]).is_err(), "{argument}");
        }
    }

    /// The `/` of the bind of the data root is no less refused when it is the
    /// whole host.
    #[test]
    fn a_bind_of_the_data_root_or_of_the_host_is_refused() {
        for arguments in [
            &["--bind", "~/.peppy:/mnt/stolen"][..],
            &["-B", "/:/mnt/host"][..],
            &["--nv", "--bind=/home/user/.peppy/conf/peer:/peer"][..],
        ] {
            assert!(parse(arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn flags_that_mount_nothing_of_the_host_parse() {
        let arguments = [
            "--nv",
            "--no-setgroups",
            "--writable-tmpfs",
            "--contain",
            "-c",
            "--cleanenv",
            "--hostname=robot",
            "--env",
            "MODE=fast",
            "--scratch",
            "/scratch",
            "-S/scratch/overlay-dir",
            "-cS/home/Bob",
        ];
        let parsed = parse(&arguments).expect("no flag mounts a path of the host");
        assert_eq!(&*parsed, arguments.map(String::from).as_slice());
    }

    /// A long flag whose name starts with the name of a refused flag is not
    /// that flag.
    #[test]
    fn a_long_flag_is_read_by_its_whole_name() {
        for argument in ["--bindings", "--home-dir-of-nothing"] {
            parse(&[argument]).unwrap_or_else(|refusal| panic!("{argument}: {refusal}"));
        }
    }

    #[test]
    fn an_argument_that_is_no_flag_parses() {
        parse(&["/data", "B", "W", "-"]).expect("no argument is a flag");
    }

    #[test]
    fn no_arguments_parse() {
        assert_eq!(parse(&[]), Ok(ApptainerRunArgs::default()));
    }

    #[test]
    fn the_arguments_serialize_as_the_list_of_the_manifest() {
        let parsed = parse(&["--nv", "--no-setgroups"]).expect("the flags parse");
        let json = serde_json::to_string(&parsed).expect("the arguments serialize");
        assert_eq!(json, r#"["--nv","--no-setgroups"]"#);
        let read: ApptainerRunArgs = serde_json::from_str(&json).expect("the arguments read");
        assert_eq!(read, parsed);
    }
}
