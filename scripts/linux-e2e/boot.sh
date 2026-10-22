#!/bin/sh
# The container's init. Most of /proc/sys and /sys is the host kernel's, shared
# with every other container, so only the network namespace's sysctls and the
# container's own cgroup tree stay writable. Any failure stops the container.
set -e
mount --bind /proc/sys /proc/sys
mount -o remount,bind,ro /proc/sys
mount --bind /proc/sys/net /proc/sys/net
mount -o remount,bind,rw /proc/sys/net
mount -o remount,bind,ro /sys
grep -q ' /sys/fs/cgroup rw,' /proc/self/mountinfo
exec /sbin/init
