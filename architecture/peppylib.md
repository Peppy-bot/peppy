# Introduction

`peppylib` is the library that ships with either Rust or Python to communicate with the rest of the nodes in the system. It also expose the current node to the "node stack" (an in-memory stack of nodes and their location maintained by the `peppy` daemon).

## How does it work?

`peppylib` runs the following steps:

 1. Reads the `peppy.json5`
 2. Exposes the node to the `peppy` daemon that responds with an acknoledgement
 3. If a `consumes` section is defined, poll the `peppy` daemon to resolve the expected input type of the `topic`/`services`/`nodes`.
