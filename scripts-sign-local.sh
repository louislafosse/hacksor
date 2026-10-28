#!/bin/bash
# Give local Hacksor builds a STABLE code signature, so macOS stops asking for
# Desktop/Documents access after every rebuild.
#
# Why it asks every time: a local build is ad-hoc signed, and an ad-hoc
# signature is derived from the binary's contents. Every rebuild therefore
# produces a different identity, and TCC — which remembers permission against
# the signing identity — sees an app it has never met. Signing with one
# self-signed certificate that is reused gives TCC something constant to
# remember.
#
# This is for LOCAL builds only. Released builds are signed in CI and are not
# affected. Run it once to create the certificate; the install step calls it
# again on each rebuild to re-sign.
#
#   ./scripts-sign-local.sh                 # sign /Applications/Hacksor.app
#   ./scripts-sign-local.sh path/to/App.app # sign somewhere else
#
# To undo: delete "Hacksor Local Signing" from Keychain Access (login
# keychain), then `tccutil reset All dev.hacksor.app`.
set -euo pipefail

IDENTITY="Hacksor Local Signing"
BUNDLE_ID="dev.hacksor.app"
APP="${1:-/Applications/Hacksor.app}"

if [ ! -d "$APP" ]; then
  echo "no app bundle at $APP" >&2
  exit 1
fi

# 1. The certificate, created once and reused for every later build.
if ! security find-identity -v -p codesigning 2>/dev/null | grep -qF "$IDENTITY"; then
  echo "Creating the signing certificate (one time)…"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  cat > "$tmp/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = $IDENTITY
O  = Hacksor
[v3]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$tmp/k.pem" -out "$tmp/c.pem" -config "$tmp/cert.cnf" 2>/dev/null
  openssl pkcs12 -export -out "$tmp/i.p12" -inkey "$tmp/k.pem" -in "$tmp/c.pem" \
    -passout pass: -name "$IDENTITY" 2>/dev/null
  # -T lets codesign use the key without prompting for the keychain password
  # on every build; the key never leaves the login keychain.
  security import "$tmp/i.p12" -k "$HOME/Library/Keychains/login.keychain-db" \
    -P "" -T /usr/bin/codesign -T /usr/bin/security
  # Without this, macOS still prompts the first time codesign reads the key.
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
    -k "" "$HOME/Library/Keychains/login.keychain-db" >/dev/null 2>&1 || true
  echo "Certificate created."
fi

# 2. Sign. --identifier pins the signing identifier too: a local build
#    otherwise gets hacksor-<content hash>, which moves on every rebuild for
#    the same reason the signature does.
echo "Signing $APP …"
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" \
  --timestamp=none --generate-entitlement-der "$APP"

codesign -dv "$APP" 2>&1 | grep -E "^(Identifier|Authority|Signature)" || true

# 3. Clear grants recorded against the OLD ad-hoc identities, so the next
#    prompt is the one that sticks.
for svc in SystemPolicyDesktopFolder SystemPolicyDocumentsFolder SystemPolicyDownloadsFolder; do
  tccutil reset "$svc" "$BUNDLE_ID" >/dev/null 2>&1 || true
done

echo
echo "Done. macOS will ask once more for Desktop access — allow it, and the"
echo "grant will survive future rebuilds."
