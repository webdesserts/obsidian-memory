#!/usr/bin/env bash
#
# Build the desktop app and code-sign it with the stable self-signed certificate
# so the macOS Application Firewall's "allow incoming connections" rule persists
# across rebuilds.
#
# Why: ad-hoc signing (`codesign -s -`) anchors the firewall's Designated
# Requirement on the binary's cdhash, which changes on every `cargo build` — so
# the firewall re-prompts every rebuild. Signing with a named cert anchors the DR
# on the cert instead (stable), so one "Allow" sticks across all future rebuilds.
# Proven on rhea (MDM/stealth firewall) 2026-06-13.
#
# Prereq: the "ObsidianMemory Dev Signing" code-signing cert must be in the login
# keychain and trusted for code signing. Setup runbook: Plans/Stable Code-Signing
# (the cert is also stored in 1Password "Develop" for fleet distribution).
#
# Usage:
#   ./scripts/build-desktop.sh            # debug build + sign
#   ./scripts/build-desktop.sh --release  # release build + sign
#   (any extra args are forwarded to `cargo build -p desktop`)
#
# Note: do NOT use `cargo run` for the app — it launches the binary before it can
# be signed. Build-then-sign with this script, then launch the signed binary.
set -euo pipefail

CERT_NAME="ObsidianMemory Dev Signing"

# Detect build profile from the forwarded args (default: debug).
PROFILE_DIR="target/debug"
for arg in "$@"; do
  [[ "$arg" == "--release" ]] && PROFILE_DIR="target/release"
done
BINARY="${PROFILE_DIR}/desktop"

cargo build -p desktop "$@"

if ! security find-identity -v -p codesigning | grep -q "$CERT_NAME"; then
  echo "error: code-signing identity '$CERT_NAME' not found in your keychain." >&2
  echo "  Set it up per Plans/Stable Code-Signing (cert is in 1Password 'Develop')." >&2
  echo "  Without it, the firewall re-prompts on every rebuild." >&2
  exit 1
fi

echo "Signing ${BINARY} with '${CERT_NAME}'…"
codesign -s "$CERT_NAME" --force "$BINARY"
codesign --verify -vvv "$BINARY" 2>/dev/null && echo "Signed + verified: ${BINARY}"
