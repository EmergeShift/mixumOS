# Build context: directory containing the release `mixum-gateway` binary.
# Produced by cloud/gateway/build.sh (cargo build --release -p mixum-gateway).
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY mixum-gateway /usr/local/bin/mixum-gateway

ENTRYPOINT ["/usr/local/bin/mixum-gateway"]
