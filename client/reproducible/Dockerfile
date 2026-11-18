# The page's build environment, pinned so anyone can build the same bytes. scripts/build-page.sh uses it; see client/reproducible/README.md.
# Everything is pinned: the base image by digest, Debian packages to one snapshot date, wasm-bindgen to the version in
# client/wasm/Cargo.lock (passed in), and binaryen by its tarball's hash. Always run as linux/amd64.
FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7

ARG DEBIAN_SNAPSHOT=20261115T000000Z
RUN printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s trixie main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
 && rm -f /etc/apt/sources.list.d/* \
 && apt-get update -q \
 && apt-get install -yq --no-install-recommends clang llvm nodejs curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN rustup target add wasm32-unknown-unknown

ARG WASM_BINDGEN_VERSION
RUN test -n "$WASM_BINDGEN_VERSION" && cargo install wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked

# The wasm-opt that wasm-pack 0.13.1 used.
ARG BINARYEN=version_117
ARG BINARYEN_SHA256=3dc677006555b355ea2da5e82602065a161d5e83eaefd3f759afa00b96e83212
RUN curl -fsSL -o /tmp/binaryen.tar.gz "https://github.com/WebAssembly/binaryen/releases/download/$BINARYEN/binaryen-$BINARYEN-x86_64-linux.tar.gz" \
 && echo "$BINARYEN_SHA256  /tmp/binaryen.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/binaryen.tar.gz -C /opt \
 && ln -s "/opt/binaryen-$BINARYEN/bin/wasm-opt" /usr/local/bin/wasm-opt \
 && rm /tmp/binaryen.tar.gz

COPY build-inside.sh /usr/local/bin/build-inside.sh
ENTRYPOINT ["/usr/local/bin/build-inside.sh"]
