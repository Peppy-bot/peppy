//! Cap'n Proto encoding utilities for node messages.
pub(super) mod add;
pub(super) mod builder;
pub(super) mod documents;
pub(super) mod info;
pub(super) mod init;
pub(super) mod publish;
pub(super) mod remove;
pub(super) mod run;
pub(super) mod stop;
pub(super) mod sync;

use crate::Result;
use crate::node_capnp;

/// Writes `(key, value)` pairs into a `List(EnvVar)` builder that the caller
/// sized, because only the caller can name its goal's field in the error.
fn write_env_vars(
    mut list: capnp::struct_list::Builder<'_, node_capnp::env_var::Owned>,
    env_vars: &[(String, String)],
) {
    for (idx, (key, value)) in (0_u32..).zip(env_vars.iter()) {
        let mut env_var = list.reborrow().get(idx);
        env_var.set_key(key);
        env_var.set_value(value);
    }
}

/// Inverse of [`write_env_vars`].
fn read_env_vars(
    list: capnp::struct_list::Reader<'_, node_capnp::env_var::Owned>,
) -> Result<Vec<(String, String)>> {
    let mut env_vars = Vec::with_capacity(list.len() as usize);
    for idx in 0..list.len() {
        let env_var = list.get(idx);
        env_vars.push((
            env_var.get_key()?.to_str()?.to_owned(),
            env_var.get_value()?.to_str()?.to_owned(),
        ));
    }
    Ok(env_vars)
}
