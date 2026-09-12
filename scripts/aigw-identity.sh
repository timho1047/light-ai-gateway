#!/usr/bin/env bash
# Create, and trust, the self-signed code-signing identity ai-gw is signed with.
#
# Why this exists
# ---------------
# macOS binds a keychain item's access-control list to the CODE SIGNATURE of the
# application asking for it. cargo produces an ad-hoc signature, whose identity
# is the hash of the code itself -- so every rebuild is a brand new application
# to the keychain and any "Always Allow" grant is silently invalidated. The
# result is one authorisation prompt per stored key, per run, forever.
#
# A stable identity fixes that: the access-control entry names the certificate,
# and the certificate does not change when the code does.
set -euo pipefail

IDENTITY="${AIGW_SIGN_IDENTITY:-aigw-dev}"
KEYCHAIN="${AIGW_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

valid() {
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "\"$IDENTITY\""
}

exists_untrusted() {
  security find-identity -p codesigning 2>/dev/null | grep -qF "\"$IDENTITY\""
}

report() {
  echo "identity \"$IDENTITY\" is ready:"
  security find-identity -v -p codesigning | grep -F "\"$IDENTITY\""
  echo
  echo "Next: scripts/aigw-install.sh builds, signs and installs the binary."
}

if valid; then
  report
  exit 0
fi

# The certificate exists but macOS will not accept it for signing. Keychain
# Access creates a self-signed certificate WITHOUT trusting it, so this is the
# normal state right after "Create a Certificate..." and it is easy to mistake
# for success. The identity then blocks codesign on an authorisation dialog.
if exists_untrusted; then
  echo "certificate \"$IDENTITY\" exists but is not trusted for code signing."
  echo "adding a user-level trust setting (this needs your login password)..."
  pem="$(mktemp)"
  trap 'rm -f "$pem"' EXIT
  security find-certificate -c "$IDENTITY" -p "$KEYCHAIN" > "$pem"
  # -r trustRoot marks the self-signed certificate as its own root, -p codeSign
  # scopes the trust to code signing rather than everything.
  security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$pem"
  if valid; then
    echo
    report
    exit 0
  fi
  echo >&2 "trust was added but the identity is still not usable." >&2
  exit 1
fi

cat >&2 <<EOF
No code-signing identity named "$IDENTITY" was found.

Create it through Keychain Access -- five clicks, and it sets up the key access
that the command-line route does not:

  1. Open Keychain Access  (Spotlight: "Keychain Access", or
     /System/Library/CoreServices/Applications/Keychain Access.app)
  2. Menu: Keychain Access > Certificate Assistant > Create a Certificate...
  3. Name:              $IDENTITY
     Identity Type:     Self Signed Root
     Certificate Type:  Code Signing
  4. Create, then Done.

Then run this script again: it adds the trust setting that step 4 does NOT add.

To remove the identity later:
    security delete-identity -c "$IDENTITY" "$KEYCHAIN"
EOF
exit 1
