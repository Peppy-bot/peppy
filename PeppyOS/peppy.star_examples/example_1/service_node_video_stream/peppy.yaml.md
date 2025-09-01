```yaml
# PeppyOS REST API Service Node Configuration
# This node provides a REST API interface for system interaction

depends_on:
  # The peppy service running in systemd will first try to access the node by polling its local registery and if no local node with `uvc_camera` is present, it will download it from nodes.peppy.bot
  - name: "uvc_camera"
    # Will pull this particular version if specified, otherwise pulls the most recent one (always specify it to avoid breaking changes)
    version: "0.1"
    namespace: "/"
    # If specified, will override default parameters or provide the ones that are required
    parameters: []

node_config:
  name: "web_video_stream"
  namespace: "/"
  version: "0.1.0"
  auto_start: true
  respawn: true
  respawn_delay: 10.0
  shutdown_timeout_s: 30.0

# The parameters of the current node
node_parameters:
  http:
    host: "0.0.0.0"
    port: 8080
    cors_enabled: true
    cors_origins: "*"
    max_connections: 100
    request_timeout_ms: 30000
  auth:
    enabled: false
    type: "jwt"
    secret_key: ""
  endpoints:
    prefix: "/api/v1"
    health_check: "/health"
    metrics: "/metrics"
    documentation: "/docs"
  rate_limit:
    enabled: true
    requests_per_minute: 60
    burst_size: 10
  video_stream:
    enabled: true
    format: "mjpeg"
    quality: 80
    max_fps: 30

exposes: []

subscribes_to:
  topics:
    - type: "sensor_msgs/Image"
      name: "uvc_camera:/camera/video_feed"
      callback: "handle_video_feed"
  services:
    - service_type: "custom_srvs/GetSystemInfo"
      service_name: "/api/get_system_info"
      callback: "handle_get_system_info"
      qos_profile: "reliable"
  actions:
    - action_type: "custom_actions/ProcessVideo"
      action_name: "/api/process_video"
      callback: "handle_process_video_action"
      qos_profile: "reliable"

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