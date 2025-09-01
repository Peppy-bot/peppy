```yaml
# PeppyOS Root Node Configuration
# This is the main configuration file that defines the root node of the current machine.
# It's run using the `peppy node serve` command. It will automatically use the `peppy.yaml` file in the same path the command is run as the root configuration file and will look into child folders for other `peppy.yaml` files for children nodes.

# Any new node that is created should be added to the root_node by running the command `peppy node add <path_to_peppy.yaml>`

# There is only one root node/service per machine/OS and they automatically discover the other root nodes that are on the same network running on different computers

# When running in production, the `peppy service deploy` command moves the current node and its children into the OS directories and installs a systemd service that runs on a different namespace on the device (so that the dev and prod environments do not get mingled together)

node_config:
  is_root: true
  name: "my_robot" # Because it's a root node, this name will be prefixed by a small UID to avoid conflicts with other root nodes in the same network
  namespace: "/" # namespace is implicitely prefixed with `/dev` in dev environment (running outside `systemd`) and `/prod` when installed with `peppy service install`
  version: "0.1.0"
  respawn: false
  respawn_delay: 2.0 # seconds

node_parameters:
  status:
    frequency: 1Hz # Every second the root node publishes its latest status

# The root node cannot subscribe to any node
exposes:
  topics:
    # Exposes its status that includes:
    #  - Its name & configuration
    #  - The node list
    #  - The communication channels between the nodes
    #  - etc..
    # This publishing allow another node such as `peppy_rest_api` to aggregate root nodes statuses
    # and expose that information under a single REST API for a frontend to consume.
    - type: "configuration/metadata"
      name: "/root_node/status"
      qos_profile: "standard"  # Options: "standard", "reliable", "sensor_data"
  services: []
  actions: []

resources:
  max_memory_mb: 1024
  cpu_affinity: []

logging:
  min_level: "info"
  file_path: "/var/log/peppy/peppy_root.log"
  max_file_size_mb: 100
  format: "text"
```