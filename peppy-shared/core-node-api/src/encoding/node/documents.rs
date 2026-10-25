//! Cap'n Proto encoding of the `instance_documents` service: the pinned
//! documents of one instance of the stack.

use capnp::message::Builder;

use crate::encoding::{capnp_list_len, decode_message, encode_message, required_text};
use crate::node_capnp;
use crate::{Error, Payload, Result};

/// Request payload for the `instance_documents` service: one instance of
/// the stack of the daemon that answers, by its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceDocumentsRequest {
    pub instance_id: String,
}

impl InstanceDocumentsRequest {
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<node_capnp::instance_documents_request::Builder>();
            request.set_instance_id(&self.instance_id);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::instance_documents_request::Reader>()?;
        Ok(Self {
            instance_id: required_text(request.get_instance_id()?.to_str()?, "instanceId")?,
        })
    }
}

/// One document, by the identity it is pinned under: a name, a tag and the
/// SHA-256 of its content. The fingerprint is computed from the content, so
/// the two always agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedDocument {
    name: String,
    tag: String,
    sha256: String,
    content: String,
}

impl PinnedDocument {
    pub fn new(
        name: impl Into<String>,
        tag: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        Self {
            name: name.into(),
            tag: tag.into(),
            sha256: config::fingerprint::fingerprint_for_bytes(content.as_bytes()),
            content,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The SHA-256 of [`Self::content`], in lowercase hex.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    fn write(&self, mut builder: node_capnp::pinned_document::Builder<'_>) {
        builder.set_name(&self.name);
        builder.set_tag(&self.tag);
        builder.set_sha256(&self.sha256);
        builder.set_content(&self.content);
    }

    /// Reads a document, refusing one whose fingerprint is not that of its
    /// content.
    fn read(reader: node_capnp::pinned_document::Reader<'_>) -> Result<Self> {
        let name = required_text(reader.get_name()?.to_str()?, "name")?;
        let tag = required_text(reader.get_tag()?.to_str()?, "tag")?;
        let document = Self::new(name, tag, reader.get_content()?.to_str()?);
        let sha256 = reader.get_sha256()?.to_str()?;
        if sha256 != document.sha256 {
            return Err(Error::Decoding(format!(
                "document `{}:{}` is pinned at sha256 `{sha256}`, but its content hashes to `{}`",
                document.name, document.tag, document.sha256
            )));
        }
        Ok(document)
    }
}

/// Response payload for the `instance_documents` service: the pinned
/// documents an instance's build was generated from. An instance that is
/// not in the stack, or a document the daemon can no longer read, is an
/// error reply instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceDocuments {
    /// The daemon that runs the instance.
    pub core_node: String,
    pub instance_id: String,
    /// The node manifest as the stack holds it, serialized as JSON5, under
    /// the node's name and tag.
    pub manifest: PinnedDocument,
    /// Each contract document the manifest names, in `manifest.implements`
    /// order, then `depends_on.contracts` order.
    pub contracts: Vec<PinnedDocument>,
    /// Each pairing document the manifest names, in `depends_on.pairings`
    /// order, then `depends_on.pairing_observers` order.
    pub pairings: Vec<PinnedDocument>,
}

impl InstanceDocuments {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<node_capnp::instance_documents_response::Builder>();
            response.set_core_node(&self.core_node);
            response.set_instance_id(&self.instance_id);
            self.manifest.write(response.reborrow().init_manifest());
            write_documents(
                response
                    .reborrow()
                    .init_contracts(capnp_list_len(self.contracts.len(), "contracts")?),
                &self.contracts,
            );
            write_documents(
                response.init_pairings(capnp_list_len(self.pairings.len(), "pairings")?),
                &self.pairings,
            );
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::instance_documents_response::Reader>()?;
        Ok(Self {
            core_node: required_text(response.get_core_node()?.to_str()?, "coreNode")?,
            instance_id: required_text(response.get_instance_id()?.to_str()?, "instanceId")?,
            manifest: PinnedDocument::read(response.get_manifest()?)?,
            contracts: read_documents(response.get_contracts()?)?,
            pairings: read_documents(response.get_pairings()?)?,
        })
    }
}

fn write_documents(
    mut list: capnp::struct_list::Builder<'_, node_capnp::pinned_document::Owned>,
    documents: &[PinnedDocument],
) {
    for (index, document) in documents.iter().enumerate() {
        document.write(list.reborrow().get(index as u32));
    }
}

fn read_documents(
    list: capnp::struct_list::Reader<'_, node_capnp::pinned_document::Owned>,
) -> Result<Vec<PinnedDocument>> {
    list.iter().map(PinnedDocument::read).collect()
}

impl crate::encoding::Wire for InstanceDocumentsRequest {
    type Root = crate::node_capnp::instance_documents_request::Owned;
}

impl crate::encoding::Wire for InstanceDocuments {
    type Root = crate::node_capnp::instance_documents_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> InstanceDocuments {
        InstanceDocuments {
            core_node: "robot-7".to_owned(),
            instance_id: "arm_inst".to_owned(),
            manifest: PinnedDocument::new("arm", "v1", "{ peppy_schema: \"node/v1\" }"),
            contracts: vec![
                PinnedDocument::new("limb_motion", "v1", "{ a: 1 }"),
                PinnedDocument::new("gripper", "v2", "{ b: 2 }"),
            ],
            pairings: vec![PinnedDocument::new("arm_link", "v1", "{ c: 3 }")],
        }
    }

    #[test]
    fn the_request_and_the_response_round_trip() {
        let request = InstanceDocumentsRequest::new("arm_inst");
        let payload = request.encode().unwrap();
        assert_eq!(
            InstanceDocumentsRequest::decode(payload.as_ref()).unwrap(),
            request
        );

        let response = documents();
        let payload = response.encode().unwrap();
        assert_eq!(
            InstanceDocuments::decode(payload.as_ref()).unwrap(),
            response
        );
    }

    #[test]
    fn a_document_carries_the_fingerprint_of_its_content() {
        let document = PinnedDocument::new("limb_motion", "v1", "{ a: 1 }");
        assert_eq!(
            document.sha256(),
            config::fingerprint::fingerprint_for_bytes(b"{ a: 1 }")
        );
    }

    /// A fingerprint that is not that of the content is refused at decode,
    /// so a reader never holds a document whose identity lies about it.
    #[test]
    fn a_document_whose_fingerprint_does_not_match_its_content_is_refused() {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<node_capnp::instance_documents_response::Builder>();
            response.set_core_node("robot-7");
            response.set_instance_id("arm_inst");
            let mut manifest = response.reborrow().init_manifest();
            manifest.set_name("arm");
            manifest.set_tag("v1");
            manifest.set_sha256("00");
            manifest.set_content("{}");
        }
        let payload = encode_message(&builder).unwrap();
        let error = InstanceDocuments::decode(payload.as_ref()).unwrap_err();
        assert!(
            error.to_string().contains("is pinned at sha256 `00`"),
            "{error}"
        );
    }

    #[test]
    fn a_request_with_no_instance_id_is_refused() {
        let payload = InstanceDocumentsRequest::new("").encode().unwrap();
        assert!(InstanceDocumentsRequest::decode(payload.as_ref()).is_err());
    }
}
