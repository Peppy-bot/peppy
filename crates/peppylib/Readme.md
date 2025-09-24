# Peppylib Rust

Peppy control interface lib in Rust. Connects to a peppyOS service.

## How to publish?

Simply use `cargo publish` or `cargo publish --dry-run` at the root of this directory.
Before you do, don't forget to update the `Cargo.toml` manifest, notably the `version` attribute.

## Notes

The `peppy` project depends on `peppylib` to start the root node but `peppylib` depends on a messaging server started by `peppy` to operate.
