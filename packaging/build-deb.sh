#!/bin/sh
# Builds a .deb of Grainhouse into target/deb/. Needs dpkg-dev (for dpkg-shlibdeps).
set -e
cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(dpkg --print-architecture)
ID=io.github.jegly.Grainhouse
cargo build --release
OUT=target/deb
NAME=grainhouse_${VERSION}_${ARCH}
ROOT=$OUT/$NAME
rm -rf "$OUT"
install -Dm755 target/release/grainhouse "$ROOT/usr/bin/grainhouse"
strip "$ROOT/usr/bin/grainhouse"
# the command-line tool is the same program under another name
ln -s grainhouse "$ROOT/usr/bin/grainhouse-cli"
install -Dm644 data/$ID.desktop "$ROOT/usr/share/applications/$ID.desktop"
install -Dm644 data/$ID.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/$ID.svg"
install -Dm644 data/$ID.metainfo.xml "$ROOT/usr/share/metainfo/$ID.metainfo.xml"
install -Dm644 LICENSE "$ROOT/usr/share/doc/grainhouse/copyright"
install -Dm644 NOTICE "$ROOT/usr/share/doc/grainhouse/NOTICE"
install -Dm644 README.md "$ROOT/usr/share/doc/grainhouse/README.md"

# the shared libraries the programs need, worked out by dpkg-shlibdeps
mkdir -p "$OUT/debian"
printf 'Source: grainhouse\n\nPackage: grainhouse\nArchitecture: any\n' > "$OUT/debian/control"
DEPS=$(cd "$OUT" && dpkg-shlibdeps -O "$NAME/usr/bin/grainhouse" 2>/dev/null | sed 's/^shlibs:Depends=//')
SIZE=$(du -sk "$ROOT/usr" | cut -f1)

mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<CONTROL
Package: grainhouse
Version: $VERSION
Architecture: $ARCH
Maintainer: jegly <jegly@users.noreply.github.com>
Installed-Size: $SIZE
Depends: $DEPS
Section: graphics
Priority: optional
Homepage: https://github.com/jegly/grainhouse
Description: Photo editor with film looks, AI effects and upscaling
 Grainhouse edits RAW files and JPEGs: white balance, tone curve, colour
 mixer and grading, crop, lens corrections, noise reduction and local
 adjustments with AI masks. It adds 153 film stocks, creative looks, your
 own .cube LUTs, camera looks, 30 effects (many using AI to find people),
 diffusion filters, film frames, and Real-ESRGAN upscaling on export.
CONTROL
dpkg-deb --build --root-owner-group "$ROOT" "$OUT/$NAME.deb"
echo "Built $OUT/$NAME.deb"
