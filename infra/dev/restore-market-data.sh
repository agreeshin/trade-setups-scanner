#!/usr/bin/env bash
# Dev helper: restore a market_data dump into the dev postgres container.
# Usage: restore-market-data.sh <dump.sql> [target-db]
set -euo pipefail
FILE="${1:?usage: restore-market-data.sh <dump.sql> [target-db]}"
TARGET_DB="${2:-market_data}"
docker exec trade-setups-dev-postgres psql -U trade_setups -d postgres -c \
  "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '${TARGET_DB}' AND pid <> pg_backend_pid()" > /dev/null
docker exec trade-setups-dev-postgres psql -U trade_setups -d postgres -c "DROP DATABASE IF EXISTS ${TARGET_DB}"
docker exec trade-setups-dev-postgres createdb -U trade_setups "${TARGET_DB}"
docker exec -i trade-setups-dev-postgres psql -U trade_setups -d "${TARGET_DB}" -v ON_ERROR_STOP=1 < "$FILE"
echo "restored ${TARGET_DB} from ${FILE}"
