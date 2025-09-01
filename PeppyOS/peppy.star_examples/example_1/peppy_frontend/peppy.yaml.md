```yaml
# Frontend node that connects to a peppy backend
node_config:
  name: "peppy_frontend"
  namespace: "/"
  version: "0.1.0"
  tags:
    - "peppy"
    - "rest"
    - "frontend"
  respawn: true
  respawn_delay: 2.0

node_parameters:
  # This is where the parameters to connect to the API node are defined
  server_api:
    host: "localhost"
    port: 8080
    endpoint: "/"

# This node does not expose anything for other nodes. It exposes an Astro frontend to the end user but that is outside of the realm of node-to-node communication
exposes: []

# This node does not subscribe to any other node, it connects to `peppy_backend` via its REST API, which is outside the node-to-node communication realm 
subscribes_to: []

resources:
  max_memory_mb: 512

logging:
  min_level: "info"
```