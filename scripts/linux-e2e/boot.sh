#!/bin/sh
# The container's init. Most of /proc/sys is the host kernel's, shared with
# every other container, so only the network namespace's part stays writable.
set -e
mount --bind /proc/sys /proc/sys
mount -o remount,bind,ro /proc/sys
mount --bind /proc/sys/net /proc/sys/net
mount -o remount,bind,rw /proc/sys/net
exec /sbin/init
