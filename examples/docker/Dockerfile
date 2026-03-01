FROM debian:bookworm-slim

ARG EGREGORE_VERSION=latest
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) ARCH="x86_64-unknown-linux-gnu" ;; \
      arm64) ARCH="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$EGREGORE_VERSION" = "latest" ]; then \
      URL="https://github.com/pknull/egregore/releases/latest/download/egregore-${ARCH}.tar.gz"; \
    else \
      URL="https://github.com/pknull/egregore/releases/download/${EGREGORE_VERSION}/egregore-${ARCH}.tar.gz"; \
    fi; \
    curl -fsSL "$URL" | tar xz -C /usr/local/bin egregore; \
    chmod +x /usr/local/bin/egregore; \
    egregore --version

VOLUME /data

EXPOSE 7654 7655

ENTRYPOINT ["egregore", "--data-dir", "/data"]
