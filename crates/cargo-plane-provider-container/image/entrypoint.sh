#!/bin/sh
# Keeps the build container running. Builds don't go through this process:
# each SSH connection is a separate `sshd -i` started with `docker exec`.
#
# With IDLE_MINUTES set above 0, exits (stopping the container, which runs
# with `--restart no`) once no other process has run for that many minutes.
# The container runs with `--init`, so PID 1 is the init process, not this.
IDLE_MINUTES=${IDLE_MINUTES:-0}
case "$IDLE_MINUTES" in
    '' | *[!0-9]*) IDLE_MINUTES=0 ;;
esac
if [ "$IDLE_MINUTES" -eq 0 ]; then
    exec sleep infinity
fi

idle=0
while sleep 60; do
    # The glob expands in this shell, so the check itself starts no process.
    busy=0
    for proc in /proc/[0-9]*; do
        case "${proc#/proc/}" in
            1 | "$$") ;;
            *)
                busy=1
                break
                ;;
        esac
    done
    if [ "$busy" -eq 1 ]; then
        idle=0
    else
        idle=$((idle + 1))
    fi
    if [ "$idle" -ge "$IDLE_MINUTES" ]; then
        echo "nothing ran for $IDLE_MINUTES minutes, stopping"
        exit 0
    fi
done
