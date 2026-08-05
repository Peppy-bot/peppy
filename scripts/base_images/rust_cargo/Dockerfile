FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

ENV PATH="/root/.cargo/bin:$PATH"

# Mount point for the peppy container build cache.
RUN mkdir /peppy-cache

# sccache for the build cache, pinned and checksum verified per architecture.
# The musl builds are fully static. The peppy daemon activates it by setting
# RUSTC_WRAPPER=sccache with SCCACHE_DIR inside /peppy-cache.
ARG TARGETARCH
RUN set -eu; \
    SCCACHE_VERSION=v0.16.0; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64; sha=aec995a83ad3dff3d14b6314e08858b7b73d35ca85a5bcf3d3a9ec07dee35588 ;; \
      arm64) arch=aarch64; sha=f73a5c39f96bb6ebb89cc7915cf182260d4cbf30765322c5e793d0fe8bd80784 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    name="sccache-${SCCACHE_VERSION}-${arch}-unknown-linux-musl"; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/${SCCACHE_VERSION}/${name}.tar.gz" -o /tmp/sccache.tgz; \
    echo "${sha}  /tmp/sccache.tgz" | sha256sum -c -; \
    tar -xzf /tmp/sccache.tgz -C /tmp; \
    install -m 0755 "/tmp/${name}/sccache" /usr/local/bin/sccache; \
    rm -rf /tmp/sccache.tgz "/tmp/${name}"
