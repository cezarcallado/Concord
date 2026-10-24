#!/bin/bash
# Build concord for macOS (Apple Silicon) and pack it for friends: concord.app (ad-hoc signed)
# plus LEIA-ME-MAC.txt (tools/LEIA-ME-MAC.txt) in dist/concord-<version>-mac-arm64.zip.
#
# Run ON the Mac, from anywhere inside the repository:   tools/package-mac.sh
#   -j N             cargo jobs (default 3: an 8 GB Mac builds without swapping)
#   --skip-build     pack the binary already in the target dir
# Needs: rustup's Rust (aarch64-apple-darwin) and the Command Line Tools (codesign, ditto, plutil).
# CARGO_TARGET_DIR is honoured (default: <repo>/target).
set -euo pipefail

JOBS=3
BUILD=1
while [ $# -gt 0 ]; do
  case "$1" in
    -j) JOBS="$2"; shift 2 ;;
    --skip-build) BUILD=0; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "run this on a Mac" >&2; exit 1; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

# The workspace version (`version = "x.y.z"` under [workspace.package]).
VERSION="$(awk '/^\[workspace.package\]/{f=1; next} /^\[/{f=0} f && /^version *=/{gsub(/[" \r]/, "", $0); split($0, a, "="); print a[2]; exit}' Cargo.toml)"
[ -n "$VERSION" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
echo "concord $VERSION for macOS (arm64)"

if [ "$BUILD" = 1 ]; then
  # A release goes to other people: keep the builder's home folder (their macOS user name) out of
  # the binary's panic/debug paths. RUSTFLAGS replaces .cargo/config.toml's rustflags, so -ObjC
  # (see there) must be repeated here.
  RUSTFLAGS="-C link-args=-ObjC --remap-path-prefix=$HOME=~ --remap-path-prefix=$ROOT=concord" \
    CARGO_INCREMENTAL=0 cargo build --release -p voxhub-app -j "$JOBS"
fi
BIN="$TARGET_DIR/release/concord"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }
[ "$(lipo -archs "$BIN" 2>/dev/null || echo arm64)" = "arm64" ] || echo "warning: $BIN is not arm64-only" >&2

OUT="$ROOT/dist"
STAGE="$OUT/concord-$VERSION-mac"
APP="$STAGE/concord.app"
rm -rf "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/concord"
cp "$ROOT/app/assets/logo/concord.icns" "$APP/Contents/Resources/concord.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>pt-BR</string>
	<key>CFBundleDisplayName</key>
	<string>concord</string>
	<key>CFBundleExecutable</key>
	<string>concord</string>
	<key>CFBundleIconFile</key>
	<string>concord</string>
	<key>CFBundleIdentifier</key>
	<string>app.concord.desktop</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>concord</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.social-networking</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>LSMultipleInstancesProhibited</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>O concord usa o microfone para a voz nas salas e chamadas.</string>
	<key>NSCameraUsageDescription</key>
	<string>O concord usa a câmera quando você liga a sua câmera numa sala.</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>O concord conversa com o servidor do seu grupo, que pode estar na sua rede de casa.</string>
	<key>NSScreenCaptureUsageDescription</key>
	<string>O concord grava a tela ou a janela que você escolher para transmitir aos seus amigos.</string>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" > /dev/null
printf 'APPL????' > "$APP/Contents/PkgInfo"

# The pt-BR read-me for friends (what to click the first time, the permissions, what works).
sed "s/@VERSION@/$VERSION/g" "$ROOT/tools/LEIA-ME-MAC.txt" > "$STAGE/LEIA-ME-MAC.txt"

# Ad-hoc signature (no Apple developer account): LEIA-ME-MAC explains the first open
# (Privacidade e Segurança > "Abrir Mesmo Assim", or xattr -dr com.apple.quarantine).
codesign --force --deep -s - "$APP"
codesign --verify --deep --strict "$APP"

ZIP="$OUT/concord-$VERSION-mac-arm64.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$STAGE" "$ZIP"
echo "ok: $ZIP ($(du -h "$ZIP" | cut -f1))"
