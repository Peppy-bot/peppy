//! Cap'n Proto encoding utilities for node messages.
pub mod add;
pub mod builder;
pub mod documents;
pub mod info;
pub mod init;
pub mod remove;
pub mod run;
pub mod stop;
pub mod sync;

use crate::Result;
use crate::node_capnp;

/// Writes `(key, value)` pairs into a `List(EnvVar)` builder that the caller
/// sized, because only the caller can name its goal's field in the error.
fn write_env_vars(
    mut list: capnp::struct_list::Builder<'_, node_capnp::env_var::Owned>,
    env_vars: &[(String, String)],
) {
    for (idx, (key, value)) in env_vars.iter().enumerate() {
        let mut env_var = list.reborrow().get(idx as u32);
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
