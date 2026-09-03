//! What `~/.ssh/config` selects for a host, read the way `ssh` itself reads
//! it. `ssh -G <host>` prints the configuration `ssh` would connect with
//! after every `Host` and `Match` block, `Include` file and system default
//! has been applied, so peppy never parses the file on its own and cannot
//! disagree with `ssh` about which agent socket and identity files apply to
//! a host. Two directives are read: `IdentityAgent`, and the `IdentityFile`
//! list.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The host, user and port a git ssh URL connects to: what `ssh` matches
/// its configuration against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl SshTarget {
    /// Parses the URL forms libgit2 connects over ssh: `ssh://`, `git+ssh://`
    /// and `ssh+git://` URLs, and the scp-style `[user@]host:path`. Any other
    /// URL is not an ssh target.
    pub fn from_git_url(url: &str) -> Option<Self> {
        if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(scheme, "ssh" | "git+ssh" | "ssh+git") {
                return None;
            }
            let parsed = url::Url::parse(&format!("ssh://{rest}")).ok()?;
            let host = match parsed.host()? {
                url::Host::Domain(domain) => domain.to_owned(),
                url::Host::Ipv4(address) => address.to_string(),
                url::Host::Ipv6(address) => address.to_string(),
            };
            let user = (!parsed.username().is_empty()).then(|| parsed.username().to_owned());
            return Some(Self {
                user,
                host,
                port: parsed.port(),
            });
        }
        let (authority, _path) = url.split_once(':')?;
        if authority.is_empty() || authority.contains('/') {
            return None;
        }
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user).filter(|user| !user.is_empty()), host),
            None => (None, authority),
        };
        if host.is_empty() {
            return None;
        }
        Some(Self {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            port: None,
        })
    }

    /// A host no `Host` or `Match` block names, so what `ssh -G` resolves for
    /// it is the configuration every host shares: the `Host *` blocks and
    /// the defaults.
    pub fn without_host_specific_configuration() -> Self {
        Self {
            user: None,
            host: "host.invalid".to_owned(),
            port: None,
        }
    }
}

/// The agent `ssh` uses for a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityAgent {
    /// No `IdentityAgent` directive, or `IdentityAgent SSH_AUTH_SOCK`: the
    /// agent `SSH_AUTH_SOCK` names.
    FromEnvironment,
    /// `IdentityAgent none`, or `IdentityAgent $NAME` with `NAME` unset: no
    /// agent at all.
    Disabled,
    /// `IdentityAgent <path>`, or `IdentityAgent $NAME` with `NAME` set: the
    /// agent at this socket.
    Socket(PathBuf),
}

/// What `ssh` selects for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHostConfig {
    pub identity_agent: IdentityAgent,
    /// The `IdentityFile` paths in the order `ssh` offers them, with `~` and
    /// the tokens `%d`, `%h`, `%n`, `%p`, `%r` and `%%` expanded. Whether a
    /// path exists is not checked here.
    pub identity_files: Vec<PathBuf>,
}

/// Reads what `ssh` selects for `target`. `Ok(None)` when no `ssh` is on the
/// PATH; `Err` when `ssh -G` fails, which is what `ssh` itself does with that
/// configuration (a bad directive, an unreadable `Include`).
pub fn resolve_host_config(target: &SshTarget) -> Result<Option<SshHostConfig>, String> {
    let output = match resolve_command(target, None).output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("could not run ssh -G {}: {err}", target.host)),
    };
    if !output.status.success() {
        return Err(format!(
            "ssh -G {} failed, so what ~/.ssh/config selects for that host is unknown: {}",
            target.host,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let home = dirs::home_dir();
    Ok(Some(parse_resolved_config(
        &stdout,
        target,
        home.as_deref(),
        &|name| std::env::var(name).ok(),
    )))
}

/// The `ssh -G` invocation that resolves `target`. `config_file` replaces the
/// user's and the system's configuration files, which the tests use to keep
/// the machine's `~/.ssh/config` out of the resolution.
fn resolve_command(target: &SshTarget, config_file: Option<&Path>) -> Command {
    let mut command = Command::new("ssh");
    command.arg("-G");
    if let Some(config_file) = config_file {
        command.arg("-F").arg(config_file);
    }
    if let Some(user) = &target.user {
        command.arg("-l").arg(user);
    }
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }
    command.arg("--").arg(&target.host);
    command.stdin(Stdio::null());
    command
}

/// The values `ssh` substitutes for the tokens an `IdentityFile` may carry.
struct TokenValues<'a> {
    home: Option<&'a Path>,
    /// `%h`: the host name after `HostName` rewriting.
    hostname: &'a str,
    /// `%n`: the host name as the URL spelled it.
    original_host: &'a str,
    /// `%p`: the resolved port.
    port: Option<&'a str>,
    /// `%r`: the resolved remote user.
    user: Option<&'a str>,
}

/// Parses the `key value` lines `ssh -G` prints. `env` answers the
/// `IdentityAgent $NAME` form, which `ssh -G` prints unexpanded.
fn parse_resolved_config(
    stdout: &str,
    target: &SshTarget,
    home: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> SshHostConfig {
    let directives: Vec<(&str, &str)> = stdout
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(key, value)| (key, value.trim()))
        .collect();
    let value_of = |key: &str| {
        directives
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| *value)
    };
    let identity_agent = match value_of("identityagent") {
        None => IdentityAgent::FromEnvironment,
        Some(value) => parse_identity_agent(value, env),
    };
    let tokens = TokenValues {
        home,
        hostname: value_of("hostname").unwrap_or(&target.host),
        original_host: &target.host,
        port: value_of("port"),
        user: value_of("user"),
    };
    let identity_files = directives
        .iter()
        .filter(|(key, _)| *key == "identityfile")
        .map(|(_, value)| expand_identity_file(value, &tokens))
        .collect();
    SshHostConfig {
        identity_agent,
        identity_files,
    }
}

fn parse_identity_agent(value: &str, env: &dyn Fn(&str) -> Option<String>) -> IdentityAgent {
    match value {
        "none" => IdentityAgent::Disabled,
        "SSH_AUTH_SOCK" => IdentityAgent::FromEnvironment,
        variable if variable.starts_with('$') => match env(&variable[1..]) {
            Some(path) if !path.is_empty() => IdentityAgent::Socket(PathBuf::from(path)),
            _ => IdentityAgent::Disabled,
        },
        path => IdentityAgent::Socket(PathBuf::from(path)),
    }
}

/// Expands the tokens and the leading `~` of an `IdentityFile` value, which
/// `ssh -G` prints as written. A token peppy does not know the value of is
/// left as printed, so the path names a file that does not exist.
fn expand_identity_file(value: &str, tokens: &TokenValues<'_>) -> PathBuf {
    let mut expanded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            expanded.push(character);
            continue;
        }
        let token = chars.next();
        let substitution = match token {
            Some('%') => Some("%".to_owned()),
            Some('d') => tokens.home.map(|home| home.to_string_lossy().into_owned()),
            Some('h') => Some(tokens.hostname.to_owned()),
            Some('n') => Some(tokens.original_host.to_owned()),
            Some('p') => tokens.port.map(str::to_owned),
            Some('r') => tokens.user.map(str::to_owned),
            _ => None,
        };
        match (substitution, token) {
            (Some(substitution), _) => expanded.push_str(&substitution),
            (None, Some(token)) => {
                expanded.push('%');
                expanded.push(token);
            }
            (None, None) => expanded.push('%'),
        }
    }
    expand_tilde(&expanded, tokens.home)
}

fn expand_tilde(value: &str, home: Option<&Path>) -> PathBuf {
    match (value.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn github() -> SshTarget {
        SshTarget {
            user: Some("git".to_owned()),
            host: "github.com".to_owned(),
            port: None,
        }
    }

    #[test]
    fn scp_style_urls_name_their_user_and_host() {
        assert_eq!(
            SshTarget::from_git_url("git@github.com:Peppy-bot/private-nodes-hub.git"),
            Some(github())
        );
        for url in [
            "github.com:Peppy-bot/private-nodes-hub.git",
            "@github.com:Peppy-bot/private-nodes-hub.git",
        ] {
            assert_eq!(
                SshTarget::from_git_url(url),
                Some(SshTarget {
                    user: None,
                    host: "github.com".to_owned(),
                    port: None,
                }),
                "{url}"
            );
        }
    }

    #[test]
    fn ssh_scheme_urls_name_their_user_host_and_port() {
        for url in [
            "ssh://git@github.com:2222/Peppy-bot/private-nodes-hub.git",
            "git+ssh://git@github.com:2222/Peppy-bot/private-nodes-hub.git",
            "ssh+git://git@github.com:2222/Peppy-bot/private-nodes-hub.git",
        ] {
            assert_eq!(
                SshTarget::from_git_url(url),
                Some(SshTarget {
                    user: Some("git".to_owned()),
                    host: "github.com".to_owned(),
                    port: Some(2222),
                }),
                "{url}"
            );
        }
        assert_eq!(
            SshTarget::from_git_url("ssh://[::1]/repo.git"),
            Some(SshTarget {
                user: None,
                host: "::1".to_owned(),
                port: None,
            })
        );
    }

    #[test]
    fn urls_that_do_not_connect_over_ssh_are_not_targets() {
        for url in [
            "https://github.com/Peppy-bot/nodes-hub.git",
            "git://github.com/Peppy-bot/nodes-hub.git",
            "file:///srv/repo.git",
            "/srv/repo.git",
            "./relative/path",
            ":no-host",
            "@:no-host",
        ] {
            assert_eq!(SshTarget::from_git_url(url), None, "{url}");
        }
    }

    #[test]
    fn an_absent_identity_agent_directive_means_the_environment_agent() {
        let config = parse_resolved_config(
            "user git\nhostname github.com\nport 22\n",
            &github(),
            None,
            &no_env,
        );
        assert_eq!(config.identity_agent, IdentityAgent::FromEnvironment);
        assert!(config.identity_files.is_empty());
    }

    #[test]
    fn identity_agent_values_follow_the_ssh_config_forms() {
        let parse = |line: &str, env: &dyn Fn(&str) -> Option<String>| {
            parse_resolved_config(&format!("{line}\n"), &github(), None, env).identity_agent
        };
        assert_eq!(
            parse(
                "identityagent /Users/me/Library/Group Containers/1password/agent.sock",
                &no_env
            ),
            IdentityAgent::Socket(PathBuf::from(
                "/Users/me/Library/Group Containers/1password/agent.sock"
            ))
        );
        assert_eq!(
            parse("identityagent none", &no_env),
            IdentityAgent::Disabled
        );
        assert_eq!(
            parse("identityagent SSH_AUTH_SOCK", &no_env),
            IdentityAgent::FromEnvironment
        );
        let agent_var = |name: &str| (name == "MY_AGENT").then(|| "/run/my.sock".to_owned());
        assert_eq!(
            parse("identityagent $MY_AGENT", &agent_var),
            IdentityAgent::Socket(PathBuf::from("/run/my.sock"))
        );
        assert_eq!(
            parse("identityagent $UNSET_AGENT", &agent_var),
            IdentityAgent::Disabled
        );
    }

    #[test]
    fn identity_files_keep_their_order_and_expand_tilde_and_tokens() {
        let stdout = "user git\nhostname ssh.github.com\nport 443\n\
                      identityfile ~/.ssh/id_rsa\n\
                      identityfile %d/.ssh/%r_at_%h_%p\n\
                      identityfile /keys/%n_%%\n\
                      identityfile ~\n\
                      identityfile /keys/%u_unknown\n";
        let config = parse_resolved_config(stdout, &github(), Some(Path::new("/home/me")), &no_env);
        assert_eq!(
            config.identity_files,
            vec![
                PathBuf::from("/home/me/.ssh/id_rsa"),
                PathBuf::from("/home/me/.ssh/git_at_ssh.github.com_443"),
                PathBuf::from("/keys/github.com_%"),
                PathBuf::from("/home/me"),
                PathBuf::from("/keys/%u_unknown"),
            ]
        );
    }

    #[test]
    fn without_a_home_directory_tilde_and_home_tokens_stay_as_printed() {
        let config = parse_resolved_config(
            "identityfile ~/.ssh/id_rsa\nidentityfile %d/.ssh/id_rsa\n",
            &github(),
            None,
            &no_env,
        );
        assert_eq!(
            config.identity_files,
            vec![
                PathBuf::from("~/.ssh/id_rsa"),
                PathBuf::from("%d/.ssh/id_rsa")
            ]
        );
    }

    #[test]
    fn the_resolve_command_passes_the_target_the_way_ssh_expects_it() {
        let target = SshTarget {
            user: Some("git".to_owned()),
            host: "github.com".to_owned(),
            port: Some(2222),
        };
        let command = resolve_command(&target, Some(Path::new("/tmp/ssh_config")));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-G",
                "-F",
                "/tmp/ssh_config",
                "-l",
                "git",
                "-p",
                "2222",
                "--",
                "github.com"
            ]
        );
    }

    /// Runs the real `ssh -G` against a configuration file written for the
    /// test, so the parser sees output the installed `ssh` produces. The
    /// test passes vacuously on a host without `ssh`.
    #[test]
    fn ssh_g_output_resolves_host_blocks_the_way_ssh_does() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("ssh_config");
        let socket = dir.path().join("agent.sock");
        std::fs::write(
            &config_file,
            format!(
                "Host github.com\n  IdentityFile ~/.ssh/github_only\n\
                 Host *\n  IdentityAgent \"{}\"\n  IdentityFile ~/.ssh/everywhere\n",
                socket.display()
            ),
        )
        .unwrap();

        let output = match resolve_command(&github(), Some(&config_file)).output() {
            Ok(output) => output,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => panic!("ssh -G could not run: {err}"),
        };
        assert!(
            output.status.success(),
            "ssh -G failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let config = parse_resolved_config(
            &String::from_utf8_lossy(&output.stdout),
            &github(),
            Some(Path::new("/home/me")),
            &no_env,
        );

        assert_eq!(config.identity_agent, IdentityAgent::Socket(socket));
        assert_eq!(
            config.identity_files,
            vec![
                PathBuf::from("/home/me/.ssh/github_only"),
                PathBuf::from("/home/me/.ssh/everywhere"),
            ]
        );
    }
}
