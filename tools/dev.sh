#!/bin/sh
# Runs a command in the build container: ./tools/dev.sh cargo build --release
set -e
cd "$(dirname "$0")/.."
engine="${DOCKER:-podman}"
"$engine" image exists screenlet-windows-dev 2>/dev/null || "$engine" build -q -t screenlet-windows-dev -f Containerfile .
z=""; [ "$engine" = podman ] && z=":Z"
exec "$engine" run --rm -v "$PWD:/src$z" -v screenlet-cargo:/cargo$z -w /src screenlet-windows-dev "$@"
