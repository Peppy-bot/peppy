use std::collections::HashMap;

use config::{ConfigResult, encoding::map_message_format_to_capnpn_proto, node::MessageFormat};

pub fn generate_capnp_files_from_messages(
    messages_format: &[MessageFormat],
) -> ConfigResult<(Vec<String>, Vec<HashMap<String, String>>)> {
    let mut schemas = Vec::with_capacity(messages_format.len());
    let mut type_mappings = Vec::with_capacity(messages_format.len());

    for format in messages_format.iter().cloned() {
        let (schema, mapping) = map_message_format_to_capnpn_proto(format)?;
        schemas.push(schema);
        type_mappings.push(mapping);
    }

    Ok((schemas, type_mappings))
}
