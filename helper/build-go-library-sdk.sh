#!/usr/bin/env bash
# Cross-build cgo against Sailfish's libc, not the host's aarch64 sysroot.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
target="${SAILFISH_TARGET:-SailfishOS-5.2.0.15-aarch64}"
# Pinned SDK image (keep in sync with .github/workflows/*.yml and build.sh).
image="${SAILFISH_SDK_IMAGE:-coderus/sailfishos-platform-sdk-aarch64:5.2.0.15@sha256:e5f7596d14502746b308c3dde36f65fc25d7f76c45a01acaed56a1f55b76f592}"
container="electric-eel-go-build-$$"
go_root="$(go env GOROOT)"
module_cache="$(go env GOMODCACHE)"
(cd "$repo_root/helper/session" && go mod download)
mkdir -p "$repo_root/app/lib"
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
# The SDK's sb2d is a 32-bit i386 binary that opens its sockets via
# socketcall(2); Docker 29.4.x's builtin seccomp profile makes socketcall
# return ENOSYS, so sb2 (and every cgo/mb2 compile) fails. Pin Docker's own
# upstream default profile (moby docker-v29.9.3, socketcall allowed again).
docker create --name "$container" \
  --security-opt "seccomp=$repo_root/tools/sailfish-sdk-seccomp.json" \
  --volume "$repo_root:/home/mersdk/ee-src:ro" \
  --volume "$go_root:/home/mersdk/ee-go:ro" \
  --volume "$module_cache:/home/mersdk/ee-mod:ro" \
  "$image" sleep infinity >/dev/null
docker start "$container" >/dev/null
docker exec "$container" mkdir -p /home/mersdk/ee-build
docker exec --workdir /home/mersdk/ee-src/helper/session \
  --env GOROOT=/home/mersdk/ee-go --env GOMODCACHE=/home/mersdk/ee-mod \
  --env GOPATH=/home/mersdk/ee-build/gopath --env GOCACHE=/home/mersdk/ee-build/cache \
  --env TMPDIR=/home/mersdk/ee-build --env GOOS=linux --env GOARCH=arm64 \
  --env CGO_ENABLED=1 --env "CC=sb2 -t $target gcc" \
  "$container" /home/mersdk/ee-go/bin/go build -buildvcs=false -trimpath \
  -buildmode=c-shared -o /home/mersdk/ee-build/libteslable.so ./cmd/teslable
docker exec "$container" sb2 -t "$target" strip --strip-unneeded /home/mersdk/ee-build/libteslable.so
docker exec "$container" chmod 0644 /home/mersdk/ee-build/libteslable.so
docker cp "$container:/home/mersdk/ee-build/libteslable.so" "$repo_root/app/lib/libteslable.so"
