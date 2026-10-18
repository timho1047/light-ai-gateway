#!/usr/bin/env bash
# Build, sign and install lgw and lgwl.
#
# Signing is not cosmetic on macOS. An ad-hoc signature -- what cargo produces --
# is identified by the hash of the code, so the keychain sees a different
# application after every rebuild and re-asks for every stored key. A stable
# signature is what lets an "Always Allow" grant stick.
#
# The install record written at the end is what lets lgwl update find this
# repository again after the terminal is closed.
#
# Usage:
#   scripts/lgw-install.sh
#   LGW_SIGN_IDENTITY=other scripts/lgw-install.sh
set -euo pipefail

IDENTITY="${LGW_SIGN_IDENTITY:-aigw-dev}"
BIN="${LGW_BIN:-${CARGO_HOME:-$HOME/.cargo}/bin/lgw}"
LGWL_BIN="$(dirname "$BIN")/lgwl"
STATE_DIR="${LGWL_STATE_DIR:-${HOME:?HOME is required}/.light-ai-gateway/lgwl}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "==> cargo install --path $REPO --locked"
cargo install --path "$REPO" --locked

if [[ ! -x "$BIN" ]]; then
  echo >&2 "cargo install did not produce $BIN"
  exit 1
fi
if [[ ! -x "$LGWL_BIN" ]]; then
  echo >&2 "cargo install did not produce $LGWL_BIN"
  exit 1
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  # A VALID identity, not merely an imported certificate: codesign would block on
  # a keychain authorisation dialog for an identity whose key it cannot use, and
  # this script must not hang halfway through an install.
  if ! security find-identity -v -p codesigning 2>/dev/null | grep -qF "\"$IDENTITY\""; then
    echo >&2 "no usable codesigning identity named \"$IDENTITY\"."
    echo >&2 "run scripts/lgw-identity.sh for the exact steps (it needs your password)."
    exit 1
  fi

  for binary in "$BIN" "$LGWL_BIN"; do
    echo "==> signing $binary"
    # --identifier is pinned so the designated requirement does not drift: it is
    # the requirement, not the code hash, that the keychain remembers. Treat this
    # string as part of the stored credentials rather than as a label. The
    # launcher gets its own identifier because no keychain item belongs to it.
    # --force is needed because cargo leaves an ad-hoc signature in place.
    identifier="com.timho.lgw"
    if [[ "$binary" == "$LGWL_BIN" ]]; then
      identifier="com.timho.lgwl"
    fi
    codesign --force --sign "$IDENTITY" --identifier "$identifier" --timestamp=none "$binary"
  done

  echo "==> verifying"
  codesign --verify --strict "$BIN"
  codesign --verify --strict "$LGWL_BIN"
else
  echo "not macOS: signatures are skipped"
fi

mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR"
printf '%s\n%s\n%s\n' "$REPO" "$IDENTITY" "$BIN" > "$STATE_DIR/install.conf"
chmod 600 "$STATE_DIR/install.conf"

echo
echo "installed:  $BIN"
echo "launcher:   $LGWL_BIN"
if [[ "$(uname -s)" == "Darwin" ]]; then
  echo "signed by:  $IDENTITY"
  echo "requirement the keychain will remember:"
  codesign -d -r- "$BIN" 2>&1 | tail -1 | sed 's/^/    /'
fi
echo
echo "Background gateway:"
echo "    lgwl launch     # start detached from this terminal"
echo "    lgwl status     # is it running?"
echo "    lgwl log        # follow its log (Ctrl-C stops only the viewer)"
echo "    lgwl exit       # stop it"
echo "    lgwl update     # reinstall, restarting it if it was running"
echo
if [[ "$(uname -s)" == "Darwin" ]]; then
  echo "Run lgw once and click Always Allow on the prompt. Every key lives in the"
  echo "single vault item, so one authorisation covers every account from then on."
fi
