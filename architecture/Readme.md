# Introduction

This document describes how PeppyOS works, what are the different commands associated to it and how it's architectured.

## Chapters

### The `peppy` cli

The `peppy` cli is a compiled program that acts as bridge to:
    - Gather information about the running nodes
    - Communicate and pull information from other nodes in the same network
    - Watch for file system changes and create new code interfaces when those changes to `peppy.json5` are detected

![Link to the diagram](./peppy-cli.mmd)

The following sub commands are available as part of the `peppy` cli:

    - `nodes`: All commands related to the nodes
    - `serve`: Command relating to starting the `peppy` daemon

### The peppy root node

The peppy root node is a node that is unique (there can be no more than a single instance of a root node per machine) and its role is to compile all the nodes present on the same machine together such that all the nodes run as separate threads (by default, `run_mode: "fork"` can be specified in their configuration).

### The `peppy.json5` configuration files

Every peppy node is represented by a single `peppy.json5` file.
