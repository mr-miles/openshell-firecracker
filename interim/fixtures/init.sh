#!/bin/bash
# Shaped like OpenShell's guest init script (only the part the patch touches).
set -euo pipefail

mount_initial_fs() {
    mount -t proc proc /proc 2>/dev/null || true
    mount -t sysfs sysfs /sys 2>/dev/null || true
    mount -t tmpfs tmpfs /tmp 2>/dev/null || true
    mount -t tmpfs tmpfs /run 2>/dev/null || true
    mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
}

other() {
    mount -t devtmpfs devtmpfs /dev 2>/dev/null &
}
