#!/usr/bin/env bash
# Building and packaging for release

set -ex

# Write the notices of the crates statically linked into $TARGET's binary
# and of the Rust standard library into $1. rust-notices.py fails when a
# crate has no license text; gaps go in licenses/rust-overrides.json.
notices() {
    local python=python3

    # Windows runners have python but no python3
    if [ "$OS_NAME" = windows-latest ]; then
        python=python
    fi
    "$python" scripts/rust-notices.py --target "$TARGET" --title "$PROJECT_NAME" \
        --overrides licenses/rust-overrides.json --own-license MIT \
        --out "$1/Third-Party-Notices.txt" --rust-std "$1/Rust-Licenses.html"
    test -s "$1/Third-Party-Notices.txt"
    test -s "$1/Rust-Licenses.html"
}

pack() {
    local tempdir
    local out_dir
    local package_name
    local gcc_prefix

    tempdir=$(mktemp -d 2>/dev/null || mktemp -d -t tmp)
    out_dir=$(pwd)
    package_name="$PROJECT_NAME-${GITHUB_REF/refs\/tags\//}-$TARGET"

    if [[ $TARGET == "arm-unknown-linux-gnueabihf" ]]; then
        gcc_prefix="arm-linux-gnueabihf-"
    elif [[ $TARGET == "aarch64-unknown-linux-gnu" ]]; then
        gcc_prefix="aarch64-linux-gnu-"
    else
        gcc_prefix=""
    fi

    # create a "staging" directory
    mkdir "$tempdir/$package_name"

    # copying the main binary
    cp "target/$TARGET/release/$PROJECT_NAME" "$tempdir/$package_name/"
    if [ "$OS_NAME" != windows-latest ]; then
        "${gcc_prefix}"strip "$tempdir/$package_name/$PROJECT_NAME"
    fi

    # readme, license and third-party notices
    cp README.md "$tempdir/$package_name"
    cp LICENSE "$tempdir/$package_name"
    cp "$notices_dir/Third-Party-Notices.txt" "$notices_dir/Rust-Licenses.html" "$tempdir/$package_name"

    # archiving
    pushd "$tempdir"
    if [ "$OS_NAME" = windows-latest ]; then
        7z a "$out_dir/$package_name.zip" "$package_name"/*
    else
        tar czf "$out_dir/$package_name.tar.gz" "$package_name"/*
    fi
    popd
    rm -r "$tempdir"
}

make_deb() {
    local tempdir
    local architecture
    local version
    local dpkgname
    local conflictname
    local gcc_prefix
    local homepage
    local maintainer

    homepage="https://github.com/wimpysworld/ia-get"
    maintainer="Martin Wimpress <code@wimpress.io>"
    copyright_years="2023 - "$(date "+%Y")

    case $TARGET in
        x86_64*)
            architecture=amd64
            gcc_prefix=""
            library_dir=""
            ;;
        i686*)
            architecture=i386
            gcc_prefix=""
            library_dir=""
            ;;
        aarch64*)
            architecture=arm64
            gcc_prefix="aarch64-linux-gnu-"
            library_dir="-l/usr/aarch64-linux-gnu/lib"
            ;;
        arm*hf)
            architecture=armhf
            gcc_prefix="arm-linux-gnueabihf-"
            library_dir="-l/usr/arm-linux-gnueabihf/lib"
            ;;
        *)
            echo "make_deb: skipping target '${TARGET}'" >&2
            return 0
            ;;
    esac
    version=${GITHUB_REF/refs\/tags\//}

    if [[ $TARGET = *musl* ]]; then
      dpkgname=$PACKAGE_NAME-musl
      conflictname=$PROJECT_NAME
    else
      dpkgname=$PACKAGE_NAME
      conflictname=$PROJECT_NAME-musl
    fi

    tempdir=$(mktemp -d 2>/dev/null || mktemp -d -t tmp)

    # copy the main binary
    install -Dm755 "target/$TARGET/release/$PROJECT_NAME" "$tempdir/usr/bin/$PROJECT_NAME"
    "${gcc_prefix}"strip "$tempdir/usr/bin/$PROJECT_NAME"

    # Work out shared library dependencies
    # dpkg-shlibdeps requires debian/control file. Dummy it and clean up
    mkdir "./debian"
    touch "./debian/control"
    depends="$(dpkg-shlibdeps $library_dir -O "$tempdir/usr/bin/$PROJECT_NAME" 2> /dev/null | sed 's/^shlibs:Depends=//')"
    rm -rf "./debian"

    # readme, license and third-party notices
    install -Dm644 README.md "$tempdir/usr/share/doc/$dpkgname/README.md"
    install -Dm644 "$notices_dir/Third-Party-Notices.txt" "$tempdir/usr/share/doc/$dpkgname/Third-Party-Notices.txt"
    install -Dm644 "$notices_dir/Rust-Licenses.html" "$tempdir/usr/share/doc/$dpkgname/Rust-Licenses.html"
    # The binary paragraph keeps "Files: *" from claiming the statically
    # linked crates and Rust standard library are MIT.
    cat > "$tempdir/usr/share/doc/$dpkgname/copyright" <<EOF
Format: http://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: $PROJECT_NAME
Source: $homepage
Comment: usr/bin/$PROJECT_NAME statically links Rust crates and the Rust
 standard library, which keep their own licenses. Their license texts and
 notices are in Third-Party-Notices.txt and Rust-Licenses.html, next to
 this file.

Files: *
Copyright: $copyright_years $maintainer
License: MIT

Files: usr/bin/$PROJECT_NAME
Copyright: $copyright_years $maintainer
 The authors of each Rust crate, as listed in Third-Party-Notices.txt
 The Rust Project Developers and the authors listed in Rust-Licenses.html
License: MIT and LicenseRef-Bundled-Rust
Comment: Besides ia-get's own code (MIT), the binary contains the Rust
 crates listed in Third-Party-Notices.txt (MIT, Apache-2.0, ISC,
 BSD-3-Clause, Unicode-3.0, CDLA-Permissive-2.0, MPL-2.0 and others) and
 the Rust standard library (Rust-Licenses.html). In the $PACKAGE_NAME-musl
 package it also contains musl libc (MIT), listed in Third-Party-Notices.txt.
 MPL-2.0 crates such as colored are used unmodified; Third-Party-Notices.txt
 gives the location of their Source Code Form.

License: MIT
 Permission is hereby granted, free of charge, to any
 person obtaining a copy of this software and associated
 documentation files (the "Software"), to deal in the
 Software without restriction, including without
 limitation the rights to use, copy, modify, merge,
 publish, distribute, sublicense, and/or sell copies of
 the Software, and to permit persons to whom the Software
 is furnished to do so, subject to the following
 conditions:
 .
 The above copyright notice and this permission notice
 shall be included in all copies or substantial portions
 of the Software.
 .
 THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
 ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
 TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
 PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
 SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
 CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
 OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
 IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 DEALINGS IN THE SOFTWARE.

License: LicenseRef-Bundled-Rust
 Each statically linked component keeps its own license. Their full
 license texts, copyright notices and source links are in
 Third-Party-Notices.txt and Rust-Licenses.html, next to this file.
EOF
    chmod 644 "$tempdir/usr/share/doc/$dpkgname/copyright"

    # Control file
    mkdir "$tempdir/DEBIAN"
    cat > "$tempdir/DEBIAN/control" <<EOF
Package: $dpkgname
Version: $version
Section: utils
Priority: optional
Maintainer: Martin Wimpress <code@wimpress.io>
Architecture: $architecture
Depends: $depends
Conflicts: $conflictname
Description: File downloader for archive.org
 Pass the URL of an archive.org details page you want to download and ia-get
 will automatically get the XML metadata and download all files to the current
 working directory.
EOF

    fakeroot dpkg-deb -Zgzip --build "$tempdir" "${dpkgname}_${version}_${architecture}.deb"
}


main() {
    # generated once, shipped in the archive and the .deb
    notices_dir=$(mktemp -d 2>/dev/null || mktemp -d -t tmp)
    notices "$notices_dir"

    pack
    if [[ $TARGET = *linux* ]]; then
      make_deb
    fi
    rm -r "$notices_dir"
}

main
