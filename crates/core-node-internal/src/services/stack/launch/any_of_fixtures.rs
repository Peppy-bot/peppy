//! The brain of the tests of an `any_of` of launch variables: it takes the
//! two variables of an OpenAI-compatible API, else the key of the provider.

use config::runtime::Name;
use daemon_config::launcher::{DeploymentInstance, ForwardedEnv, LaunchVariable, LaunchVariables};
use std::collections::BTreeSet;

/// The base address of the OpenAI-compatible API, in the first set.
pub(super) const BASE: &str = "OPENAI_COMPATIBLE_API_BASE";
/// The key of the OpenAI-compatible API, in the first set.
pub(super) const API_KEY: &str = "OPENAI_COMPATIBLE_API_KEY";
/// The key of the provider, the second set.
pub(super) const PROVIDER_KEY: &str = "GEMINI_API_KEY";

fn variable(name: &str) -> LaunchVariable {
    LaunchVariable::try_from(name.to_owned()).expect("a launch variable")
}

/// The brain `id`, whose `any_of` takes the two variables of the API, else
/// the key of the provider.
pub(super) fn brain_with_any_of(id: &str) -> DeploymentInstance {
    let mut brain = DeploymentInstance::empty(Name::new(id).expect("an instance id"));
    brain.env_from_launch = LaunchVariables::any_of(vec![
        vec![variable(BASE), variable(API_KEY)],
        vec![variable(PROVIDER_KEY)],
    ])
    .expect("an any_of");
    brain
}

/// `entries` as the forwarded environment of the launcher of that brain,
/// whose launch variables are the names of both sets.
pub(super) fn forwarded_to_the_brain(entries: &[(&str, &str)]) -> ForwardedEnv {
    ForwardedEnv::parse(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        &BTreeSet::from([variable(BASE), variable(API_KEY), variable(PROVIDER_KEY)]),
    )
}
