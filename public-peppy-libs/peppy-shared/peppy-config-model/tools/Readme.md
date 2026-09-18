# How to build

The bundled compiler is pinned to Cap'n Proto 1.5.0. Download the source from
the [official installation instructions](https://capnproto.org/install.html)
and verify its SHA-256 checksum before building:

```sh
CAPNP_VERSION=1.5.0
curl -O "https://capnproto.org/capnproto-c++-${CAPNP_VERSION}.tar.gz"
# Expected SHA-256: 77dbc13ca82d9c87ddb4581dd49559d45b63096433d3dadea08b7f31b360a5ba
tar zxf "capnproto-c++-${CAPNP_VERSION}.tar.gz"
cd "capnproto-c++-${CAPNP_VERSION}"

# Linux
./configure --enable-static --disable-shared LDFLAGS="-static-libgcc -static-libstdc++" CXXFLAGS="-O2"
make -j$(nproc)

# macOS (Apple Silicon)
./configure --enable-static --disable-shared CXXFLAGS="-O2"
make -j$(sysctl -n hw.ncpu)
```

The binary will be in `<root_dir>/capnp`. Build each artifact on its matching
operating system and architecture, then confirm it with `file capnp` and
`./capnp --version` before replacing the corresponding file in this directory.
