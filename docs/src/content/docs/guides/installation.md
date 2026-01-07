---
title: Installation
description: How to install PeppyOS on your system
---

To install `peppy` you can run the following command in your terminal:

```sh
curl -fsSL https://peppy.bot/install.sh | bash
```

If you already have a release archive (for example from CI), you can install from a local file by passing its path to the installer:

```sh
curl -fsSL https://peppy.bot/install.sh | bash -s -- ./peppy-x86_64-unknown-linux-gnu.tar.gz
```

If you cloned the repository, you can also run:

```sh
./scripts/install.sh ./dist/peppy-x86_64-unknown-linux-gnu.tar.gz
```
