//! Which restart policy an instance runs under.
//!
//! Four levels can write one: the instance (its launcher entry, an
//! adjustment, or the flags of a single run), the node's own `peppy.json5`,
//! the launcher root, and the built-in `on_failure`. The first written wins,
//! taken whole: a cap written at one level never combines with a policy from
//! another.

use config::node::{ResolvedRestart, RestartDeclaration, RestartOrigin, RestartRequest};

use super::types::DeploymentInstance;

/// The launcher's say for one instance of a flat launcher: its entry's
/// `framework.restart` (which an adjustment may have written), else the
/// launcher root's, else nothing.
pub fn restart_request_of(
    root: Option<RestartDeclaration>,
    instance: &DeploymentInstance,
) -> RestartRequest {
    instance
        .framework
        .restart
        .map(RestartRequest::Instance)
        .or(root.map(RestartRequest::LauncherRoot))
        .unwrap_or_default()
}

/// The policy written on the instance itself, by its entry, an adjustment
/// or the flags, which no other level overrides; `None` when its node, the
/// launcher root or the default decides it.
pub fn instance_restart(request: RestartRequest) -> Option<ResolvedRestart> {
    match request {
        RestartRequest::Instance(_) | RestartRequest::Flag(_) => request.written(),
        RestartRequest::LauncherRoot(_) | RestartRequest::Unset => None,
    }
}

/// Resolves one instance's policy from the launcher's or flag's request and
/// the declaration of the node's own `peppy.json5`: the instance's own
/// level, then the node's, then the launcher root's, then the default.
pub fn resolve_restart(
    request: RestartRequest,
    node_declaration: Option<RestartDeclaration>,
) -> ResolvedRestart {
    let node = node_declaration.map(|declaration| ResolvedRestart {
        declaration,
        origin: RestartOrigin::Node,
    });
    instance_restart(request)
        .or(node)
        .or(request.written())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::RestartPolicy;
    use std::num::NonZeroU32;

    fn on_failure() -> RestartDeclaration {
        RestartDeclaration::policy_only(RestartPolicy::OnFailure)
    }

    fn always_capped() -> RestartDeclaration {
        RestartDeclaration::new(RestartPolicy::Always, NonZeroU32::new(3)).unwrap()
    }

    fn never() -> RestartDeclaration {
        RestartDeclaration::policy_only(RestartPolicy::Never)
    }

    /// An instance's entry is its request, then the launcher root's policy,
    /// then nothing.
    #[test]
    fn the_launchers_request_is_the_entry_then_the_root() {
        let bare = DeploymentInstance::empty(config::runtime::Name::new("arm_inst").unwrap());
        let mut written = bare.clone();
        written.framework.restart = Some(always_capped());
        assert_eq!(
            restart_request_of(Some(on_failure()), &written),
            RestartRequest::Instance(always_capped())
        );
        assert_eq!(
            restart_request_of(Some(on_failure()), &bare),
            RestartRequest::LauncherRoot(on_failure())
        );
        assert_eq!(restart_request_of(None, &bare), RestartRequest::Unset);
    }

    #[test]
    fn resolve_restart_first_written_wins() {
        let instance = RestartRequest::Instance(always_capped());
        let flag = RestartRequest::Flag(always_capped());
        let root = RestartRequest::LauncherRoot(on_failure());
        let cases = [
            (
                instance,
                Some(never()),
                always_capped(),
                RestartOrigin::Instance,
            ),
            (flag, Some(never()), always_capped(), RestartOrigin::Flag),
            (root, Some(never()), never(), RestartOrigin::Node),
            (
                RestartRequest::Unset,
                Some(on_failure()),
                on_failure(),
                RestartOrigin::Node,
            ),
            (root, None, on_failure(), RestartOrigin::LauncherRoot),
            (
                RestartRequest::Unset,
                None,
                on_failure(),
                RestartOrigin::Default,
            ),
        ];
        for (request, manifest, declaration, origin) in cases {
            let resolved = resolve_restart(request, manifest);
            assert_eq!(
                resolved.declaration, declaration,
                "{request:?} / {manifest:?}"
            );
            assert_eq!(resolved.origin, origin, "{request:?} / {manifest:?}");
        }
    }

    #[test]
    fn resolve_restart_takes_a_level_whole() {
        // A cap at the launcher root never combines with the node's policy.
        let root = RestartRequest::LauncherRoot(always_capped());
        let resolved = resolve_restart(root, Some(on_failure()));
        assert_eq!(resolved.declaration, on_failure());
        assert_eq!(resolved.origin, RestartOrigin::Node);
    }

    /// A policy written on the instance resolves on its own; one the node or
    /// the launcher root decides needs the node's `peppy.json5`.
    #[test]
    fn a_policy_written_on_the_instance_resolves_without_the_manifest() {
        let on_instance =
            instance_restart(RestartRequest::Instance(on_failure())).expect("an entry's policy");
        assert_eq!(
            (on_instance.declaration, on_instance.origin),
            (on_failure(), RestartOrigin::Instance)
        );
        let on_flag =
            instance_restart(RestartRequest::Flag(on_failure())).expect("a flag's policy");
        assert_eq!(on_flag.origin, RestartOrigin::Flag);
        assert_eq!(
            instance_restart(RestartRequest::LauncherRoot(on_failure())),
            None
        );
        assert_eq!(instance_restart(RestartRequest::Unset), None);
    }
}
