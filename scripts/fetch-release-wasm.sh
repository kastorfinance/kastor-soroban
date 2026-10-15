#!/usr/bin/env bash
# Downloads each contract's WASM from a GitHub release made by release.yml and
# checks its build attestation (SEP-55). Writes <crate_name>.wasm into <out-dir>.
#   ./scripts/fetch-release-wasm.sh v0.2.0 /tmp/wasm
set -euo pipefail
TAG=$1; OUT=$2; REPO=${REPO:-kastorfinance/kastor-soroban}
PACKAGES="kyc-registry property-token primary-sale dividends secondary-market"
mkdir -p "$OUT"
for pkg in $PACKAGES; do
  release=$(gh release list -R "$REPO" --limit 200 --json tagName -q ".[].tagName | select(startswith(\"$TAG-${pkg}_${pkg}_pkg\"))" | head -n 1)
  [ -n "$release" ] || { echo "no release for $pkg at $TAG" >&2; exit 1; }
  tmp=$(mktemp -d)
  gh release download "$release" -R "$REPO" -p '*.wasm' -D "$tmp"
  file=$(ls "$tmp"/*.wasm)
  # The attestation is signed by the reusable SEP-55 workflow, not by a workflow in this repo.
  gh attestation verify "$file" -R "$REPO" --signer-repo stellar-expert/soroban-build-workflow >/dev/null
  mv "$file" "$OUT/${pkg//-/_}.wasm"
  echo "$pkg: $(sha256sum "$OUT/${pkg//-/_}.wasm" | cut -c1-16)… attested ($release)"
done
