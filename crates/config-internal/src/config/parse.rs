use super::types::{Name, Namespace, NodeConfig};
use crate::error::{Error, Result};
use saphyr::{LoadableYamlNode, Yaml};

/// Parser responsible for extracting configuration sections from YAML documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    /// Takes a yaml content as parameter
    pub fn from_content(content: &str) -> Result<NodeConfig> {
        let docs: Vec<Yaml<'_>> = Yaml::load_from_str(content)
            .map_err(|e| Error::ConfigParse(format!("Failed to parse YAML: {}", e)))?;

        if docs.is_empty() {
            return Err(Error::ConfigParse("Empty YAML document".to_string()));
        }

        let mut config = NodeConfig::default();
        // Parse sections into builder.config
        let doc = &docs[0];
        NodeConfigParser::parse_node_config_section(doc, &mut config)?;
        NodeConfigParser::parse_exposes_section(doc, &mut config)?;
        NodeConfigParser::parse_resources_section(doc, &mut config)?;
        NodeConfigParser::parse_logging_section(doc, &mut config)?;

        Ok(config)
    }

    fn parse_node_config_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(node_config) = doc.as_mapping_get("node_config") {
            // Required/optional string fields
            if let Some(n) = Self::get_str(node_config, "name")? {
                config.node_config.name = Name::new(n.to_string())?;
            }

            if let Some(ns) = Self::get_str(node_config, "namespace")? {
                config.node_config.namespace = Namespace::new(ns.to_string())?;
            }

            // Optional fields
            if let Some(v) = Self::get_str(node_config, "version")? {
                config.node_config.version = v.to_string();
            }

            if let Some(v) = Self::get_bool(node_config, "auto_start")? {
                config.node_config.auto_start = v;
            }

            if let Some(v) = Self::get_bool(node_config, "respawn")? {
                config.node_config.respawn = v;
            }

            if let Some(v) = Self::get_f64(node_config, "respawn_delay")? {
                config.node_config.respawn_delay = v;
            }
        }
        Ok(())
    }

    fn parse_exposes_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(exposes) = doc.as_mapping_get("exposes") {
            if let Some(v) = Self::get_vec_str(exposes, "topics")? {
                config.exposes.topics = v;
            }

            if let Some(v) = Self::get_vec_str(exposes, "services")? {
                config.exposes.services = v;
            }

            if let Some(v) = Self::get_vec_str(exposes, "actions")? {
                config.exposes.actions = v;
            }
        }
        Ok(())
    }

    fn parse_resources_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(resources) = doc.as_mapping_get("resources") {
            if let Some(v) = Self::get_u32(resources, "max_memory_mb")? {
                config.resources.max_memory_mb = v;
            }

            if let Some(v) = Self::get_vec_u32(resources, "cpu_affinity")? {
                config.resources.cpu_affinity = v;
            }
        }
        Ok(())
    }

    fn parse_logging_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(logging) = doc.as_mapping_get("logging") {
            if let Some(v) = Self::get_str(logging, "min_level")? {
                config.logging.min_level = v.to_string();
            }

            if let Some(v) = Self::get_str(logging, "file_path")? {
                config.logging.file_path = v.to_string();
            }

            if let Some(v) = Self::get_u32(logging, "max_file_size_mb")? {
                config.logging.max_file_size_mb = v;
            }

            if let Some(v) = Self::get_str(logging, "format")? {
                config.logging.format = v.to_string();
            }
        }
        Ok(())
    }

    fn get_str<'a>(map: &'a Yaml, key: &str) -> Result<Option<&'a str>> {
        Self::get_scalar(map, key, |v| v.as_str(), "string")
    }

    fn get_u32(map: &Yaml, key: &str) -> Result<Option<u32>> {
        Self::get_scalar(map, key, Self::yaml_to_u32, "number")
    }

    fn get_bool(map: &Yaml, key: &str) -> Result<Option<bool>> {
        Self::get_scalar(map, key, |v| v.as_bool(), "boolean")
    }

    fn get_f64(map: &Yaml, key: &str) -> Result<Option<f64>> {
        Self::get_scalar(map, key, Self::yaml_to_f64, "number")
    }

    fn get_scalar<'a, T>(
        map: &'a Yaml,
        key: &str,
        parse: impl Fn(&'a Yaml) -> Option<T>,
        ty: &str,
    ) -> Result<Option<T>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(v) = parse(value) {
                Ok(Some(v))
            } else {
                Err(Error::ConfigParse(format!(
                    "Expected {} for key '{}'",
                    ty, key
                )))
            }
        } else {
            Ok(None)
        }
    }

    fn yaml_to_u32(node: &Yaml) -> Option<u32> {
        // Accept both float-like and integer-like values
        if let Some(v) = node.as_floating_point() {
            return Some(v.max(0.0) as u32);
        }
        if let Some(s) = node.as_str()
            && let Ok(v) = s.parse::<f64>()
        {
            return Some(v.max(0.0) as u32);
        }
        // Last-resort: parse from debug representation (handles Integer(100), Real(2.0))
        let dbg = format!("{:?}", node);
        let num: String = dbg
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if num.is_empty() {
            return None;
        }
        num.parse::<f64>().ok().map(|v| v.max(0.0) as u32)
    }

    fn yaml_to_f64(node: &Yaml) -> Option<f64> {
        if let Some(v) = node.as_floating_point() {
            return Some(v);
        }
        if let Some(s) = node.as_str()
            && let Ok(v) = s.parse::<f64>()
        {
            return Some(v);
        }
        None
    }

    fn get_vec_str(map: &Yaml, key: &str) -> Result<Option<Vec<String>>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(vec) = value.as_vec() {
                let mut out = Vec::with_capacity(vec.len());
                for (i, item) in vec.iter().enumerate() {
                    if let Some(s) = item.as_str() {
                        out.push(s.to_string());
                    } else {
                        return Err(Error::ConfigParse(format!(
                            "Expected string for key '{}[{}]'",
                            key, i
                        )));
                    }
                }
                Ok(Some(out))
            } else {
                Err(Error::ConfigParse(format!(
                    "Expected array for key '{}'",
                    key
                )))
            }
        } else {
            Ok(None)
        }
    }

    fn get_vec_u32(map: &Yaml, key: &str) -> Result<Option<Vec<u32>>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(vec) = value.as_vec() {
                let mut out = Vec::with_capacity(vec.len());
                for (i, item) in vec.iter().enumerate() {
                    if let Some(v) = Self::yaml_to_u32(item) {
                        out.push(v);
                    } else {
                        return Err(Error::ConfigParse(format!(
                            "Expected number for key '{}[{}]'",
                            key, i
                        )));
                    }
                }
                Ok(Some(out))
            } else {
                Err(Error::ConfigParse(format!(
                    "Expected array for key '{}'",
                    key
                )))
            }
        } else {
            Ok(None)
        }
    }
}
