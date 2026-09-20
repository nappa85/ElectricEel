# Build and release

## Stage the in-app bundle

`helper/make-app-bundle.sh` cross-builds the Rust staticlib
(`aarch64-unknown-linux-gnu`, glibc) and the Go `tesla-session` child
(`CGO_ENABLED=0 GOOS=linux GOARCH=arm64`), staging them into
`app/thirdparty/` and `app/bin/`. Requires the `aarch64-unknown-linux-gnu`
Rust target (installed automatically if missing). Re-run whenever
`helper/` changes; nothing under `app/thirdparty` or `app/bin` is
committed. `tools/build-qm.sh` refreshes the translation catalogs.

## Build the RPM (Sailfish Platform SDK, Docker)

The `coderus/sailfishos-platform-sdk-aarch64` image (~13 GB) provides
the `SailfishOS-5.2.0.15-aarch64` target. Host-container UID mismatch
means bind-mounting does not work; copy in and out:

```sh
docker pull coderus/sailfishos-platform-sdk-aarch64

docker create --name electric-eel-build coderus/sailfishos-platform-sdk-aarch64 sleep infinity
docker start electric-eel-build

docker cp app electric-eel-build:/home/mersdk/app
docker exec -u root electric-eel-build chown -R mersdk:mersdk /home/mersdk/app
docker exec -w /home/mersdk/app electric-eel-build \
  mb2 --target SailfishOS-5.2.0.15-aarch64 build
docker cp electric-eel-build:/home/mersdk/app/RPMS/harbour-electric-eel-<version>-1.aarch64.rpm app/RPMS/

docker rm -f electric-eel-build
```

rpmlint's Sailfish config accepts only old Fedora short license names
(`ASL 2.0`, not `Apache-2.0`) and requires a `%changelog` section. The
remaining rpmlint errors on the Go child (statically linked binary in
`/usr/share`) are pre-existing and treated as warnings by the build
config. QML files are not compiled by `mb2`, only reviewed.

## Install on the phone

One package (`devel-su` only for `pkcon` itself):

```sh
scp app/RPMS/harbour-electric-eel-<version>-1.aarch64.rpm defaultuser@<phone-ip>:/tmp/
ssh defaultuser@<phone-ip>
devel-su pkcon install-local /tmp/harbour-electric-eel-<version>-1.aarch64.rpm
```

After updating, kill the running instance once (`pkill -f
harbour-electric-eel` as `defaultuser`, then relaunch from the grid):
the launcher uses `--single-instance`, so a backgrounded process would
otherwise keep running the old QML.

## Release (GitHub CI)

`.github/workflows/release.yml` builds the RPM and publishes it. Push a
tag to trigger it:

```sh
git tag v<version>
git push origin v<version>
```

The RPM version comes from the tag (leading `v` stripped); the spec,
the `.pro` `VERSION`, and `helper/Cargo.toml` are stamped from it, so
the Settings page versions and `core_version()` match. The bundle
(staticlib + `tesla-session`) is rebuilt on the runner; no binaries are
committed. Tag pushes attach the RPM to the release; manual
`workflow_dispatch` runs upload a workflow artifact instead.
