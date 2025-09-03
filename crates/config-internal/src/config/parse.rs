use crate::{
    config::types::NodeConfig,
    error::{Error, Result},
};
use saphyr::{LoadableYamlNode, Yaml};

/// Parser responsible for extracting configuration sections from YAML documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    /// Takes a yaml content as parameter
    pub fn from_content(content: &str) -> Result<String> {
        let docs: Vec<Yaml<'_>> = Yaml::load_from_str(&content)
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

        // Serialize the populated config back to YAML
        // FIXME: Replace deprecated serde_yaml when a good alternative pops up, like `saphyr-serde`
        let yaml = serde_yaml::to_string(&config)
            .map_err(|e| Error::ConfigParse(format!("Failed to serialize YAML: {}", e)))?;
        Ok(yaml)
    }

    fn parse_node_config_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(node_config) = doc.as_mapping_get("node_config") {
            // Required/optional string fields
            if let Some(n) = Self::get_str(node_config, "name")? {
                config.node_config.name = crate::config::types::Name::new(n.to_string())?;
            }

            if let Some(ns) = Self::get_str(node_config, "namespace")? {
                config.node_config.namespace = crate::config::types::Namespace::new(ns.to_string())?;
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
            if let Some(topics) = exposes.as_mapping_get("topics") {
                if let Some(vec) = topics.as_vec() {
                    let mut out = Vec::with_capacity(vec.len());
                    for (i, item) in vec.iter().enumerate() {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_string());
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected string for key 'topics[{}]'",
                                i
                            )));
                        }
                    }
                    config.exposes.topics = out;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'topics'".to_string(),
                    ));
                }
            }

            if let Some(services) = exposes.as_mapping_get("services") {
                if let Some(vec) = services.as_vec() {
                    let mut out = Vec::with_capacity(vec.len());
                    for (i, item) in vec.iter().enumerate() {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_string());
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected string for key 'services[{}]'",
                                i
                            )));
                        }
                    }
                    config.exposes.services = out;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'services'".to_string(),
                    ));
                }
            }

            if let Some(actions) = exposes.as_mapping_get("actions") {
                if let Some(vec) = actions.as_vec() {
                    let mut out = Vec::with_capacity(vec.len());
                    for (i, item) in vec.iter().enumerate() {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_string());
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected string for key 'actions[{}]'",
                                i
                            )));
                        }
                    }
                    config.exposes.actions = out;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'actions'".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn parse_resources_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(resources) = doc.as_mapping_get("resources") {
            if let Some(v) = Self::get_u32(resources, "max_memory_mb")? {
                config.resources.max_memory_mb = v;
            }

            if let Some(cpu) = resources.as_mapping_get("cpu_affinity") {
                if let Some(cpu_vec) = cpu.as_vec() {
                    let mut parsed = Vec::with_capacity(cpu_vec.len());
                    for (i, item) in cpu_vec.iter().enumerate() {
                        if let Some(v) = Self::yaml_to_u32(item) {
                            parsed.push(v);
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected number for key 'cpu_affinity[{}]'",
                                i
                            )));
                        }
                    }
                    config.resources.cpu_affinity = parsed;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'cpu_affinity'".to_string(),
                    ));
                }
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
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(s) = value.as_str() {
                return Ok(Some(s));
            } else {
                return Err(Error::ConfigParse(format!(
                    "Expected string for key '{}'",
                    key
                )));
            }
        }
        Ok(None)
    }

    fn get_u32(map: &Yaml, key: &str) -> Result<Option<u32>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(v) = Self::yaml_to_u32(value) {
                return Ok(Some(v));
            } else {
                return Err(Error::ConfigParse(format!(
                    "Expected number for key '{}'",
                    key
                )));
            }
        }
        Ok(None)
    }

    fn get_bool(map: &Yaml, key: &str) -> Result<Option<bool>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(b) = value.as_bool() {
                return Ok(Some(b));
            } else {
                return Err(Error::ConfigParse(format!(
                    "Expected boolean for key '{}'",
                    key
                )));
            }
        }
        Ok(None)
    }

    fn get_f64(map: &Yaml, key: &str) -> Result<Option<f64>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(v) = value.as_floating_point() {
                return Ok(Some(v));
            }
            if let Some(s) = value.as_str() {
                if let Ok(v) = s.parse::<f64>() {
                    return Ok(Some(v));
                }
            }
            return Err(Error::ConfigParse(format!(
                "Expected number for key '{}'",
                key
            )));
        }
        Ok(None)
    }

    fn yaml_to_u32(node: &Yaml) -> Option<u32> {
        // Accept both float-like and integer-like values
        if let Some(v) = node.as_floating_point() {
            return Some(v.max(0.0) as u32);
        }
        if let Some(s) = node.as_str() {
            if let Ok(v) = s.parse::<f64>() {
                return Some(v.max(0.0) as u32);
            }
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
}
