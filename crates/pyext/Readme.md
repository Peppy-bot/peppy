# Peppycl python

Peppy control interface lib in Python. Connects to a peppyOS service. Builds on top of `peppycl` written in Rust.

## How to build?

Change the version attribute in `./peppycl/_version.py` and `./pixi.toml` and run `pixi build`.

## How to publish?

Build the package first then go to [this page](https://prefix.dev/channels/peppy) to upload the `.conda` file.
