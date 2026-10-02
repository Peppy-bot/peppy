//! How a git or HTTP source URL appears in a log line or an error.

use std::borrow::Cow;
use std::fmt;

/// A source URL as shown to an operator: without its credentials, its query
/// and its fragment. An `ssh` URL keeps its user name, which names the account
/// (`ssh://git@host/...`), and loses its password. A URL of any other scheme
/// loses its whole user part, since a token is commonly written as the user
/// name. A value the `url` crate reads no host from, such as an scp-style
/// `git@host:path`, a local path or a malformed URL, loses the user part of
/// `user:password@` and its query.
#[derive(Clone, Copy)]
pub(crate) struct SourceUrl<'a>(&'a str);

impl<'a> SourceUrl<'a> {
    pub(crate) fn new(url: &'a str) -> Self {
        Self(url)
    }

    /// `text` with every occurrence of the URL as written replaced by the URL
    /// as shown, for an error message a library formatted around the URL.
    pub(crate) fn redact_in(self, text: &str) -> String {
        match self.shown() {
            Cow::Borrowed(_) => text.to_owned(),
            Cow::Owned(shown) => text.replace(self.0, &shown),
        }
    }

    fn shown(self) -> Cow<'a, str> {
        let mut url = match url::Url::parse(self.0) {
            Ok(url) if url.has_host() => url,
            _ => return shown_unparsed(self.0),
        };
        let keeps_user = matches!(url.scheme(), "ssh" | "git+ssh" | "ssh+git");
        let hides_user = !keeps_user && !url.username().is_empty();
        let hides_anything = hides_user
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some();
        if !hides_anything {
            return Cow::Borrowed(self.0);
        }
        // Both setters fail only for a URL with no host, which has no user
        // part to hide.
        if hides_user {
            let _ = url.set_username("");
        }
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        Cow::Owned(url.into())
    }
}

/// `raw`, which the `url` crate reads no host from, without the `user:password`
/// in front of its host and without its query. A user name alone stays: it is
/// the account of an scp-style `git@host:path`.
fn shown_unparsed(raw: &str) -> Cow<'_, str> {
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, raw),
    };
    // The user part ends at the last `@` before the path, whatever it holds.
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    let after_user = authority
        .rsplit_once('@')
        // A malformed URL hides its whole user part, like a parsed one.
        .filter(|(user, _)| scheme.is_some() || user.contains(':'))
        .map_or(rest, |(user, _)| &rest[user.len() + 1..]);
    let without_query = after_user
        .split_once('?')
        .map_or(after_user, |(before, _)| before);
    if without_query.len() == rest.len() {
        return Cow::Borrowed(raw);
    }
    match scheme {
        Some(scheme) => Cow::Owned(format!("{scheme}://{without_query}")),
        None => Cow::Borrowed(without_query),
    }
}

impl fmt::Display for SourceUrl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.shown())
    }
}

#[cfg(test)]
mod tests {
    use super::SourceUrl;

    fn shown(url: &str) -> String {
        SourceUrl::new(url).to_string()
    }

    #[test]
    fn an_https_url_loses_its_user_part_its_query_and_its_fragment() {
        for (written, expected) in [
            (
                "https://user:secret@host.example/org/repo.git",
                "https://host.example/org/repo.git",
            ),
            (
                "https://ghp_token@host.example/org/repo.git",
                "https://host.example/org/repo.git",
            ),
            (
                "http://host.example:8080/bundle.tar.zst?signature=abc&expires=1",
                "http://host.example:8080/bundle.tar.zst",
            ),
            (
                "https://host.example/bundle.tar.zst#fragment",
                "https://host.example/bundle.tar.zst",
            ),
        ] {
            assert_eq!(shown(written), expected, "{written}");
        }
    }

    #[test]
    fn an_ssh_url_keeps_its_user_name_and_loses_its_password() {
        assert_eq!(
            shown("ssh://git:secret@host.example/org/repo.git"),
            "ssh://git@host.example/org/repo.git"
        );
    }

    #[test]
    fn a_url_of_another_scheme_loses_its_whole_user_part() {
        assert_eq!(
            shown("git+https://ghp_token@host.example/org/repo.git"),
            "git+https://host.example/org/repo.git"
        );
    }

    #[test]
    fn a_value_with_no_readable_host_loses_its_credentials_and_its_query() {
        for (written, expected) in [
            (
                "https://user:secret@host.example:99999/org/repo.git",
                "https://host.example:99999/org/repo.git",
            ),
            (
                "https://ghp_token@[::1/org/repo.git",
                "https://[::1/org/repo.git",
            ),
            (
                "user:secret@host.example/org/repo.git",
                "host.example/org/repo.git",
            ),
            (
                "/srv/bundles/node.tar.zst?signature=abc",
                "/srv/bundles/node.tar.zst",
            ),
            (
                "https://user:se?cret@host.example:99999/org/repo.git",
                "https://host.example:99999/org/repo.git",
            ),
        ] {
            assert_eq!(shown(written), expected, "{written}");
        }
    }

    #[test]
    fn a_url_with_nothing_to_hide_is_shown_as_written() {
        for written in [
            "https://host.example/org/repo.git",
            "https://host.example",
            "ssh://git@host.example/org/repo.git",
            "git@host.example:org/repo.git",
            "/srv/repos/nodes",
            "file:///srv/repos/nodes",
        ] {
            assert_eq!(shown(written), written);
        }
    }

    #[test]
    fn redact_in_rewrites_the_url_inside_a_library_error() {
        let written = "https://user:secret@host.example/org/repo.git";
        let error = format!("authentication failed for '{written}'");
        assert_eq!(
            SourceUrl::new(written).redact_in(&error),
            "authentication failed for 'https://host.example/org/repo.git'"
        );
        assert_eq!(
            SourceUrl::new("https://host.example/repo.git").redact_in("unreachable"),
            "unreachable"
        );
    }
}
