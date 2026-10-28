#!/bin/sh
# Verify that the app in /Applications really is the build that was just made.
#
# Two failures in one session came from assuming otherwise: a build that failed
# behind a pipe (so the old binary stayed installed), and a build that succeeded
# without relinking (so the binary kept a stale embedded frontend). Both looked
# identical to success from the outside.
set -e
BUNDLE="src-tauri/target/release/bundle/macos/Hacksor.app/Contents/MacOS/hacksor"
INSTALLED="/Applications/Hacksor.app/Contents/MacOS/hacksor"
DIST=$(ls -t dist/assets/index-*.js 2>/dev/null | head -1)

[ -f "$BUNDLE" ] || { echo "no bundle built"; exit 1; }
[ -f "$INSTALLED" ] || { echo "not installed"; exit 1; }

# The binary must be NEWER than the frontend it is supposed to contain.
if [ -n "$DIST" ] && [ "$DIST" -nt "$BUNDLE" ]; then
  echo "STALE: $DIST is newer than the binary — the frontend was not re-embedded"
  exit 1
fi

A=$(shasum -a 256 "$BUNDLE" | cut -c1-16)
B=$(shasum -a 256 "$INSTALLED" | cut -c1-16)

# Signing REWRITES the installed binary, so after it the two hashes no longer
# match even though the install is correct. A marker recording which build was
# installed keeps the check honest; it is written before signing, because the
# signature seals Resources and a later write would break it.
MARK="/Applications/Hacksor.app/Contents/Resources/.hacksor-build"
SIGN=""
if [ "$A" = "$B" ]; then
  echo "OK: installed binary matches the build ($A)"
  SIGN=yes
elif [ -f "$MARK" ] && [ "$(cat "$MARK")" = "$A" ]; then
  echo "OK: installed build is $A (binary re-signed locally, so bytes differ)"
else
  echo "MISMATCH: built=$A installed=$B"
  exit 1
fi

# Re-sign with the stable local identity when one exists, so macOS keeps the
# file-access grants it was given instead of re-asking after every rebuild.
# Silent no-op until ./scripts-sign-local.sh has been run once.
if [ -n "$SIGN" ] && security find-certificate -c "Hacksor Local Signing" >/dev/null 2>&1; then
  printf '%s' "$A" > "$MARK"
  ./scripts-sign-local.sh /Applications/Hacksor.app >/dev/null 2>&1 \
    && echo "OK: re-signed with the local identity" \
    || echo "WARN: re-signing failed; macOS will ask for file access again"
fi
