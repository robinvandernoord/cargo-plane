# Build host for cargo-plane on a Fly.io Machine.
# Toolchain, cargo registry and synced projects live on the /data volume,
# so builds stay incremental across machine restarts.
FROM rust:1-slim-trixie

# sshd listens on 2222: port 22 belongs to Fly's own SSH server (`fly ssh console`).
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server rsync iproute2 pkg-config \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd \
 && printf 'Port 2222\nPasswordAuthentication no\n' > /etc/ssh/sshd_config.d/cargo-plane.conf

# Add -dev packages your crates link against here, e.g.:
# RUN apt-get update && apt-get install -y --no-install-recommends libssl-dev && rm -rf /var/lib/apt/lists/*

COPY entrypoint.sh /usr/local/bin/cargo-plane-entrypoint
ENTRYPOINT ["/usr/local/bin/cargo-plane-entrypoint"]
