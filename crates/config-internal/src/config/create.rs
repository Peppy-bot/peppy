use crate::config::types::ConfigTemplateType;
use crate::error::Error;
use crate::error::Result;
use askama::Template;

/// Template for root node configuration
#[derive(Template)]
#[template(path = "root_node.yaml.j2")]
struct RootNodeTemplate<'a> {
    name: &'a str,
}

/// Template for simple node configuration
#[derive(Template)]
#[template(path = "peppy_new_node_simple.yaml.j2")]
struct SimpleNodeTemplate<'a> {
    name: &'a str,
    namespace: &'a str,
}

/// Template for full node configuration
#[derive(Template)]
#[template(path = "peppy_new_node_full.yaml.j2")]
struct FullNodeTemplate<'a> {
    name: &'a str,
    namespace: &'a str,
}

pub struct NodeConfigTemplate;

impl NodeConfigTemplate {
    /// Renders the template to a string
    // TODO use newtype pattern for `name`
    pub fn render(
        template_type: &ConfigTemplateType,
        node_name: Option<&str>,
        node_namespace: Option<&str>,
    ) -> Result<String> {
        match template_type {
            ConfigTemplateType::RootNode => {
                let template = RootNodeTemplate {
                    name: node_name.unwrap(),
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::SimpleNode => {
                let template = SimpleNodeTemplate {
                    name: node_name.unwrap(),
                    namespace: node_namespace.unwrap(),
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::FullNode => {
                let template = FullNodeTemplate {
                    name: node_name.unwrap(),
                    namespace: node_namespace.unwrap(),
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
        }
    }
}
