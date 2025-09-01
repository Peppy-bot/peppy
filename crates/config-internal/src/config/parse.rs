use crate::config::NodeConfig;
use crate::error::{Error, Result};
use saphyr::{LoadableYamlNode, Yaml};
use std::fs;
use std::path::PathBuf;

pub fn parse_yaml_config(config_file: PathBuf) -> Result<NodeConfig> {
    let content = fs::read_to_string(&config_file)?;

    let docs = Yaml::load_from_str(&content)
        .map_err(|e| Error::ConfigParse(format!("Failed to parse YAML: {}", e)))?;

    if docs.is_empty() {
        return Err(Error::ConfigParse("Empty YAML document".to_string()));
    }

    let doc = &docs[0];

    // Parse the YAML into our NodeConfig structure
    let node_config = parse_node_info(doc)?;
    let node_parameters = parse_node_parameters(doc)?;
    let exposes = parse_exposes(doc)?;
    let resources = parse_resources(doc)?;
    let logging = parse_logging(doc)?;

    Ok(NodeConfig {
        node_config,
        node_parameters,
        exposes,
        resources,
        logging,
    })
}

fn parse_node_info(doc: &Yaml) -> Result<crate::config::NodeInfo> {
    let node_config = &doc["node_config"];

    if node_config.is_badvalue() {
        return Err(Error::ConfigParse(
            "node_config section not found".to_string(),
        ));
    }

    Ok(crate::config::NodeInfo {
        name: node_config["name"]
            .as_str()
            .ok_or_else(|| Error::ConfigParse("name must be a string".to_string()))?
            .to_string(),
        namespace: node_config["namespace"]
            .as_str()
            .ok_or_else(|| Error::ConfigParse("namespace must be a string".to_string()))?
            .to_string(),
        version: node_config["version"]
            .as_str()
            .ok_or_else(|| Error::ConfigParse("version must be a string".to_string()))?
            .to_string(),
        auto_start: node_config["auto_start"]
            .as_bool()
            .ok_or_else(|| Error::ConfigParse("auto_start must be a boolean".to_string()))?,
        respawn: node_config["respawn"]
            .as_bool()
            .ok_or_else(|| Error::ConfigParse("respawn must be a boolean".to_string()))?,
        respawn_delay: node_config["respawn_delay"]
            .as_floating_point()
            .ok_or_else(|| Error::ConfigParse("respawn_delay must be a number".to_string()))?,
    })
}

fn parse_node_parameters(_doc: &Yaml) -> Result<crate::config::NodeParameters> {
    // For now, return default parameters
    // In the future, this can be extended to parse custom parameters
    Ok(crate::config::NodeParameters::default())
}

fn parse_exposes(doc: &Yaml) -> Result<crate::config::Exposes> {
    let exposes = &doc["exposes"];

    if exposes.is_badvalue() {
        return Err(Error::ConfigParse("exposes section not found".to_string()));
    }

    Ok(crate::config::Exposes {
        topics: parse_string_array(&exposes["topics"])?,
        services: parse_string_array(&exposes["services"])?,
        actions: parse_string_array(&exposes["actions"])?,
    })
}

fn parse_resources(doc: &Yaml) -> Result<crate::config::Resources> {
    let resources = &doc["resources"];

    if resources.is_badvalue() {
        return Err(Error::ConfigParse(
            "resources section not found".to_string(),
        ));
    }

    Ok(crate::config::Resources {
        max_memory_mb: resources["max_memory_mb"]
            .as_integer()
            .ok_or_else(|| Error::ConfigParse("max_memory_mb must be an integer".to_string()))?
            as u32,
        cpu_affinity: parse_u32_array(&resources["cpu_affinity"])?,
    })
}

fn parse_logging(doc: &Yaml) -> Result<crate::config::Logging> {
    let logging = &doc["logging"];

    if logging.is_badvalue() {
        return Err(Error::ConfigParse("logging section not found".to_string()));
    }

    Ok(crate::config::Logging {
        min_level: logging["min_level"]
            .as_str()
            .ok_or_else(|| Error::ConfigParse("min_level must be a string".to_string()))?
            .to_string(),
        file_path: logging["file_path"]
            .as_str()
            .ok_or_else(|| Error::ConfigParse("file_path must be a string".to_string()))?
            .to_string(),
        max_file_size_mb: logging["max_file_size_mb"]
            .as_integer()
            .ok_or_else(|| Error::ConfigParse("max_file_size_mb must be an integer".to_string()))?
            as u32,
        format: logging["format"]
            .as_str()
            .ok_or_else(|| Error::ConfigParse("format must be a string".to_string()))?
            .to_string(),
    })
}

fn parse_string_array(yaml: &Yaml) -> Result<Vec<String>> {
    if let Some(vec) = yaml.as_vec() {
        vec.iter()
            .map(|item| {
                item.as_str()
                    .ok_or_else(|| Error::ConfigParse("Array elements must be strings".to_string()))
                    .map(|s| s.to_string())
            })
            .collect()
    } else {
        Ok(Vec::new())
    }
}

fn parse_u32_array(yaml: &Yaml) -> Result<Vec<u32>> {
    if let Some(vec) = yaml.as_vec() {
        vec.iter()
            .map(|item| {
                item.as_integer()
                    .ok_or_else(|| {
                        Error::ConfigParse("Array elements must be integers".to_string())
                    })
                    .map(|i| i as u32)
            })
            .collect()
    } else {
        Ok(Vec::new())
    }
}
