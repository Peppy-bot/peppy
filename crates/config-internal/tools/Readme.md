# How to build?

Pull the latest version from the install instructions [here](https://capnproto.org/install.html).
And use the following flags during compilation for static linking:

```sh
curl -O https://capnproto.org/capnproto-c++-1.3.0.tar.gz
tar zxf capnproto-c++-1.3.0.tar.gz
cd capnproto-c++-1.3.0
# Linux
./configure --enable-static --disable-shared LDFLAGS="-static-libgcc -static-libstdc++" CXXFLAGS="-O2"
make -j$(nproc)

# macOS (Apple Silicon)
./configure --enable-static --disable-shared CXXFLAGS="-O2"
make -j$(sysctl -n hw.ncpu)
```

the binary will be in `<root_dir>/capnp`.
