#!/bin/sh
# Builds a snap of Grainhouse into target/snap/ with `snap pack` (no snapcraft, LXD or root needed).
# It uses the gnome-core26 runtime and the mesa-2604 graphics snap, which snapd installs with it.
# Install with: sudo snap install --dangerous target/snap/grainhouse_<version>_amd64.snap
set -e
cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ID=io.github.jegly.Grainhouse
cargo build --release
P=target/snap/prime
rm -rf target/snap
install -Dm755 target/release/grainhouse "$P/usr/bin/grainhouse"
strip "$P/usr/bin/grainhouse"
ln -s grainhouse "$P/usr/bin/grainhouse-cli"
install -Dm644 data/$ID.svg "$P/usr/share/icons/hicolor/scalable/apps/$ID.svg"
install -Dm644 data/$ID.metainfo.xml "$P/usr/share/metainfo/$ID.metainfo.xml"
install -Dm644 data/$ID.svg "$P/meta/gui/$ID.svg"
# the desktop entry, pointing at the snap's command and icon
sed -e 's|^Exec=.*|Exec=grainhouse %F|' -e "s|^Icon=.*|Icon=\${SNAP}/meta/gui/$ID.svg|" data/$ID.desktop > "$P/meta/gui/grainhouse.desktop"
install -Dm755 packaging/snap/desktop-launch "$P/snap/command-chain/desktop-launch"
install -Dm755 packaging/snap/gpu-2604-wrapper "$P/snap/command-chain/gpu-2604-wrapper"
# a fallback launcher, from an installed GNOME runtime snap if there is one
for r in /snap/gnome-core26/current /snap/gnome-46-2404/current; do
    if [ -x "$r/command-chain/desktop-launch" ]; then
        install -Dm755 "$r/command-chain/desktop-launch" "$P/snap/command-chain/desktop-launch-bundled"
        break
    fi
done
[ -x "$P/snap/command-chain/desktop-launch-bundled" ] || { echo "no GNOME runtime snap to take desktop-launch from"; exit 1; }
# layout sources must exist when packing; the runtime is mounted over them
mkdir -p "$P/gnome-platform/usr/libexec/glycin-loaders" "$P/gnome-platform/usr/share/xml/iso-codes"
mkdir -p "$P/gnome-platform" "$P/gpu-2604" "$P/data-dir/themes" "$P/data-dir/icons" "$P/data-dir/sounds"
sed "s/@VERSION@/$VERSION/" packaging/snap/snap.yaml.in > "$P/meta/snap.yaml"
snap pack "$P" target/snap/
ls -la target/snap/*.snap
