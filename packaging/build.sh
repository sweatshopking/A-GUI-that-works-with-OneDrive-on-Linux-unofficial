#!/usr/bin/env bash
# Builds release packages. Run inside the matching build environment:
#
#   packaging/build.sh deb-appimage   on Ubuntu 22.04  -> dist/*.deb, dist/*.AppImage
#   packaging/build.sh rpm            on Fedora 41     -> dist/*.rpm
#
# Packages are built for the machine it runs on: x86_64 (amd64) or aarch64 (arm64).
#
# packaging/build-in-containers.sh runs both with podman or docker; the GitHub workflow uses
# the same targets. Every package bundles its own copy of the OneDrive Client for Linux
# (abraunegg/onedrive, GPL-3.0), built from the tagged release source below.
set -euo pipefail

ONEDRIVE_TAG="${ONEDRIVE_TAG:-v2.5.11}"
LINUXDEPLOY_TAG="${LINUXDEPLOY_TAG:-1-alpha-20251107-1}"
# the official LDC release ships a static D runtime on every distro (Fedora's package doesn't)
LDC_VERSION="${LDC_VERSION:-1.43.0}"
APP=linux-onedrive-gui
APPSTREAM_ID=io.github.sweatshopking.LinuxOnedriveGui

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${CARGO_TARGET_DIR:-$ROOT/target}/package-work"
DIST="$ROOT/dist"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET/release/$APP"
ARCH="$(uname -m)" # x86_64 or aarch64: the names LDC, linuxdeploy, AppImage and rpm use
case "$ARCH" in
    x86_64) DEB_ARCH=amd64 ;;
    aarch64) DEB_ARCH=arm64 ;;
    *) echo "error: unsupported architecture $ARCH" >&2; exit 1 ;;
esac
mkdir -p "$WORK" "$DIST"

log() { printf '\n==> %s\n' "$*"; }

install_deps_ubuntu() {
    log "Installing build dependencies (apt)"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
        ca-certificates curl git make gcc g++ pkg-config file dpkg-dev \
        xz-utils libcurl4-openssl-dev libsqlite3-dev libdbus-1-dev zlib1g-dev \
        libfontconfig1-dev libfreetype-dev libxkbcommon-dev libwayland-dev \
        libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev libegl1-mesa-dev \
        desktop-file-utils appstream >/dev/null
}

install_deps_fedora() {
    log "Installing build dependencies (dnf)"
    dnf install -y -q \
        ca-certificates curl git make gcc gcc-c++ pkgconf-pkg-config file rpm-build \
        xz libcurl-devel sqlite-devel dbus-devel zlib-devel \
        fontconfig-devel freetype-devel libxkbcommon-devel wayland-devel \
        libX11-devel libXcursor-devel libXrandr-devel libXi-devel mesa-libGL-devel mesa-libEGL-devel \
        desktop-file-utils >/dev/null
}

install_rust() {
    if ! command -v cargo >/dev/null; then
        log "Installing Rust"
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
    fi
    # shellcheck disable=SC1091
    source "${CARGO_HOME:-$HOME/.cargo}/env"
}

install_ldc() {
    local dir="$WORK/ldc2-$LDC_VERSION-linux-$ARCH"
    if [ ! -x "$dir/bin/ldmd2" ]; then
        log "Installing LDC $LDC_VERSION"
        curl -fsSL "https://github.com/ldc-developers/ldc/releases/download/v$LDC_VERSION/ldc2-$LDC_VERSION-linux-$ARCH.tar.xz" | tar -xJ -C "$WORK"
    fi
    export PATH="$dir/bin:$PATH"
}

build_client() {
    install_ldc
    log "Building the OneDrive Client for Linux $ONEDRIVE_TAG"
    rm -rf "$WORK/onedrive"
    git clone -q -c advice.detachedHead=false --depth 1 --branch "$ONEDRIVE_TAG" https://github.com/abraunegg/onedrive "$WORK/onedrive"
    cd "$WORK/onedrive"
    ./configure DC=ldmd2 --with-systemdsystemunitdir=no --with-systemduserunitdir=no >/dev/null
    # link the D runtime statically so users don't need it installed
    make -j"$(nproc)" DC="ldmd2 -link-defaultlib-shared=false -L-lz" >"$WORK/onedrive-build.log" 2>&1 || { tail -40 "$WORK/onedrive-build.log"; exit 1; }
    if ldd onedrive | grep -qE 'phobos|druntime'; then
        echo "error: the client still links the D runtime dynamically" >&2
        exit 1
    fi
    ./onedrive --version
    cd "$ROOT"
}

build_app() {
    log "Building $APP $VERSION"
    cargo build --release --locked
}

# Lays out the installed file tree shared by the .deb and .rpm.
stage_tree() {
    local dest="$1"
    rm -rf "$dest"
    install -Dm755 "$BIN" "$dest/usr/bin/$APP"
    install -Dm755 "$WORK/onedrive/onedrive" "$dest/usr/lib/$APP/onedrive"
    install -Dm644 "$ROOT/packaging/$APP.desktop" "$dest/usr/share/applications/$APP.desktop"
    install -Dm644 "$ROOT/packaging/$APPSTREAM_ID.metainfo.xml" "$dest/usr/share/metainfo/$APPSTREAM_ID.metainfo.xml"
    install -Dm644 "$ROOT/assets/$APP.svg" "$dest/usr/share/icons/hicolor/scalable/apps/$APP.svg"
    install -Dm644 "$ROOT/assets/$APP.png" "$dest/usr/share/icons/hicolor/256x256/apps/$APP.png"
    for f in "$ROOT"/assets/icons/hicolor/scalable/status/*.svg; do
        install -Dm644 "$f" "$dest/usr/share/icons/hicolor/scalable/status/$(basename "$f")"
    done
    install -Dm644 "$ROOT/packaging/$APP-servicemenu.desktop" "$dest/usr/share/kio/servicemenus/$APP.desktop"
    chmod 755 "$dest/usr/share/kio/servicemenus/$APP.desktop"
    install -Dm644 "$ROOT/LICENSE" "$dest/usr/share/licenses/$APP/LICENSE"
    install -Dm644 "$WORK/onedrive/LICENSE" "$dest/usr/share/licenses/$APP/onedrive-LICENSE"
    install -Dm644 "$ROOT/packaging/THIRD-PARTY.md" "$dest/usr/share/doc/$APP/THIRD-PARTY.md"
}

build_deb() {
    log "Packaging .deb"
    local stage="$WORK/deb"
    stage_tree "$stage"
    mkdir -p "$stage/DEBIAN" "$WORK/debian"
    # dpkg-shlibdeps needs a debian/control to exist; it only reads the package name from it
    printf 'Source: %s\n\nPackage: %s\nArchitecture: %s\n' "$APP" "$APP" "$DEB_ARCH" >"$WORK/debian/control"
    local linked
    linked="$(cd "$WORK" && dpkg-shlibdeps -O "$stage/usr/bin/$APP" "$stage/usr/lib/$APP/onedrive" 2>/dev/null | sed 's/^shlibs:Depends=//')"
    # libraries the window opens at runtime (dlopen), which shlibdeps can't see
    local runtime="libxkbcommon0, libegl1, libwayland-client0, libwayland-egl1, libx11-6, libxcursor1, libxrandr2, libxi6, systemd, xdg-utils"
    cat >"$stage/DEBIAN/control" <<EOF
Package: $APP
Version: $VERSION
Section: net
Priority: optional
Architecture: $DEB_ARCH
Maintainer: sweatshopking <336541686+sweatshopking@users.noreply.github.com>
Depends: $linked, $runtime
Recommends: kdialog | zenity
Homepage: https://github.com/sweatshopking/A-GUI-that-works-with-OneDrive-on-Linux-unofficial
Description: OneDrive and SharePoint sync with a tray icon and settings window
 A friendly front end for the OneDrive Client for Linux (abraunegg/onedrive),
 which is bundled. Sign in, choose folders, set sync times, pause, see what
 synced and share links, without using the command line.
EOF
    dpkg-deb --root-owner-group --build "$stage" "$DIST/${APP}_${VERSION}_${DEB_ARCH}.deb" >/dev/null
    dpkg-deb --info "$DIST/${APP}_${VERSION}_${DEB_ARCH}.deb" | sed -n '/Depends/p'
}

# Lists the distribution packages (and exact versions) that the libraries copied into an AppImage
# came from, so anyone can get the matching source, as their (L)GPL licenses ask.
bundled_sources() {
    local appdir="$1" lib pkg distro
    # read only the name: sourcing os-release would overwrite our own VERSION
    distro="$(. /etc/os-release && echo "$PRETTY_NAME")"
    echo "Libraries copied unmodified from $distro into this AppImage."
    echo "Their copyright and license files are in usr/share/doc/<package>/ inside the AppImage."
    echo "Source for each is available from https://launchpad.net/ubuntu/+source/<source>/<version>"
    echo "or with: apt-get source <source>=<version>"
    echo
    echo "source version"
    for lib in "$appdir"/usr/lib/*.so*; do
        pkg="$(dpkg -S "*/$(basename "$lib")" 2>/dev/null | head -1 | cut -d: -f1)" || true
        [ -n "$pkg" ] && dpkg-query -W -f='${source:Package} ${source:Version}\n' "$pkg"  || true
    done | sort -u
}

build_appimage() {
    log "Packaging AppImage"
    local appdir="$WORK/AppDir"
    rm -rf "$appdir"
    mkdir -p "$appdir/usr/share/icons/hicolor/scalable/status"
    cp "$ROOT"/assets/icons/hicolor/scalable/status/*.svg "$appdir/usr/share/icons/hicolor/scalable/status/"
    install -Dm644 "$ROOT/packaging/$APPSTREAM_ID.metainfo.xml" "$appdir/usr/share/metainfo/$APPSTREAM_ID.metainfo.xml"
    install -Dm644 "$ROOT/LICENSE" "$appdir/usr/share/licenses/$APP/LICENSE"
    install -Dm644 "$WORK/onedrive/LICENSE" "$appdir/usr/share/licenses/$APP/onedrive-LICENSE"
    install -Dm644 "$ROOT/packaging/THIRD-PARTY.md" "$appdir/usr/share/doc/$APP/THIRD-PARTY.md"
    local ld="$WORK/linuxdeploy-$ARCH.AppImage"
    [ -x "$ld" ] || {
        curl -fsSL -o "$ld" "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_TAG/linuxdeploy-$ARCH.AppImage"
        chmod +x "$ld"
    }
    # containers have no FUSE, so let the tool unpack itself
    export APPIMAGE_EXTRACT_AND_RUN=1
    (
        cd "$WORK"
        LDAI_OUTPUT="$DIST/A_GUI_that_works_with_OneDrive_on_Linux-unofficial-${VERSION}-${ARCH}.AppImage" \
            "$ld" --appdir "$appdir" \
            --executable "$BIN" \
            --executable "$WORK/onedrive/onedrive" \
            --desktop-file "$ROOT/packaging/$APP.desktop" \
            --icon-file "$ROOT/assets/$APP.svg" >"$WORK/linuxdeploy.log" 2>&1 || { tail -40 "$WORK/linuxdeploy.log"; exit 1; }
        bundled_sources "$appdir" >"$appdir/usr/share/licenses/$APP/BUNDLED-LIBRARIES.txt"
        LDAI_OUTPUT="$DIST/A_GUI_that_works_with_OneDrive_on_Linux-unofficial-${VERSION}-${ARCH}.AppImage" \
            "$ld" --appdir "$appdir" --output appimage >>"$WORK/linuxdeploy.log" 2>&1 || { tail -40 "$WORK/linuxdeploy.log"; exit 1; }
    )
    ls -la "$DIST"/*-"$ARCH".AppImage
}

build_rpm() {
    log "Packaging .rpm"
    local stage="$WORK/rpm-root"
    stage_tree "$stage"
    local top="$WORK/rpmbuild"
    rm -rf "$top"
    mkdir -p "$top"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
    sed -e "s/@VERSION@/$VERSION/" "$ROOT/packaging/$APP.spec.in" >"$top/SPECS/$APP.spec"
    rpmbuild -bb --quiet \
        --define "_topdir $top" \
        --define "stage $stage" \
        "$top/SPECS/$APP.spec"
    cp "$top"/RPMS/"$ARCH"/*.rpm "$DIST/"
    rpm -qp --requires "$DIST"/*."$ARCH".rpm | grep -v '^rpmlib' | tr '\n' ' '
    echo
}

case "${1:-}" in
    deb-appimage)
        [ "${SKIP_DEPS:-}" = 1 ] || install_deps_ubuntu
        install_rust
        build_client
        build_app
        build_deb
        build_appimage
        ;;
    rpm)
        [ "${SKIP_DEPS:-}" = 1 ] || install_deps_fedora
        install_rust
        build_client
        build_app
        build_rpm
        ;;
    *)
        echo "usage: $0 deb-appimage|rpm" >&2
        exit 2
        ;;
esac
log "Done: $(ls "$DIST")"
