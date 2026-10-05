#!/bin/bash
# Builds the udnf source and binary RPMs into target/rpm/, inside the build container
# (or directly when rpmbuild and cargo are available, as in CI).
# Sources are packed with the vendored crates so the SRPM builds offline (mock, COPR...).
# Usage: scripts/build-rpm.sh
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
name=udnf
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
spec_version=$(sed -n 's/^Version: *//p' udnf.spec)
[[ $version == "$spec_version" ]] || { echo "version Cargo.toml ($version) ≠ udnf.spec ($spec_version)" >&2; exit 1; }

out=$repo/target/rpm
rm -rf "$out"
mkdir -p "$out/SOURCES"

# Sources: everything but build outputs and VCS data.
tar --exclude-vcs --exclude=./target --exclude=./.cargo \
    --transform "s|^\.|$name-$version|" -czf "$out/SOURCES/$name-$version.tar.gz" .

# Inside a build environment (CI container) run directly, otherwise through the dev container.
if command -v rpmbuild >/dev/null && command -v cargo >/dev/null; then
    in_env=(sh -c)
else
    in_env=("$repo/scripts/dev.sh" sh -c)
fi

"${in_env[@]}" "
    set -e
    cd '$out'
    cargo vendor --locked --manifest-path '$repo/Cargo.toml' vendor >/dev/null
    tar -cJf SOURCES/$name-$version-vendor.tar.xz vendor
    rm -rf vendor
    rpmbuild -ba --quiet \
        --define '_topdir $out' --define '_sourcedir $out/SOURCES' \
        --define '_rpmdir $out' --define '_srcrpmdir $out' \
        '$repo/udnf.spec'
"
find "$out" -maxdepth 2 -name '*.rpm' | sort
