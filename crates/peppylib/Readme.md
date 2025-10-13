# Peppylib Rust

Peppy control interface lib in Rust. Connects to a peppyOS service.
It works on top of PMI (Peppy Messaging Interface) and provides a structure for topics/services/actions messages.

## How to publish?

Simply use `cargo publish` or `cargo publish --dry-run` at the root of this directory.
Before you do, don't forget to update the `Cargo.toml` manifest, notably the `version` attribute.
