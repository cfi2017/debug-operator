FROM rust:1.95-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin debug-operator

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/debug-operator /usr/local/bin/debug-operator
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/debug-operator"]
