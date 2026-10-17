#!/usr/bin/env bash
# Builds all three packages locally the same way the GitHub workflow does, using podman (or docker).
#   packaging/build-in-containers.sh            # everything
#   packaging/build-in-containers.sh rpm        # just one target
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="$(command -v podman || command -v docker)"
targets=("${@:-deb-appimage rpm}")
for t in ${targets[*]}; do
    case "$t" in
        deb-appimage) image=docker.io/library/ubuntu:22.04 ;;
        rpm) image=registry.fedoraproject.org/fedora:41 ;;
        *) echo "unknown target $t" >&2; exit 2 ;;
    esac
    echo "==> $t in $image"
    "$ENGINE" run --rm -v "$ROOT:/src" -w /src \
        -e CARGO_TARGET_DIR="/src/target/container-$t" \
        -e CARGO_HOME=/src/target/container-cargo-home \
        "$image" bash -c "packaging/build.sh $t"
done
ls -la "$ROOT/dist"
