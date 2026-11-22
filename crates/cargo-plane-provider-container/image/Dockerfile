# Build container for cargo-plane. `cargo plane setup container` builds this
# on the SSH host from stdin, so it has no build context: everything is inline,
# and setup puts `entrypoint.sh` into the ENTRYPOINT line as a JSON string.
# The base must be Debian or Ubuntu (apt-get).
ARG BASE=ubuntu:24.04
FROM ${BASE}

ARG PACKAGES=""
ENV DEBIAN_FRONTEND=noninteractive
# sshd only runs per connection (`sshd -i`), never as a daemon. PAM is off:
# key logins as root don't need it, and pam_loginuid fails in containers.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      openssh-server rsync build-essential pkg-config curl ca-certificates git $PACKAGES \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd /work \
 && ssh-keygen -A \
 && printf 'UsePAM no\nPasswordAuthentication no\nPermitRootLogin prohibit-password\nLogLevel ERROR\n' \
      > /etc/ssh/sshd_config.d/cargo-plane.conf

# In ~/.cargo, which cargo-plane sources before every command (sshd doesn't
# pass on the image's ENV).
# Downloaded first: piped into sh, a failed download would be an empty
# script that "succeeds".
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh \
 && sh /tmp/rustup-init.sh -y --profile minimal --component clippy --component rustfmt \
 && rm /tmp/rustup-init.sh \
 && /root/.cargo/bin/cargo --version

# Last, so changing the key doesn't rebuild the layers above.
ARG PUBKEY
RUN test -n "$PUBKEY" \
 && mkdir -p /root/.ssh \
 && printf '%s\n' "$PUBKEY" > /root/.ssh/authorized_keys \
 && chmod 700 /root/.ssh \
 && chmod 600 /root/.ssh/authorized_keys

ENTRYPOINT ["/bin/sh", "-c", @ENTRYPOINT_SCRIPT@, "cargo-plane-entrypoint"]
