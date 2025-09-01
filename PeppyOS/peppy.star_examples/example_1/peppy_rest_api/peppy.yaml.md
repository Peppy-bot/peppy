```yaml
# Rest API that connects to the root peppy node or multiple root nodes and expose their status via a REST API
node_config:
  name: "peppy_rest_api"
  namespace: "/"
  version: "0.1.0"
  tags:
    - "peppy"
    - "rest"
  respawn: true
  respawn_delay: 3.0

node_parameters:
  http:
    host: "0.0.0.0"
    port: 8080
    cors_enabled: true
    cors_origins: "*"
    max_connections: 100
    request_timeout_ms: 30000

exposes:
  services:
    - type: "http/rest"
      name: "/peppy/rest_api"

subscribes_to: []

resources:
  max_memory_mb: 256
  cpu_affinity: []

logging:
  min_level: "info"

diagnostics:
  enabled: true
  publish_rate_hz: 1.0
  health_checks:
    - "camera_connected"
    - "frame_rate_nominal"
```