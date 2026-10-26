#!/usr/bin/env bash
# Check that the archive and .deb packages before_deploy.sh built for
# $TARGET carry the license notices. ci.yml and release.yml run it from
# the repository root after before_deploy.sh.

set -euxo pipefail

# The musl target links the toolchain's self-contained musl. Its libc.a
# holds musl's version string (__libc_version), which must match the musl
# entry in licenses/rust-overrides.json; a stable rustc that moves to a new
# musl fails here until that entry and licenses/rust/ are updated.
musl_version=
if [[ $TARGET == *musl* ]]; then
    libc="$(rustc --print sysroot)/lib/rustlib/$TARGET/lib/self-contained/libc.a"
    musl_version=$(grep -azxE '[0-9]+\.[0-9]+\.[0-9]+' "$libc" | tr '\0' '\n' | sort -u)
    [[ $musl_version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
fi

check() {
    test -s "$1/Third-Party-Notices.txt"
    test -s "$1/Rust-Licenses.html"
    grep -q '^tokio ' "$1/Third-Party-Notices.txt"
    grep -q '^colored ' "$1/Third-Party-Notices.txt"
    grep -q 'Copyright notices for The Rust Standard Library' "$1/Rust-Licenses.html"
    if [[ -n $musl_version ]]; then
        grep -qxF "musl libc $musl_version" "$1/Third-Party-Notices.txt"
    fi
}

mkdir notices-check
if [ "$OS_NAME" = windows-latest ]; then
    7z x -onotices-check ia-get-*-"$TARGET".zip
else
    tar -xzf ia-get-*-"$TARGET".tar.gz -C notices-check
fi
for dir in notices-check/ia-get-*-"$TARGET"; do
    grep -q 'Permission is hereby granted' "$dir/LICENSE"
    check "$dir"
done

if [[ $TARGET == *linux* ]]; then
    for deb in ia-get*.deb; do
        pkg=$(dpkg-deb -f "$deb" Package)
        doc="notices-check/$pkg/usr/share/doc/$pkg"
        dpkg-deb -x "$deb" "notices-check/$pkg"
        # The binary has its own paragraph, so "Files: *" (MIT) does not
        # cover the bundled crates, and that paragraph's license is defined.
        grep -qxF 'Files: usr/bin/ia-get' "$doc/copyright"
        grep -qxF 'License: MIT and LicenseRef-Bundled-Rust' "$doc/copyright"
        grep -qxF 'License: LicenseRef-Bundled-Rust' "$doc/copyright"
        check "$doc"
    done
fi
