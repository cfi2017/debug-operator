FROM rust:1.95-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin debug-operator

FROM alpine:3.22
RUN apk add --no-cache ca-certificates
COPY --from=builder /src/target/release/debug-operator /usr/local/bin/debug-operator
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/debug-operator"]
