#!/bin/bash
# Builds the release package on Ubuntu (WSL works) and writes release/walltablet.tar.gz and
# release/latest.json. Commit both: the tablets find the new version through latest.json.
# Needs on Ubuntu 26.04 (its own Rust is enough, see rust-version in Cargo.toml):
#   sudo apt-get install -y build-essential pkg-config cargo rustc libgtk-4-dev libwebkitgtk-6.0-dev \
#     libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstrtspserver-1.0-dev
set -euo pipefail
cd "$(dirname "$0")/.."
ver=$(cat VERSION)
cargo test --locked --workspace --release
cargo build --locked --workspace --release
bin=${CARGO_TARGET_DIR:-target}/release
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg=$stage/walltablet-$ver
mkdir -p "$pkg/bin" "$pkg/deploy" release
install -m 755 "$bin/wts-server" "$bin/wts-kiosk" "$bin/wts-camera" "$pkg/bin/"
install -m 644 deploy/*.service "$pkg/deploy/"
install -m 755 deploy/install.sh deploy/update.sh "$pkg/deploy/"
cp VERSION README.md "$pkg/"
tar --sort=name --owner=0 --group=0 --numeric-owner -C "$stage" -czf release/walltablet.tar.gz "walltablet-$ver"
sha=$(sha256sum release/walltablet.tar.gz | cut -d' ' -f1)
size=$(stat -c %s release/walltablet.tar.gz)
printf '{"name": "WallTabletSuite", "version": "%s", "file": "walltablet.tar.gz", "sha256": "%s", "size": %s, "built": "%s"}\n' \
  "$ver" "$sha" "$size" "$(date -u +%FT%TZ)" >release/latest.json
echo "release/walltablet.tar.gz: Version $ver, $size Bytes"
