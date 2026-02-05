# Hello World Topic Exposes

This example demonstrates how to emit messages to a topic using the peppylib Python bindings.

## Prerequisites

Before running this example, make sure you have a zenohd server running. You can start one using the `zenohd_simple` example.

## Usage

```bash
uv run main.py
```

## What it does

1. Creates a `MessengerHandle` connected to a local zenohd server
2. Generates random names for `master_node` and `instance_id`
3. Emits a "Hello world" payload to the `hello_msg` topic with `Reliable` QoS
