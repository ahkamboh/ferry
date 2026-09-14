#!/usr/bin/env bash
# Build Ferry.app and install it to ~/Applications.
#   ./build.sh                      -> ~/Applications/Ferry.app
#   ./build.sh /path/to/Ferry.app   -> somewhere else
#   ./build.sh --zip                -> also writes the release zip next to the repo
#
# The binary is universal: one file carrying arm64 and x86_64, so it runs on an
# Intel Mac as well. 1.5.3 shipped arm64 only, which left Intel out.
# Windows has its own script, build-windows.ps1.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
ZIP=0
ARGS=()
for a in "$@"; do
  case "$a" in
    --zip) ZIP=1 ;;
    *) ARGS+=("$a") ;;
  esac
done
APP="${ARGS[0]:-$HOME/Applications/Ferry.app}"

command -v cargo >/dev/null || { echo "Rust is required: https://rustup.rs"; exit 1; }
command -v lipo  >/dev/null || { echo "lipo is required: xcode-select --install"; exit 1; }

VER=$(awk -F'"' '/^version = /{print $2; exit}' "$ROOT/src-tauri/Cargo.toml")
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

# rustup only ships the host's std, so the second one has to be asked for. Both
# are needed before lipo has two halves to join.
for t in "${TARGETS[@]}"; do
  rustup target list --installed 2>/dev/null | grep -qx "$t" || {
    echo "-> adding target $t"; rustup target add "$t"; }
done

for t in "${TARGETS[@]}"; do
  echo "-> building $VER for $t"
  cargo build --release --target "$t" --manifest-path "$ROOT/src-tauri/Cargo.toml"
done

echo "-> bundling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# One binary out of the two. Signing has to come after this: lipo rewrites the
# Mach-O header, so a signature made on either half does not survive the join.
lipo -create -output "$APP/Contents/MacOS/Ferry" \
  "$ROOT/src-tauri/target/aarch64-apple-darwin/release/ferry" \
  "$ROOT/src-tauri/target/x86_64-apple-darwin/release/ferry"
cp "$ROOT/src-tauri/icons/icon.icns" "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Ferry</string>
  <key>CFBundleDisplayName</key><string>Ferry</string>
  <key>CFBundleExecutable</key><string>Ferry</string>
  <key>CFBundleIdentifier</key><string>dev.ahkamboh.ferry</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST

codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

# Say out loud what came out: a build that quietly dropped one half looks exactly
# like a build that did not. Both checks read a variable rather than a pipe,
# because grep -q closes the pipe on its first match and pipefail then reports the
# writer's SIGPIPE as a failed check.
ARCHS=$(lipo -archs "$APP/Contents/MacOS/Ferry")
echo "-> archs: $ARCHS"
for want in arm64 x86_64; do
  case " $ARCHS " in *" $want "*) ;; *) echo "!! $want missing, not universal"; exit 1 ;; esac
done
SIG=$(codesign -dv "$APP" 2>&1 || true)
case "$SIG" in *adhoc*) echo "-> signature: adhoc" ;;
               *) echo "!! not ad hoc signed"; exit 1 ;; esac

echo "-> done: $APP  ($(du -sh "$APP" | cut -f1))"

if [ "$ZIP" = 1 ]; then
  Z="$ROOT/../Ferry-$VER-macos-universal.zip"
  rm -f "$Z"
  # ditto, not zip: it keeps the resource forks and the signature intact.
  (cd "$(dirname "$APP")" && ditto -c -k --sequesterRsrc --keepParent "$(basename "$APP")" "$Z")
  echo "-> zip : $Z  ($(du -h "$Z" | cut -f1))"
fi
echo "   open \"$APP\""
