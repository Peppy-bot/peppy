mod create;

pub use create::{create_peppy_node_config, init_root_node};

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use starlark::environment::{Globals, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{Heap, Value};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub namespace: String,
    pub version: String,
    pub auto_start: bool,
    pub respawn: bool,
    pub respawn_delay: f64,
    pub publishes: Vec<String>,
    pub subscribes: Vec<String>,
    pub services: Vec<String>,
    pub actions: Vec<String>,
    pub depends_on: Vec<String>,
    pub parameters: Parameters,
    pub qos_profile: String,
    pub resources: Resources,
    pub logging: Logging,
    pub init_script: String,
    pub diagnostics: Diagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameters {
    // Add runtime parameters as needed
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resources {
    pub max_memory_mb: u32,
    pub cpu_affinity: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logging {
    pub level: String,
    pub to_file: bool,
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostics {
    pub enabled: bool,
    pub publish_rate_hz: f64,
}

pub fn parse_starlark_config(config_file: PathBuf) -> Result<NodeConfig> {
    let heap = Heap::new();
    let content = fs::read_to_string(&config_file)?;

    let ast = AstModule::parse(&config_file.to_string_lossy(), content, &Dialect::Extended)?;

    let globals = Globals::extended_internal();
    let module = Module::new();
    let mut evaluator = Evaluator::new(&module);

    evaluator.eval_module(ast, &globals)?;

    let root_node = module.get("root_node").ok_or(Error::ConfigParse(
        "root_node not found in configuration".to_string(),
    ))?;

    // Extract values from the Starlark struct
    let namespace = root_node
        .get_attr("namespace", &heap)?
        .ok_or(Error::ConfigParse(
            "namespace attribute not found".to_string(),
        ))?
        .unpack_str()
        .ok_or(Error::ConfigParse("namespace must be a string".to_string()))?
        .to_string();

    let version = root_node
        .get_attr("version", &heap)?
        .ok_or(Error::ConfigParse(
            "version attribute not found".to_string(),
        ))?
        .unpack_str()
        .ok_or(Error::ConfigParse("version must be a string".to_string()))?
        .to_string();

    let auto_start = root_node
        .get_attr("auto_start", &heap)?
        .ok_or(Error::ConfigParse(
            "auto_start attribute not found".to_string(),
        ))?
        .unpack_bool()
        .ok_or(Error::ConfigParse(
            "auto_start must be a boolean".to_string(),
        ))?;

    let respawn = root_node
        .get_attr("respawn", &heap)?
        .ok_or(Error::ConfigParse(
            "respawn attribute not found".to_string(),
        ))?
        .unpack_bool()
        .ok_or(Error::ConfigParse("respawn must be a boolean".to_string()))?;

    let respawn_delay_val =
        root_node
            .get_attr("respawn_delay", &heap)?
            .ok_or(Error::ConfigParse(
                "respawn_delay attribute not found".to_string(),
            ))?;
    let respawn_delay = respawn_delay_val.unpack_i32().ok_or(Error::ConfigParse(
        "respawn_delay must be a number".to_string(),
    ))? as f64;

    let qos_profile = root_node
        .get_attr("qos_profile", &heap)?
        .ok_or(Error::ConfigParse(
            "qos_profile attribute not found".to_string(),
        ))?
        .unpack_str()
        .ok_or(Error::ConfigParse(
            "qos_profile must be a string".to_string(),
        ))?
        .to_string();

    let init_script = root_node
        .get_attr("init_script", &heap)?
        .ok_or(Error::ConfigParse(
            "init_script attribute not found".to_string(),
        ))?
        .unpack_str()
        .ok_or(Error::ConfigParse(
            "init_script must be a string".to_string(),
        ))?
        .to_string();

    // Extract nested structs
    let resources_value = root_node
        .get_attr("resources", &heap)?
        .ok_or(Error::ConfigParse(
            "resources attribute not found".to_string(),
        ))?;
    let resources = Resources {
        max_memory_mb: resources_value
            .get_attr("max_memory_mb", &heap)?
            .ok_or(Error::ConfigParse(
                "max_memory_mb attribute not found".to_string(),
            ))?
            .unpack_i32()
            .ok_or(Error::ConfigParse(
                "max_memory_mb must be an integer".to_string(),
            ))? as u32,
        cpu_affinity: extract_int_list(
            resources_value
                .get_attr("cpu_affinity", &heap)?
                .ok_or(Error::ConfigParse(
                    "cpu_affinity attribute not found".to_string(),
                ))?,
            &heap,
        )?,
    };

    let logging_value = root_node
        .get_attr("logging", &heap)?
        .ok_or(Error::ConfigParse(
            "logging attribute not found".to_string(),
        ))?;
    let logging = Logging {
        level: logging_value
            .get_attr("level", &heap)?
            .ok_or(Error::ConfigParse("level attribute not found".to_string()))?
            .unpack_str()
            .ok_or(Error::ConfigParse("level must be a string".to_string()))?
            .to_string(),
        to_file: logging_value
            .get_attr("to_file", &heap)?
            .ok_or(Error::ConfigParse(
                "to_file attribute not found".to_string(),
            ))?
            .unpack_bool()
            .ok_or(Error::ConfigParse("to_file must be a boolean".to_string()))?,
        file_path: logging_value
            .get_attr("file_path", &heap)?
            .ok_or(Error::ConfigParse(
                "file_path attribute not found".to_string(),
            ))?
            .unpack_str()
            .ok_or(Error::ConfigParse("file_path must be a string".to_string()))?
            .to_string(),
    };

    let diagnostics_value = root_node
        .get_attr("diagnostics", &heap)?
        .ok_or(Error::ConfigParse(
            "diagnostics attribute not found".to_string(),
        ))?;
    let diagnostics = Diagnostics {
        enabled: diagnostics_value
            .get_attr("enabled", &heap)?
            .ok_or(Error::ConfigParse(
                "enabled attribute not found".to_string(),
            ))?
            .unpack_bool()
            .ok_or(Error::ConfigParse("enabled must be a boolean".to_string()))?,
        publish_rate_hz: {
            let val = diagnostics_value
                .get_attr("publish_rate_hz", &heap)?
                .ok_or(Error::ConfigParse(
                    "publish_rate_hz attribute not found".to_string(),
                ))?;
            val.unpack_i32().ok_or(Error::ConfigParse(
                "publish_rate_hz must be a number".to_string(),
            ))? as f64
        },
    };

    // Extract lists
    let publishes = extract_string_list(
        root_node
            .get_attr("publishes", &heap)?
            .ok_or(Error::ConfigParse(
                "publishes attribute not found".to_string(),
            ))?,
        &heap,
    )?;
    let subscribes = extract_string_list(
        root_node
            .get_attr("subscribes", &heap)?
            .ok_or(Error::ConfigParse(
                "subscribes attribute not found".to_string(),
            ))?,
        &heap,
    )?;
    let services = extract_string_list(
        root_node
            .get_attr("services", &heap)?
            .ok_or(Error::ConfigParse(
                "services attribute not found".to_string(),
            ))?,
        &heap,
    )?;
    let actions = extract_string_list(
        root_node
            .get_attr("actions", &heap)?
            .ok_or(Error::ConfigParse(
                "actions attribute not found".to_string(),
            ))?,
        &heap,
    )?;
    let depends_on = extract_string_list(
        root_node
            .get_attr("depends_on", &heap)?
            .ok_or(Error::ConfigParse(
                "depends_on attribute not found".to_string(),
            ))?,
        &heap,
    )?;

    Ok(NodeConfig {
        namespace,
        version,
        auto_start,
        respawn,
        respawn_delay,
        publishes,
        subscribes,
        services,
        actions,
        depends_on,
        parameters: Parameters {},
        qos_profile,
        resources,
        logging,
        init_script,
        diagnostics,
    })
}

fn extract_string_list<'v>(value: Value<'v>, heap: &'v Heap) -> Result<Vec<String>> {
    let list = value.iterate(heap)?;
    let mut result = Vec::new();
    for item in list {
        if let Some(s) = item.unpack_str() {
            result.push(s.to_string());
        }
    }
    Ok(result)
}

fn extract_int_list<'v>(value: Value<'v>, heap: &'v Heap) -> Result<Vec<u32>> {
    let list = value.iterate(heap)?;
    let mut result = Vec::new();
    for item in list {
        if let Some(i) = item.unpack_i32() {
            result.push(i as u32);
        }
    }
    Ok(result)
}
