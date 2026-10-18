#!/bin/sh
# Runs sshd and stops the machine (by exiting) after IDLE_MINUTES without SSH
# connections. Deploy with `--restart no` so exiting stops the machine.
set -eu

: "${IDLE_MINUTES:=15}"

# Persist the toolchain on the volume; the image's copy seeds it on first boot.
if [ ! -d /data/rustup ]; then
    cp -a /usr/local/rustup /data/rustup
fi
mkdir -p /data/cargo /data/cargo-plane

mkdir -p /root/.ssh
printf '%s\n' "${SSH_AUTHORIZED_KEYS:?set SSH_AUTHORIZED_KEYS to your public key(s)}" > /root/.ssh/authorized_keys
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys

# Host keys on the volume, so they survive restarts and image updates.
mkdir -p /data/ssh
for type in ed25519 rsa; do
    key=/data/ssh/ssh_host_${type}_key
    [ -f "$key" ] || ssh-keygen -q -t "$type" -N '' -f "$key"
    ln -sf "$key" "/etc/ssh/ssh_host_${type}_key"
    ln -sf "$key.pub" "/etc/ssh/ssh_host_${type}_key.pub"
done

/usr/sbin/sshd -D -e &

idle=0
while sleep 60; do
    if [ "$(ss -Htn state established '( sport = :2222 )' | wc -l)" -gt 0 ]; then
        idle=0
    else
        idle=$((idle + 1))
    fi
    if [ "$idle" -ge "$IDLE_MINUTES" ]; then
        echo "no SSH connections for $IDLE_MINUTES minutes, stopping"
        exit 0
    fi
done
