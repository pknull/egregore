FROM rust:1-bookworm AS builder

WORKDIR /build
COPY . .
RUN cargo build --release -p egregore

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/egregore /usr/local/bin/egregore

VOLUME /data

EXPOSE 7654 7655

ENTRYPOINT ["egregore", "--data-dir", "/data"]
