#!/usr/bin/env bash
# Deploys the five Kastor contracts to Stellar testnet and runs the full
# investor flow: KYC -> buy -> holding period -> settle -> rental income -> claim
# -> resale on the secondary market. Also shows a transfer to an unverified
# wallet being rejected.
#
# Requires: stellar CLI >= 28, Rust with the wasm32v1-none target, jq.
# Writes contract ids and wasm hashes to deployments/testnet.json.
#
# By default it builds locally. RELEASE=v0.2.0 ./scripts/deploy-testnet.sh deploys
# the attested WASM of that GitHub release instead (SEP-55).
set -euo pipefail

NET="--network testnet"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WASM="$ROOT/target/wasm32v1-none/release"
OUT="$ROOT/deployments/testnet.json"
HOLDING_LEDGERS=5          # ~25 s so the demo can settle right away
REFUND_GRACE_LEDGERS=17280 # ~1 day
UNIT_PRICE=500000000       # 50 KUSD (7 decimals) per property unit
SUPPLY=1000
RESALE_PRICE=550000000    # 55 KUSD per unit on the secondary market
MARKET_FEE_BPS=100         # 1 %
RELEASE=${RELEASE:-}

log() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

key() {
  stellar keys address "$1" >/dev/null 2>&1 || stellar keys generate "$1" $NET --fund >/dev/null
  stellar keys address "$1"
}

invoke() { # invoke <contract> <source> <fn> [args...]
  local id=$1 src=$2; shift 2
  stellar contract invoke $NET --id "$id" --source "$src" -- "$@"
}

if [ -n "$RELEASE" ]; then
  log "Download attested WASM from release $RELEASE"
  WASM=$(mktemp -d)
  "$ROOT/scripts/fetch-release-wasm.sh" "$RELEASE" "$WASM"
else
  log "Build"
  (cd "$ROOT" && stellar contract build >/dev/null)
fi

log "Accounts"
ADMIN=$(key kastor-admin)
OPERATOR=$(key kastor-kyc-operator)
TREASURY=$(key kastor-treasury)
ISSUER=$(key kastor-usd-issuer)
ALICE=$(key kastor-alice)
BOB=$(key kastor-bob)
CAROL=$(key kastor-carol)
printf 'admin %s\noperator %s\ntreasury %s\nalice %s\nbob %s\ncarol %s\n' "$ADMIN" "$OPERATOR" "$TREASURY" "$ALICE" "$BOB" "$CAROL"

log "Test dollar (KUSD) standing in for USDC"
ASSET="KUSD:$ISSUER"
USD=$(stellar contract id asset --asset "$ASSET" $NET)
stellar contract asset deploy --asset "$ASSET" --source kastor-usd-issuer $NET >/dev/null 2>&1 || true
for who in kastor-admin kastor-treasury kastor-alice kastor-carol; do
  stellar tx new change-trust --source "$who" --line "$ASSET" $NET >/dev/null 2>&1 || true
done
invoke "$USD" kastor-usd-issuer mint --to "$ALICE" --amount 100000000000   # 10,000 KUSD
invoke "$USD" kastor-usd-issuer mint --to "$ADMIN" --amount 100000000000
invoke "$USD" kastor-usd-issuer mint --to "$CAROL" --amount 100000000000

deploy() { # deploy <wasm> [constructor args...]
  local wasm=$1; shift
  stellar contract deploy $NET --source kastor-admin --wasm "$WASM/$wasm.wasm" -- "$@"
}

log "Deploy kyc-registry"
REGISTRY=$(deploy kyc_registry --admin "$ADMIN")
invoke "$REGISTRY" kastor-admin set_operator --operator "$OPERATOR" --enabled true

log "Deploy property-token (Kastor Villa 01)"
TOKEN=$(deploy property_token --admin "$ADMIN" --registry "$REGISTRY" \
  --name "Kastor Villa 01" --symbol KV01 --metadata_uri "ipfs://kastor-villa-01-demo")

log "Deploy primary-sale"
SALE=$(deploy primary_sale --config "$(jq -nc \
  --arg admin "$ADMIN" --arg token "$TOKEN" --arg reg "$REGISTRY" --arg usd "$USD" \
  --arg treasury "$TREASURY" --arg price "$UNIT_PRICE" \
  --argjson hold $HOLDING_LEDGERS --argjson grace $REFUND_GRACE_LEDGERS \
  '{admin:$admin, property_token:$token, registry:$reg, usdc:$usd, treasury:$treasury,
    unit_price:$price, holding_ledgers:$hold, refund_grace_ledgers:$grace}')")

log "Deploy dividends"
DIVIDENDS=$(deploy dividends --admin "$ADMIN" --usdc "$USD" --property_token "$TOKEN")

log "Deploy secondary-market"
MARKET=$(deploy secondary_market --config "$(jq -nc \
  --arg admin "$ADMIN" --arg reg "$REGISTRY" --arg usd "$USD" --arg treasury "$TREASURY" \
  --argjson fee $MARKET_FEE_BPS \
  '{admin:$admin, registry:$reg, usdc:$usd, treasury:$treasury, fee_bps:$fee}')")
invoke "$MARKET" kastor-admin set_property --property "$TOKEN" --enabled true

log "KYC: sale contract, admin, Alice and Carol verified; Bob is not"
LEDGER=$(curl -s https://horizon-testnet.stellar.org/ | jq .history_latest_ledger)
EXPIRY=$((LEDGER + 30 * 17280))
invoke "$REGISTRY" kastor-kyc-operator verify --operator "$OPERATOR" \
  --wallets "[\"$SALE\",\"$ADMIN\",\"$ALICE\",\"$CAROL\"]" --expires_at_ledger $EXPIRY

log "Mint the property supply into the sale contract"
invoke "$TOKEN" kastor-admin mint --to "$SALE" --amount $SUPPLY

log "Alice buys 10 units (500 KUSD into escrow)"
invoke "$SALE" kastor-alice buy --buyer "$ALICE" --units 10
invoke "$SALE" kastor-alice purchase --buyer "$ALICE"

log "Wait for the holding period, then settle"
sleep $((HOLDING_LEDGERS * 6 + 5))
invoke "$SALE" kastor-admin complete --buyer "$ALICE"
echo "Alice KV01 balance: $(invoke "$TOKEN" kastor-alice balance --id "$ALICE")"

log "Transfer to unverified Bob is rejected (expected error NotVerified = #7)"
if invoke "$TOKEN" kastor-alice transfer --from "$ALICE" --to "$BOB" --amount 1 2>&1; then
  echo "UNEXPECTED: transfer went through" >&2; exit 1
fi

log "Rental income: 1,000 KUSD split pro-rata among holders"
invoke "$DIVIDENDS" kastor-admin allocate_pro_rata --holders "[\"$ALICE\"]" --amount 10000000000
invoke "$DIVIDENDS" kastor-alice claim --holder "$ALICE"

log "Secondary market: Alice lists 4 units at 55 KUSD, Carol buys 2"
LIST_UNTIL=$((LEDGER + 7 * 17280))
invoke "$TOKEN" kastor-alice approve --from "$ALICE" --spender "$MARKET" --amount 4 --expiration_ledger $LIST_UNTIL
LISTING=$(invoke "$MARKET" kastor-alice list --seller "$ALICE" --property "$TOKEN" --units 4 \
  --unit_price $RESALE_PRICE --expiration_ledger $LIST_UNTIL)
invoke "$MARKET" kastor-carol buy --buyer "$CAROL" --listing_id "$LISTING" --units 2
echo "Carol KV01 balance: $(invoke "$TOKEN" kastor-carol balance --id "$CAROL")"
invoke "$MARKET" kastor-alice listing --listing_id "$LISTING"

log "Write $OUT"
hash() { sha256sum "$WASM/$1.wasm" | cut -d' ' -f1; }
jq -n --arg reg "$REGISTRY" --arg tok "$TOKEN" --arg sale "$SALE" --arg div "$DIVIDENDS" --arg mkt "$MARKET" \
  --arg usd "$USD" --arg asset "$ASSET" \
  --arg hreg "$(hash kyc_registry)" --arg htok "$(hash property_token)" \
  --arg hsale "$(hash primary_sale)" --arg hdiv "$(hash dividends)" --arg hmkt "$(hash secondary_market)" \
  --arg src "${RELEASE:+github release $RELEASE (SEP-55 attested build)}" --arg commit "$(git -C "$ROOT" rev-parse HEAD)" \
  --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg cli "$(stellar --version | head -1)" '{
  network: "testnet", deployed_at: $date, cli: $cli, commit: $commit,
  wasm_source: (if $src == "" then "local build" else $src end),
  test_dollar: {asset: $asset, contract: $usd},
  contracts: {
    kyc_registry:   {id: $reg,  wasm_sha256: $hreg},
    property_token: {id: $tok,  wasm_sha256: $htok},
    primary_sale:   {id: $sale, wasm_sha256: $hsale},
    dividends:      {id: $div,  wasm_sha256: $hdiv},
    secondary_market: {id: $mkt, wasm_sha256: $hmkt}
  }}' | tee "$OUT"

echo
echo "Explorer: https://stellar.expert/explorer/testnet/contract/$TOKEN"
