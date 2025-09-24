# Introduction

This document describes how PeppyOS works, what are the different commands associated to it and how it's architectured.

## Chapters

### The `peppy` cli

The `peppy` cli is a compiled program that acts as bridge to:
    - Gather information about the running nodes
    - Communicate and pull information from other nodes in the same network
    - Watch for file system changes and create new code interfaces when those changes to `peppy.json5` are detected
    - Add a node to its list of running nodes called the "node stack"

![Link to the diagram](./peppy-cli.mmd)

The following sub commands are available as part of the `peppy` cli:

    - `nodes`: All commands related to the nodes
    - `serve`: Command relating to starting the `peppy` daemon. This command is used by systemd to run `peppy` in the background. The command maintains a "stack of nodes". The list is regularly updated 
    - `add`: Add a new node to the node stack. The node becomes referenced by the cluster from its position on the filesystem or the network
    - `push`: Push a new node to the node stack. The difference with `add` is that it takes the entire files & folders starting from the root dir of `peppy.json5` and move them to a cache folder from which the node is then started.
    - `sync`: Must be run in a folder where `peppy.json5` is present. It interrogate the node stack and check the current node from which the command is run to determine if the node depends on other nodes, it then generates interface code (in Python or Rust) based on the `subscribes_to` and `exposes` definition of the `peppy.json5`. If the current node has a `subscribes_to` and the subscribed node is not present in the node stack, the generated interface raises a warning saying the expected node message format could not be found.
    - `list`: preview of the node stack and where the `peppy.json5` are located.
  
### The peppy root node

The peppy root node is a node that is unique (there can be no more than a single instance of a root node per machine) and its role is to compile all the nodes present on the same machine together such that all the nodes run as separate processes. Another run mode `thread` is available, but if a node with that mode is added to PeppyOS, the root node will force a restart of the entire node stack.

### The `peppy.json5` configuration files

Every peppy node is represented by a single `peppy.json5` file.
