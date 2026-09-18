//! The codec every wire message that carries instance endpoints shares.
//!
//! `node.capnp` and `launch.capnp` each define their own endpoint struct and
//! kind enum, since every schema in the directory is self-contained, so the
//! codec is generated once per schema from the same body.

use config::node::EndpointKind;

use crate::Result;
use crate::encoding::{capnp_list_len, read_text_list, required_text, write_text_list};
use crate::graph::InstanceEndpoint;

macro_rules! endpoint_list_codec {
    ($write:ident, $read:ident, $capnp:ident, $endpoint:ident, $kind:ident) => {
        /// Writes `endpoints` into a struct list already sized for them.
        pub(crate) fn $write(
            mut list: ::capnp::struct_list::Builder<'_, crate::$capnp::$endpoint::Owned>,
            endpoints: &[InstanceEndpoint],
        ) -> Result<()> {
            assert_eq!(
                list.len() as usize,
                endpoints.len(),
                "an endpoint list is sized for exactly the endpoints written into it"
            );
            for (index, endpoint) in endpoints.iter().enumerate() {
                let mut wire = list.reborrow().get(index as u32);
                wire.set_label(&endpoint.label);
                wire.set_kind(match endpoint.kind {
                    EndpointKind::Page => crate::$capnp::$kind::Page,
                    EndpointKind::Mcp => crate::$capnp::$kind::Mcp,
                });
                let url_count = capnp_list_len(endpoint.urls.len(), "InstanceEndpoint.urls")?;
                write_text_list(wire.init_urls(url_count), &endpoint.urls);
            }
            Ok(())
        }

        /// Inverse of the writer: a label is required, a kind outside the
        /// schema is refused.
        pub(crate) fn $read(
            list: ::capnp::struct_list::Reader<'_, crate::$capnp::$endpoint::Owned>,
        ) -> Result<Vec<InstanceEndpoint>> {
            let mut endpoints = Vec::with_capacity(list.len() as usize);
            for index in 0..list.len() {
                let wire = list.get(index);
                let label = required_text(wire.get_label()?.to_str()?, "InstanceEndpoint.label")?;
                let kind = match wire.get_kind() {
                    Ok(crate::$capnp::$kind::Page) => EndpointKind::Page,
                    Ok(crate::$capnp::$kind::Mcp) => EndpointKind::Mcp,
                    Err(err) => {
                        return Err(crate::Error::Decoding(format!(
                            "InstanceEndpoint.kind for `{label}`: {err}"
                        )));
                    }
                };
                endpoints.push(InstanceEndpoint {
                    label,
                    kind,
                    urls: read_text_list(wire.get_urls()?)?,
                });
            }
            Ok(endpoints)
        }
    };
}

endpoint_list_codec!(
    write_node_endpoints,
    read_node_endpoints,
    node_capnp,
    instance_endpoint,
    EndpointKind
);

endpoint_list_codec!(
    write_launch_endpoints,
    read_launch_endpoints,
    launch_capnp,
    launch_endpoint,
    EndpointKind
);

/// Sample endpoints for the codec tests of the messages that carry them:
/// both kinds, one label with several URLs.
#[cfg(test)]
pub(crate) fn sample_endpoints() -> Vec<InstanceEndpoint> {
    vec![
        InstanceEndpoint {
            label: "camera_v1".to_string(),
            kind: EndpointKind::Mcp,
            urls: vec!["http://127.0.0.1:8900/camera/v1/mcp".to_string()],
        },
        InstanceEndpoint {
            label: "panel".to_string(),
            kind: EndpointKind::Page,
            urls: vec![
                "http://127.0.0.1:8765".to_string(),
                "http://192.168.1.5:8765".to_string(),
            ],
        },
    ]
}
