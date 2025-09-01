```yaml
# PeppyOS Root Node Configuration
# This is the main configuration file that defines the root node of the current machine.
# It's run using the `peppy node serve` command. It will automatically use the `peppy.yaml` file in the same path the command is run as the root configuration file and will look into child folders for other `peppy.yaml` files for children nodes.

# Any new node that is created should be added to the root_node by running the command `peppy node add <path_to_peppy.yaml>`

# There is one root node/service per machine/OS and they automatically discover the other root nodes that are on the same network running on different computers

# When running in production, the `peppy service install` command moves the current node and its children into the OS directories and installs a systemd service that runs on a different port and namespace on the device (so that the dev and prod environments do not )

node_config:
  name: "<root_node>" # Reserved name, a random name will be generated for the root node
  namespace: "/" # namespace is implicitely prefixed with `/dev` in dev environment (running outside `systemd`) and `/prod` when installed with `peppy service install`
  version: "0.1.0"
  auto_start: true
  respawn: false
  respawn_delay: 2.0 # seconds

node_parameters:

exposes:
  topics: []
  services: []
  actions: []

resources:
  max_memory_mb: 512
  cpu_affinity: []

logging:
  min_level: "info"
  file_path: "/var/log/peppy/peppy_root.log"
  max_file_size_mb: 100
  format: "text"
```