FROM rust:1.85-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/transmitarr-stream-proxy /usr/local/bin/transmitarr-stream-proxy
EXPOSE 8888
ENV RUST_LOG=info
CMD ["transmitarr-stream-proxy"]
