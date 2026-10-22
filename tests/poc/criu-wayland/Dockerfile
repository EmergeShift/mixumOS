FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    criu weston foot sway iproute2 procps tini \
 && rm -rf /var/lib/apt/lists/*
RUN useradd -m demo
COPY session.sh /usr/local/bin/session.sh
RUN chmod 755 /usr/local/bin/session.sh
ENTRYPOINT ["/usr/bin/tini","--"]
CMD ["sleep","infinity"]
