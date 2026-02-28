FROM rust:1.85-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /build/target/release/dispatcharr-proxy /usr/local/bin/dispatcharr-proxy
EXPOSE 8888
ENV RUST_LOG=info
CMD ["dispatcharr-proxy"]
