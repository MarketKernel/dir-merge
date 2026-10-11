#!/bin/sh
# Packs a Linux binary into a Debian/Ubuntu package.
# usage: build_scripts/linux-deb.sh [binary] [output-dir]
#   defaults: target/release/dir-merge and dist/
# Needs dpkg-deb and objdump (Debian/Ubuntu: dpkg, binutils).
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${1:-$root/target/release/dir-merge}"
out="${2:-$root/dist}"
if [ ! -f "$bin" ]; then
    echo "error: binary not found: $bin" >&2
    echo "build it first (cargo build --release) or use build_scripts/build.sh" >&2
    exit 1
fi
mkdir -p "$out"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
arch="$(dpkg --print-architecture)"
# The newest glibc symbol the binary uses is the oldest libc6 it runs on.
glibc="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"

pkg="$(mktemp -d)/dir-merge"
install -Dm755 "$bin" "$pkg/usr/bin/dir-merge"
install -Dm644 "$root/assets/dir-merge.desktop" "$pkg/usr/share/applications/dir-merge.desktop"
install -Dm644 "$root/assets/icon-256.png" "$pkg/usr/share/icons/hicolor/256x256/apps/dir-merge.png"
install -Dm644 "$root/LICENSE" "$pkg/usr/share/doc/dir-merge/copyright"

# X11, Wayland, Vulkan and OpenGL are loaded at run time (dlopen), so they are not in the
# binary's NEEDED list; any desktop already has them, the dependencies cover minimal installs.
mkdir -p "$pkg/DEBIAN"
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: dir-merge
Version: $version
Architecture: $arch
Maintainer: MarketKernel <https://github.com/MarketKernel/dir-merge>
Installed-Size: $(du -sk "$pkg/usr" | cut -f1)
Depends: libc6 (>= $glibc), libgcc-s1 | libgcc1, libxkbcommon0, libvulkan1 | libegl1 | libgl1
Recommends: libx11-6, libxcursor1, libxrandr2, libxi6, libxkbcommon-x11-0, libwayland-client0, xdg-desktop-portal
Section: devel
Homepage: https://github.com/MarketKernel/dir-merge
Priority: optional
Description: Compare and merge files and folders
 Side-by-side text diff with inline highlighting and merging, and
 recursive folder comparison with copy, delete and sync actions.
CONTROL

deb="$out/dir-merge_${version}_${arch}.deb"
dpkg-deb --root-owner-group --build "$pkg" "$deb" >/dev/null
rm -rf "$(dirname "$pkg")"
echo "$deb"
