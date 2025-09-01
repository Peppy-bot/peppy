```yaml
# PeppyOS REST API Service Node Configuration
# This node provides a REST API interface for system interaction

# FIXME: `depends_on` is a repetition with `subscribes_to`
depends_on:
  # The peppy service running as a daemon will first try to access the node by polling its local registery and if no local node with `uvc_camera` is present, it will download it from the nodes.peppy.bot repository
  - name: "uvc_camera"
    # Will pull this particular version if specified, otherwise pulls the most recent one (always specify it to avoid breaking changes if pulled from nodes.peppy.bot, or leave blank if the node is local to this project)
    version: "0.1"
    namespace: "/" # In what ns is this node deployed?

node_config:
  name: "web_video_stream"
  namespace: "/"
  version: "0.1.0"
  respawn: true
  respawn_delay: 10.0

# The parameters of the current node
node_parameters:
  http:
    host: "localhost"
    port: 8081
    cors_enabled: true
    cors_origins: "*"
    max_connections: 100
    request_timeout_ms: 30000
  video_stream:
    format: "mjpeg"
    quality: 80
    max_fps: 30

subscribes_to:
  topics:
    - type: "sensor_message/image"
      name: "uvc_camera:/camera/video_feed"
      callback: "on_handle_video_feed"
#  actions:
#    - action_type: "custom_actions/ProcessVideo"
#      action_name: "/api/process_video"
#      callback: "handle_process_video_action"

# This node does not expose anything for other nodes. It exposes a web url (http://localhost:8081/video) to the end user but that is outside of the realm of node-to-node communication
exposes: []

resources:
  max_memory_mb: 512
  cpu_affinity: []
  max_threads: 10

logging:
  min_level: "info"
  file_path: "/var/log/peppy/rest_api.log"
  max_file_size_mb: 100
  format: "text"

diagnostics:
  enabled: true
  publish_rate_hz: 0.5
  health_checks:
    - "http_server_running"
    - "websocket_connections"
    - "request_latency"
    - "error_rate"
```