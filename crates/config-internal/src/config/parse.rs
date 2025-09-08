use super::types::{
    Action, Exposes, LogFormat, Logging, Name, Namespace, NodeConfig, QoSProfile, Resources,
    Service, SubscribesTo, Topic,
};
use crate::error::{ParsingError, Result};
use saphyr::{LoadableYamlNode, Yaml};
use std::fs;
use std::path::Path;

/// Parser responsible for extracting configuration sections from YAML documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<NodeConfig> {
        let path = file.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|_| ParsingError::CannotRead(path.display().to_string()))?;

        if content.trim().is_empty() {
            Err(ParsingError::EmptyContent(path.display().to_string()).into())
        } else {
            Self::from_content(&content)
        }
    }

    /// Takes a yaml content as parameter
    pub fn from_content(content: &str) -> Result<NodeConfig> {
        let docs: Vec<Yaml<'_>> = Yaml::load_from_str(content)
            .map_err(|e| ParsingError::CannotParseYaml(e.to_string()))?;

        // Strict schema validation against unknown keys
        Self::validate_known_schema(content)?;

        let mut config = NodeConfig::default();
        // Parse sections into builder.config
        let doc = &docs[0];
        NodeConfigParser::parse_node_config_section(doc, &mut config)?;
        NodeConfigParser::parse_exposes_section(doc, &mut config)?;
        NodeConfigParser::parse_subscribes_to_section(doc, &mut config)?;
        NodeConfigParser::parse_resources_section(doc, &mut config)?;
        NodeConfigParser::parse_logging_section(doc, &mut config)?;

        Ok(config)
    }

    /// Validates that only known keys (as defined by `NodeConfig` and nested
    /// structs) are present. Uses serde with `deny_unknown_fields` to enforce
    /// strictness, while allowing arbitrary content inside `node_parameters`.
    fn validate_known_schema(content: &str) -> Result<()> {
        match serde_yaml::from_str::<NodeConfig>(content) {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("unknown field") {
                    Err(ParsingError::UnknownKey("root".to_string(), msg).into())
                } else {
                    // Defer other errors to the main saphyr-based parsing
                    Ok(())
                }
            }
        }
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
                config.node_config.auto_start = Some(v);
            }

            if let Some(v) = Self::get_bool(node_config, "respawn")? {
                config.node_config.respawn = Some(v);
            }

            if let Some(v) = Self::get_f64(node_config, "respawn_delay")? {
                config.node_config.respawn_delay = Some(v);
            }
        }
        Ok(())
    }

    fn parse_exposes_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(exposes) = doc.as_mapping_get("exposes") {
            let mut exposes_section = Exposes::default();

            if let Some(v) = Self::get_topics(exposes, "topics")? {
                exposes_section.topics = Some(v);
            }

            if let Some(v) = Self::get_services(exposes, "services")? {
                exposes_section.services = Some(v);
            }

            if let Some(v) = Self::get_actions(exposes, "actions")? {
                exposes_section.actions = Some(v);
            }

            config.exposes = Some(exposes_section);
        }
        Ok(())
    }

    fn parse_subscribes_to_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(subscribes_to) = doc.as_mapping_get("subscribes_to") {
            let mut subscribes_to_section = SubscribesTo::default();

            if let Some(v) = Self::get_topics(subscribes_to, "topics")? {
                subscribes_to_section.topics = Some(v);
            }

            if let Some(v) = Self::get_services(subscribes_to, "services")? {
                subscribes_to_section.services = Some(v);
            }

            if let Some(v) = Self::get_actions(subscribes_to, "actions")? {
                subscribes_to_section.actions = Some(v);
            }

            config.subscribes_to = Some(subscribes_to_section);
        }
        Ok(())
    }

    fn parse_resources_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(resources) = doc.as_mapping_get("resources") {
            let mut resources_section = Resources::default();

            if let Some(v) = Self::get_u32(resources, "max_memory_mb")? {
                resources_section.max_memory_mb = Some(v);
            }

            config.resources = Some(resources_section);
        }
        Ok(())
    }

    fn parse_logging_section(doc: &Yaml, config: &mut NodeConfig) -> Result<()> {
        if let Some(logging) = doc.as_mapping_get("logging") {
            let mut logging_section = Logging::default();

            if let Some(v) = Self::get_str(logging, "min_level")? {
                logging_section.min_level = v.to_string();
            }

            if let Some(v) = Self::get_str(logging, "file_path")? {
                logging_section.file_path = Some(v.to_string());
            }

            if let Some(v) = Self::get_u32(logging, "max_file_size_mb")? {
                logging_section.max_file_size_mb = Some(v);
            }

            if let Some(v) = Self::get_str(logging, "format")? {
                logging_section.format = LogFormat::from(v.to_string());
            }

            config.logging = Some(logging_section);
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
                Err(ParsingError::InvalidScalar(ty.to_string(), key.to_string()))?
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

    fn get_topics(map: &Yaml, key: &str) -> Result<Option<Vec<Topic>>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(vec) = value.as_vec() {
                let mut out = Vec::with_capacity(vec.len());
                for item in vec.iter() {
                    let mut topic = Topic::default();

                    if let Some(t) = Self::get_str(item, "type")? {
                        topic.topic_type = t.to_string();
                    }

                    if let Some(n) = Self::get_str(item, "name")? {
                        topic.name = n.to_string();
                    }

                    if let Some(qos) = Self::get_str(item, "qos_profile")? {
                        topic.qos_profile = Self::parse_qos_profile(qos)?;
                    }

                    out.push(topic);
                }
                Ok(Some(out))
            } else {
                Err(ParsingError::BadArray(key.to_string()))?
            }
        } else {
            Ok(None)
        }
    }

    fn get_services(map: &Yaml, key: &str) -> Result<Option<Vec<Service>>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(vec) = value.as_vec() {
                let mut out = Vec::with_capacity(vec.len());
                for item in vec.iter() {
                    let mut service = Service::default();

                    if let Some(t) = Self::get_str(item, "type")? {
                        service.service_type = t.to_string();
                    }

                    if let Some(n) = Self::get_str(item, "name")? {
                        service.name = n.to_string();
                    }

                    if let Some(qos) = Self::get_str(item, "qos_profile")? {
                        service.qos_profile = Self::parse_qos_profile(qos)?;
                    }

                    out.push(service);
                }
                Ok(Some(out))
            } else {
                Err(ParsingError::BadArray(key.to_string()))?
            }
        } else {
            Ok(None)
        }
    }

    fn get_actions(map: &Yaml, key: &str) -> Result<Option<Vec<Action>>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(vec) = value.as_vec() {
                let mut out = Vec::with_capacity(vec.len());
                for item in vec.iter() {
                    let mut action = Action::default();

                    if let Some(t) = Self::get_str(item, "type")? {
                        action.action_type = t.to_string();
                    }

                    if let Some(n) = Self::get_str(item, "name")? {
                        action.name = n.to_string();
                    }

                    out.push(action);
                }
                Ok(Some(out))
            } else {
                Err(ParsingError::BadArray(key.to_string()))?
            }
        } else {
            Ok(None)
        }
    }

    fn parse_qos_profile(value: &str) -> Result<QoSProfile> {
        let qos: QoSProfile =
            serde_yaml::from_str(value).map_err(|_| ParsingError::InValidQoS(value.to_string()))?;
        Ok(qos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn test_parse_minimal_config() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        assert_eq!(config.node_config.name.as_str(), "test_node");
        assert_eq!(config.node_config.namespace.as_str(), "/test");
        assert_eq!(config.node_config.version, "0.1.0"); // default
    }

    #[test]
    fn test_parse_complex_config() {
        let yaml = r#"
node_config:
  name: camera_driver
  namespace: /sensors/camera
  version: "2.1.0"
  auto_start: true
  respawn: true
  respawn_delay: 2.0

exposes:
  topics:
    - type: "sensor_msgs/Image"
      name: "/camera/image_raw"
      qos_profile: "sensor_data"
    - type: "sensor_msgs/CameraInfo"
      name: "/camera/info"
      qos_profile: "standard"
  services:
    - type: "std_srvs/SetBool"
      name: "/camera/enable"
      qos_profile: "reliable"
  actions: []

subscribes_to:
  topics:
    - type: "std_msgs/String"
      name: "/camera/command"
      qos_profile: "reliable"

resources:
  max_memory_mb: 512

logging:
  min_level: "warn"
  file_path: "/var/log/camera.log"
  max_file_size_mb: 100
  format: "text"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();

        // Verify node config
        assert_eq!(config.node_config.name.as_str(), "camera_driver");
        assert_eq!(config.node_config.namespace.as_str(), "/sensors/camera");
        assert_eq!(config.node_config.version, "2.1.0");
        assert!(config.node_config.auto_start.unwrap());
        assert!(config.node_config.respawn.unwrap());
        assert_eq!(config.node_config.respawn_delay, Some(2.0));

        // Verify exposes
        let exposes = config.exposes.unwrap();
        assert_eq!(exposes.topics.as_ref().unwrap().len(), 2);
        assert_eq!(exposes.services.as_ref().unwrap().len(), 1);
        assert_eq!(exposes.actions.as_ref().unwrap().len(), 0);

        // Verify topics content
        let topics = exposes.topics.unwrap();
        assert_eq!(topics[0].topic_type, "sensor_msgs/Image");
        assert_eq!(topics[0].name, "/camera/image_raw");
        assert_eq!(topics[0].qos_profile, QoSProfile::SensorData);

        assert_eq!(topics[1].topic_type, "sensor_msgs/CameraInfo");
        assert_eq!(topics[1].name, "/camera/info");
        assert_eq!(topics[1].qos_profile, QoSProfile::Standard);

        // Verify services content
        let services = exposes.services.unwrap();
        assert_eq!(services[0].service_type, "std_srvs/SetBool");
        assert_eq!(services[0].name, "/camera/enable");
        assert_eq!(services[0].qos_profile, QoSProfile::Reliable);

        // Verify subscribes_to
        let subscribes_to = config.subscribes_to.unwrap();
        assert_eq!(subscribes_to.topics.as_ref().unwrap().len(), 1);

        // Verify subscribes_to content
        let topics = subscribes_to.topics.unwrap();
        assert_eq!(topics[0].topic_type, "std_msgs/String");
        assert_eq!(topics[0].name, "/camera/command");
        assert_eq!(topics[0].qos_profile, QoSProfile::Reliable);

        // Verify resources
        assert_eq!(config.resources.unwrap().max_memory_mb, Some(512));

        // Verify logging
        let logging = config.logging.unwrap();
        assert_eq!(logging.min_level, "warn");
        assert_eq!(logging.file_path, Some(String::from("/var/log/camera.log")));
        assert_eq!(logging.max_file_size_mb, Some(100));
        assert_eq!(logging.format, LogFormat::Text);
    }

    #[test]
    fn test_parse_full_node_config() {
        let yaml = r#"
node_config:
  name: my_node
  namespace: /robot
  version: "1.0.0"
  auto_start: true
  respawn: false
  respawn_delay: 5.5
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        assert_eq!(config.node_config.name.as_str(), "my_node");
        assert_eq!(config.node_config.namespace.as_str(), "/robot");
        assert_eq!(config.node_config.version, "1.0.0");
        assert_eq!(config.node_config.auto_start, Some(true));
        assert_eq!(config.node_config.respawn, Some(false));
        assert_eq!(config.node_config.respawn_delay, Some(5.5));
    }

    #[test]
    fn test_parse_exposes_topics() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  topics:
    - type: "sensor_msgs/Image"
      name: "/camera/image_raw"
      qos_profile: "sensor_data"
    - type: "geometry_msgs/Twist"
      name: "/cmd_vel"
      qos_profile: "reliable"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let exposes = config.exposes.unwrap();
        let topics = exposes.topics.unwrap();

        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].topic_type, "sensor_msgs/Image");
        assert_eq!(topics[0].name, "/camera/image_raw");
        assert!(matches!(topics[0].qos_profile, QoSProfile::SensorData));

        assert_eq!(topics[1].topic_type, "geometry_msgs/Twist");
        assert_eq!(topics[1].name, "/cmd_vel");
        assert!(matches!(topics[1].qos_profile, QoSProfile::Reliable));
    }

    #[test]
    fn test_parse_exposes_services() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  services:
    - type: "std_srvs/SetBool"
      name: "/enable_motor"
      qos_profile: "standard"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let exposes = config.exposes.unwrap();
        let services = exposes.services.unwrap();

        assert_eq!(services.len(), 1);
        assert_eq!(services[0].service_type, "std_srvs/SetBool");
        assert_eq!(services[0].name, "/enable_motor");
        assert!(matches!(services[0].qos_profile, QoSProfile::Standard));
    }

    #[test]
    fn test_parse_exposes_actions() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  actions:
    - type: "navigation/MoveToGoal"
      name: "/move_to_goal"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let exposes = config.exposes.unwrap();
        let actions = exposes.actions.unwrap();

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, "navigation/MoveToGoal");
        assert_eq!(actions[0].name, "/move_to_goal");
    }

    #[test]
    fn test_parse_subscribes_to() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
subscribes_to:
  topics:
    - type: "sensor_msgs/LaserScan"
      name: "/scan"
      qos_profile: "sensor_data"
  services:
    - type: "std_srvs/Trigger"
      name: "/reset"
      qos_profile: "reliable"
  actions:
    - type: "nav_msgs/FollowPath"
      name: "/follow_path"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let subscribes_to = config.subscribes_to.unwrap();

        let topics = subscribes_to.topics.unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].topic_type, "sensor_msgs/LaserScan");

        let services = subscribes_to.services.unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].service_type, "std_srvs/Trigger");

        let actions = subscribes_to.actions.unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, "nav_msgs/FollowPath");
    }

    #[test]
    fn test_parse_resources() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
resources:
  max_memory_mb: 2048
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let resources = config.resources.unwrap();
        assert_eq!(resources.max_memory_mb, Some(2048));
    }

    #[test]
    fn test_parse_logging() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
logging:
  min_level: "debug"
  file_path: "/var/log/test.log"
  max_file_size_mb: 50
  format: "json"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let logging = config.logging.unwrap();
        assert_eq!(logging.min_level, "debug");
        assert_eq!(logging.file_path, Some("/var/log/test.log".to_string()));
        assert_eq!(logging.max_file_size_mb, Some(50));
        assert_eq!(logging.format, LogFormat::Json);
    }

    #[test]
    fn test_empty_yaml_file() {
        // Create a temporary empty file to trigger EmptyContent from from_path
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let result = NodeConfigParser::from_path(tmp.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyContent(_))
        ));
    }

    #[test]
    fn test_invalid_name() {
        let yaml = r#"
node_config:
  name: "Invalid-Name!"
  namespace: /test
"#;
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::InvalidName(_))
        ));
    }

    #[test]
    fn test_invalid_namespace() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: "/Invalid Namespace"
"#;
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::InvalidNamespace(_))
        ));
    }

    #[test]
    fn test_invalid_qos_profile() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  topics:
    - type: "std_msgs/String"
      name: "/topic"
      qos_profile: "invalid_qos"
"#;
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::InValidQoS(_))
        ));
    }

    #[test]
    fn test_empty_exposes_lists() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  topics: []
  services: []
  actions: []
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let exposes = config.exposes.unwrap();

        assert_eq!(exposes.topics.as_ref().unwrap().len(), 0);
        assert_eq!(exposes.services.as_ref().unwrap().len(), 0);
        assert_eq!(exposes.actions.as_ref().unwrap().len(), 0);
    }

    #[test]
    fn test_partial_topic_definition() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  topics:
    - name: "/topic_without_type"
"#;
        let config = NodeConfigParser::from_content(yaml).unwrap();
        let exposes = config.exposes.unwrap();
        let topics = exposes.topics.unwrap();

        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].topic_type, ""); // default empty string
        assert_eq!(topics[0].name, "/topic_without_type");
        assert!(matches!(topics[0].qos_profile, QoSProfile::Standard)); // default
    }

    #[test]
    fn test_cannot_read_file() {
        let result = NodeConfigParser::from_path("/path/that/does/not/exist.yaml");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotRead(_))
        ));
    }

    #[test]
    fn test_cannot_parse_yaml() {
        let yaml = r#"node_config: [unclosed"#; // invalid YAML
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotParseYaml(_))
        ));
    }

    #[test]
    fn test_invalid_scalar_boolean() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
  auto_start: "true"  # string instead of boolean
"#;
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::InvalidScalar(t, k)) if t == "boolean" && k == "auto_start"
        ));
    }

    #[test]
    fn test_invalid_scalar_number() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
resources:
  max_memory_mb: "abc"  # string that cannot be parsed to number
"#;
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::InvalidScalar(t, k)) if t == "number" && k == "max_memory_mb"
        ));
    }

    #[test]
    fn test_bad_array_topics() {
        let yaml = r#"
node_config:
  name: test_node
  namespace: /test
exposes:
  topics: 123  # not an array
"#;
        let result = NodeConfigParser::from_content(yaml);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::BadArray(k)) if k == "topics"
        ));
    }
}
