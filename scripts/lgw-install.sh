#!/usr/bin/env bash
# Build, sign and install lgw.
#
# Signing is not cosmetic on macOS. An ad-hoc signature -- what cargo produces --
# is identified by the hash of the code, so the keychain sees a different
# application after every rebuild and re-asks for every stored key. A stable
# signature is what lets an "Always Allow" grant stick.
#
# Usage:
#   scripts/lgw-install.sh
#   LGW_SIGN_IDENTITY=other scripts/lgw-install.sh
set -euo pipefail

IDENTITY="${LGW_SIGN_IDENTITY:-aigw-dev}"
BIN="${LGW_BIN:-${CARGO_HOME:-$HOME/.cargo}/bin/lgw}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "not macOS: nothing to sign, running the install only"
  exec cargo install --path "$REPO" --locked
fi

# A VALID identity, not merely an imported certificate: codesign would block on a
# keychain authorisation dialog for an identity whose key it cannot use, and this
# script must not hang halfway through an install.
if ! security find-identity -v -p codesigning 2>/dev/null | grep -qF "\"$IDENTITY\""; then
  echo >&2 "no usable codesigning identity named \"$IDENTITY\"."
  echo >&2 "run scripts/lgw-identity.sh for the exact steps (it needs your password)."
  exit 1
fi

echo "==> cargo install --path $REPO --locked"
cargo install --path "$REPO" --locked

echo "==> signing $BIN"
# --identifier is pinned so the designated requirement does not drift: it is the
# requirement, not the code hash, that the keychain remembers. Treat this string
# as part of the stored credentials rather than as a label -- changing it changes
# what macOS sees asking for the item, and the access-control grant does not
# follow. (Checked once here: renaming it from ai-gw to lgw did NOT break access,
# because the grant matched the certificate. Do not rely on that.)
# --force is needed because cargo leaves an ad-hoc signature in place.
codesign --force --sign "$IDENTITY" --identifier com.timho.lgw --timestamp=none "$BIN"

echo "==> verifying"
codesign --verify --strict "$BIN"

echo
echo "installed:  $BIN"
echo "signed by:  $IDENTITY"
echo "requirement the keychain will remember:"
codesign -d -r- "$BIN" 2>&1 | tail -1 | sed 's/^/    /'
echo
echo "Run lgw once and click Always Allow on the prompt. Every key lives in the"
echo "single vault item, so one authorisation covers every account from then on."
