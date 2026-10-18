#!/bin/sh
# A live SSH session keeps the machine awake, including throughout long builds.
set -eu
: "${IDLE_MINUTES:=15}"

mkdir -p /data/docker /data/ssh /root/.ssh /run/sshd
printf '%s\n' "${SSH_AUTHORIZED_KEYS:?set SSH_AUTHORIZED_KEYS}" > /root/.ssh/authorized_keys
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys
for type in ed25519 rsa; do
    key=/data/ssh/ssh_host_${type}_key
    [ -f "$key" ] || ssh-keygen -q -t "$type" -N '' -f "$key"
    ln -sf "$key" "/etc/ssh/ssh_host_${type}_key"
    ln -sf "$key.pub" "/etc/ssh/ssh_host_${type}_key.pub"
done

# The Machine's root disk can survive a hard stop, including stale PID files.
# This entrypoint exclusively owns the daemon and its managed containerd.
rm -f /var/run/docker.pid /var/run/docker/containerd/containerd.pid
dockerd --data-root=/data/docker --host=unix:///var/run/docker.sock &
docker_pid=$!
/usr/sbin/sshd -D -e &
ssh_pid=$!
timer_pid=
cleanup() {
    if [ -n "$timer_pid" ]; then
        kill "$timer_pid" 2>/dev/null || true
    fi
    kill "$docker_pid" "$ssh_pid" 2>/dev/null || true
    wait || true
}
trap cleanup EXIT
trap 'exit 0' INT TERM

idle=0
while :; do
    # A builtin wait lets TERM interrupt the timer immediately so dockerd gets
    # time to shut down before the platform's stop timeout expires.
    sleep 60 &
    timer_pid=$!
    wait "$timer_pid"
    timer_pid=
    kill -0 "$docker_pid" "$ssh_pid" || exit 1
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
