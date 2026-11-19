#!/bin/sh
set -e

## Local build helper: bump the RPM release (-N) on each build, reset to 1
## when VERSION changes. See docs/build.md.

ROOT="$(cd "$(dirname "$0")" && pwd)"
CONTAINER="${CONTAINER:-electric-eel-build}"
TARGET="${TARGET:-SailfishOS-5.2.0.15-aarch64}"
# Pinned SDK image (keep in sync with .github/workflows/*.yml and
# helper/build-go-library-sdk.sh). Exported so make-app-bundle.sh's Go
# build uses the same image.
SAILFISH_SDK_IMAGE="${SAILFISH_SDK_IMAGE:-coderus/sailfishos-platform-sdk-aarch64:5.2.0.15@sha256:e5f7596d14502746b308c3dde36f65fc25d7f76c45a01acaed56a1f55b76f592}"
export SAILFISH_SDK_IMAGE SAILFISH_TARGET="$TARGET"
STATE_FILE="$ROOT/.build-release"
PRO_FILE="$ROOT/app/harbour-electric-eel.pro"
SPEC_IN_CONTAINER="/home/mersdk/app/rpm/harbour-electric-eel.spec"

# App version comes from the .pro file (single source of truth).
VERSION="$(sed -n 's/^VERSION *= *//p' "$PRO_FILE" | head -1 | tr -d '[:space:]')"
if [ -z "$VERSION" ]; then
    echo "Could not read VERSION from $PRO_FILE" >&2
    exit 1
fi

# Release counter: increment on each build, reset to 1 when VERSION changes.
RELEASE=1
if [ -f "$STATE_FILE" ]; then
    LAST_VERSION="$(awk 'NR==1{print $1}' "$STATE_FILE")"
    LAST_RELEASE="$(awk 'NR==1{print $2}' "$STATE_FILE")"
    if [ "$LAST_VERSION" = "$VERSION" ] && [ -n "$LAST_RELEASE" ]; then
        RELEASE=$((LAST_RELEASE + 1))
    fi
fi
echo "$VERSION $RELEASE" > "$STATE_FILE"

RPM_NAME="harbour-electric-eel-${VERSION}-${RELEASE}.aarch64.rpm"
echo "Building $RPM_NAME"

if ! docker inspect "$CONTAINER" >/dev/null 2>&1; then
    echo "Creating SDK container $CONTAINER from $SAILFISH_SDK_IMAGE"
    # The SDK's sb2d is a 32-bit i386 binary that opens its sockets via
    # socketcall(2); Docker 29.4.x's builtin seccomp profile makes socketcall
    # return ENOSYS, so sb2 (and every cgo/mb2 compile) fails. Pin Docker's own
    # upstream default profile (moby docker-v29.9.3, socketcall allowed again).
    docker create --name "$CONTAINER" \
        --security-opt "seccomp=$ROOT/tools/sailfish-sdk-seccomp.json" \
        "$SAILFISH_SDK_IMAGE" sleep infinity
fi

docker start "$CONTAINER" >/dev/null

docker exec "$CONTAINER" sudo rm -rf /home/mersdk/app || true
"$ROOT/helper/make-app-bundle.sh"
docker cp "$ROOT/app" "$CONTAINER":/home/mersdk/app
docker exec -u root "$CONTAINER" chown -R mersdk:mersdk /home/mersdk/app

# Patch Version/Release only inside the container so local git stays clean.
docker exec "$CONTAINER" sed -i \
    -e "s/^Version:.*/Version:       $VERSION/" \
    -e "s/^Release:.*/Release:    $RELEASE/" \
    "$SPEC_IN_CONTAINER"

docker exec -w /home/mersdk/app "$CONTAINER" \
    mb2 --target "$TARGET" build

mkdir -p "$ROOT/app/RPMS"
docker cp "$CONTAINER":/home/mersdk/app/RPMS/"$RPM_NAME" \
    "$ROOT/app/RPMS/"
echo "Built $ROOT/app/RPMS/$RPM_NAME"
