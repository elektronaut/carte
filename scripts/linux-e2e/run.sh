#!/bin/bash
# End-to-end test of `carte setup` on Linux, in systemd containers.
#
#   run.sh [--distro debian|fedora|ubuntu] [--bin <carte>] [--browsers] [--keep]
#   run.sh clean
#
# Without --bin, carte is built for Linux in a builder container whose cargo
# registry and target directory live in named volumes, so reruns are cheap.
# --bin takes a Linux carte built elsewhere, as CI does. --keep leaves the
# containers running to look around in. `clean` removes every container, image
# and volume this script made (all are labelled carte-e2e=1).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
label=carte-e2e=1
distros="debian fedora ubuntu"
bin=
browsers=
keep=0

clean() {
  docker ps -aq --filter "label=$label" | xargs -r docker rm -f >/dev/null
  docker images -q --filter "label=$label" | sort -u | xargs -r docker rmi -f >/dev/null
  docker volume ls -q --filter "label=$label" | xargs -r docker volume rm >/dev/null
}

while [ $# -gt 0 ]; do
  case $1 in
    clean) clean; exit ;;
    --distro) distros=$2; shift ;;
    --bin) bin=$(cd "$(dirname "$2")" && pwd)/$(basename "$2"); shift ;;
    --browsers) browsers=--browsers ;;
    --keep) keep=1 ;;
    *) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
  esac
  shift
done

scratch=$(mktemp -d "${TMPDIR:-/tmp}/carte-e2e.XXXXXX")
containers=()
cleanup() {
  if [ $keep = 0 ] && [ ${#containers[@]} -gt 0 ]; then
    docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

if [ -z "$bin" ]; then
  echo "Building carte for Linux"
  for volume in carte-e2e-cargo-registry carte-e2e-target; do
    docker volume inspect "$volume" >/dev/null 2>&1 || docker volume create --label "$label" "$volume" >/dev/null
  done
  docker run --rm --label "$label" \
    -v "$repo:/src:ro" \
    -v carte-e2e-cargo-registry:/usr/local/cargo/registry \
    -v carte-e2e-target:/target \
    -v "$scratch:/out" \
    -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
    -w /src rust:1-bookworm \
    sh -c 'cargo build --locked -p carte --bin carte && cp /target/debug/carte /out/carte'
  bin=$scratch/carte
fi

status=0
for distro in $distros; do
  echo "######## $distro"
  docker build --label "$label" -t "carte-e2e-$distro" -f "$here/Dockerfile.$distro" "$here"
  name=carte-e2e-$distro-$$
  containers+=("$name")
  docker run -d --name "$name" --label "$label" --hostname "carte-e2e-$distro" \
    --privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock \
    "carte-e2e-$distro" >/dev/null
  state=
  for _ in $(seq 60); do
    state=$(docker exec "$name" systemctl is-system-running 2>/dev/null || true)
    case $state in running | degraded) break ;; esac
    sleep 1
  done
  echo "systemd: $state"
  docker cp "$bin" "$name:/usr/local/bin/carte"
  docker cp "$here/e2e.sh" "$name:/usr/local/bin/e2e.sh"
  docker exec "$name" /usr/local/bin/e2e.sh $browsers || status=1
  if [ $keep = 1 ]; then
    echo "Left running: docker exec -it $name bash"
  fi
done
exit $status
