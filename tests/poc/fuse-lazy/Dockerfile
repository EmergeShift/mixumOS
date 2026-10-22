FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    fuse3 libfuse3-dev gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*
COPY passthrough.c /poc/passthrough.c
COPY run.sh /poc/run.sh
RUN cc -O2 -Wall -Wextra -o /poc/passthrough /poc/passthrough.c -lfuse3
