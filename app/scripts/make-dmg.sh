#!/usr/bin/env bash
#
# Build a macOS .dmg from an already-bundled .app.
#
# Tauri ships its own `bundle_dmg.sh`, but that script drives Finder over
# AppleScript to position icons and set a background picture. Finder is a GUI
# process: over ssh, in CI, or under an automation harness it never answers, and
# the build dies with `AppleEvent timed out (-1712)` *after* the application has
# already been built successfully. Its own `--skip-jenkins` flag exists for
# exactly this case, but Tauri does not pass it and it cannot be set by
# environment.
#
# So this does the job with `hdiutil` alone. What is lost is only cosmetic — a
# custom background image and hand-placed icon coordinates. What is kept is
# everything that makes a .dmg work: the app, a drag-to-Applications target,
# compression, and a signature.
set -euo pipefail

APP="${1:?usage: make-dmg.sh <path-to-.app> <output.dmg> [volume name]}"
OUT="${2:?usage: make-dmg.sh <path-to-.app> <output.dmg> [volume name]}"
VOLNAME="${3:-$(basename "${APP%.app}")}"

if [[ ! -d "$APP" ]]; then
    echo "error: no such app bundle: $APP" >&2
    exit 1
fi

STAGING="$(mktemp -d)"
# `hdiutil` will not overwrite, and a stale mount from a previous failed run
# would silently poison the next one.
cleanup() {
    rm -rf "$STAGING"
}
trap cleanup EXIT

echo "  staging $(basename "$APP")"
# -R preserves the symlinks and the code signature inside the bundle; plain -r
# would flatten them and invalidate the signature.
cp -R "$APP" "$STAGING/"

# The whole user interaction a .dmg needs: drag the app onto the alias.
ln -s /Applications "$STAGING/Applications"

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"

echo "  creating $(basename "$OUT")"
# UDZO is the compressed read-only format every installer uses. `-quiet` keeps
# the build log readable; failures still surface through `set -e`.
hdiutil create \
    -quiet \
    -volname "$VOLNAME" \
    -srcfolder "$STAGING" \
    -ov \
    -format UDZO \
    "$OUT"

# Ad-hoc signature, matching what Tauri applies to the .app. Without any
# signature at all, Gatekeeper is noisier than it needs to be. A real identity
# can be supplied through APPLE_SIGNING_IDENTITY.
IDENTITY="${APPLE_SIGNING_IDENTITY:--}"
if command -v codesign >/dev/null 2>&1; then
    codesign --force --sign "$IDENTITY" "$OUT" 2>/dev/null \
        || echo "  note: could not sign the disk image (continuing)"
fi

SIZE=$(du -h "$OUT" | cut -f1 | tr -d ' ')
echo "  built $OUT ($SIZE)"
