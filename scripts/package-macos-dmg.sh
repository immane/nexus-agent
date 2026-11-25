#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:-"$ROOT/dist"}
APP="$OUT/Nexus Agent.app"
DMG="$OUT/Nexus-Agent-macos-arm64.dmg"
TARGET=aarch64-apple-darwin
BUILD="$ROOT/target/macos-app/$TARGET/release"
WEB_ICON="$ROOT/crates/nexus-server/web/icon-512.png"
VERSION=$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT/crates/nexus-tui/Cargo.toml" | sed 's/-.*//')
SWIFTC=$(xcrun --find swiftc)

case "$(uname -m)" in
  arm64) ;;
  *) echo "This first macOS package script targets Apple Silicon (arm64)." >&2; exit 2 ;;
esac

mkdir -p "$OUT"
if [ -e "$APP" ] || [ -e "$DMG" ]; then
  echo "Refusing to overwrite an existing app or DMG in $OUT." >&2
  exit 2
fi

cargo build --release --locked --target "$TARGET" --target-dir "$ROOT/target/macos-app" \
  -p nexus-server --features nexus-server/container --bins

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/packaging/macos/Info.plist" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP/Contents/Info.plist"
cp "$BUILD/nexus-server" "$APP/Contents/Resources/nexus-server"
chmod 755 "$APP/Contents/Resources/nexus-server"
"$SWIFTC" -parse-as-library -O -framework AppKit -framework Security -framework UniformTypeIdentifiers \
  "$ROOT/packaging/macos/NexusAgent/main.swift" -o "$APP/Contents/MacOS/Nexus Agent"
chmod 755 "$APP/Contents/MacOS/Nexus Agent"

STAGING=$(mktemp -d "${TMPDIR:-/private/var/folders}/nexus-dmg.XXXXXX")
trap 'rm -rf "$STAGING"' EXIT HUP INT TERM
ICONSET="$STAGING/NexusAgent.iconset"
mkdir "$ICONSET"
for SIZE in 16 32 128 256 512; do
  DOUBLE=$((SIZE * 2))
  sips -z "$SIZE" "$SIZE" "$WEB_ICON" --out "$ICONSET/icon_${SIZE}x${SIZE}.png" >/dev/null
  sips -z "$DOUBLE" "$DOUBLE" "$WEB_ICON" --out "$ICONSET/icon_${SIZE}x${SIZE}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/NexusAgent.icns"

mkdir "$STAGING/disk"
cp -R "$APP" "$STAGING/disk/"
ln -s /Applications "$STAGING/disk/Applications"
hdiutil create -volname "Nexus Agent" -srcfolder "$STAGING/disk" -ov -format UDZO "$DMG"
echo "Created $DMG"
