FROM docker.io/library/rust:1.100.0-slim-bookworm@sha256:bb875b0f8ce726053e259034848c471840e96bfe591fd52556178ae9a05e6126 AS builder
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
COPY Cargo.toml Cargo.lock ./
COPY rust/Cargo.toml ./rust/Cargo.toml
RUN mkdir -p rust/src && printf 'pub fn dependency_cache() {}\n' > rust/src/lib.rs \
    && printf 'fn main() {}\n' > rust/src/main.rs \
    && cargo build --locked --release --bin open-assets
COPY rust ./rust
COPY web/static ./web/static
COPY config/repositories.example.yaml ./config/repositories.example.yaml
RUN cargo clean --release --package open-assets \
    && find rust -type f -name '*.rs' -exec touch {} + \
    && cargo build --locked --release --bin open-assets \
    && /src/target/release/open-assets --version | grep '^open-assets [0-9]'

FROM docker.io/library/debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e
RUN apt-get update && apt-get install -y --no-install-recommends git openssh-client ca-certificates openssl && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 20010 --create-home --shell /bin/sh publisher
COPY --from=builder /src/target/release/open-assets /usr/local/bin/open-assets
USER 20010:20010
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/open-assets"]
CMD ["worker"]
