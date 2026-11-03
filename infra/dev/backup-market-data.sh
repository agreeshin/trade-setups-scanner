#!/usr/bin/env bash
# Dev helper: dump the market_data database of the dev postgres container.
# Real transfers require stopping the writer first (see docs/ops/market-data-transfer.md).
set -euo pipefail
OUT="${1:-backups/market-data-$(date +%Y%m%d-%H%M%S).sql}"
mkdir -p "$(dirname "$OUT")"
docker exec trade-setups-dev-postgres pg_dump -U trade_setups --no-owner --no-privileges market_data > "$OUT"
echo "backup written to $OUT"
