#!/usr/bin/env python3
"""Golden-прогоны поверх живого контура signals-platform-rs.

Сценарии и baselines: services/setups-platform-rs/fixtures/golden-runs.json.
Каждый сценарий выполняется sync POST /v1/replay-runs (fresh compute при
--fresh: cache-key вида price_volume_spike@1.1.1|bybit/linear/SYMBOL|from-to|...),
сетапы берутся из GET /v1/replay-runs/{id}; md5 считается по компактной
JSON-сериализации массива setups; опционально сверяется сквозной SQL-эталон
(md5 setups jsonb::text из replay_runs). Запускать на tv из корня репозитория:
python3 scripts/golden_runs.py --fresh --update --sql-check
"""
import argparse
import hashlib
import json
import subprocess
import sys
import urllib.request

FIXTURES = "services/setups-platform-rs/fixtures/golden-runs.json"


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", default=FIXTURES)
    parser.add_argument("--base-url", default="http://127.0.0.1:8083")
    parser.add_argument("--env-file", default="infra/prod/.env")
    parser.add_argument("--compose", default="infra/prod/docker-compose.yml")
    parser.add_argument("--update", action="store_true", help="записать фактические md5/counts в fixtures")
    parser.add_argument("--fresh", action="store_true", help="удалить cache-строки сценария перед прогоном")
    parser.add_argument("--sql-check", action="store_true", help="сверить sqlSetupsMd5 (md5 setups jsonb::text)")
    parser.add_argument("--keep", action="store_true", help="не удалять созданные прогоны")
    return parser.parse_args()


def api_key(env_file):
    with open(env_file, encoding="utf-8") as handle:
        for line in handle:
            if line.startswith("PLATFORM_API_KEY="):
                return line.strip().split("=", 1)[1]
    raise SystemExit(f"PLATFORM_API_KEY not found in {env_file}")


def psql(compose, sql):
    command = ["docker", "compose", "-f", compose, "exec", "-T", "postgres",
               "psql", "-U", "trade_setups", "-d", "signals", "-t", "-A", "-c", sql]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    return result.stdout.strip()


def request_json(base_url, key, path, method="GET", body=None):
    request = urllib.request.Request(
        base_url.rstrip("/") + path,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"x-api-key": key, "content-type": "application/json"},
        method=method,
    )
    with urllib.request.urlopen(request, timeout=900) as response:
        return json.loads(response.read().decode())


def compact_md5(value):
    payload = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.md5(payload).hexdigest()


def main():
    args = parse_args()
    with open(args.fixtures, encoding="utf-8") as handle:
        fixtures = json.load(handle)
    key = api_key(args.env_file)
    failures = 0
    for scenario in fixtures["scenarios"]:
        name = scenario["name"]
        expected = scenario.get("expected") or {}
        symbol = scenario["symbol"]
        if args.fresh:
            psql(args.compose, f"delete from replay_cache where cache_key like '%/{symbol}|%'")
        algorithm = scenario["algorithm"]
        body = {
            "instruments": [{
                "exchange": scenario["exchange"],
                "category": scenario["category"],
                "symbol": symbol,
            }],
            "fromUnix": scenario["fromUnix"],
            "toUnix": scenario["toUnix"],
            # Плоская форма (как echo preflight): вложенный объект algorithm
            # create-эндпоинтом игнорируется и параметры резолвились бы из
            # текущих настроек, ломая воспроизводимость golden-сценария.
            "algorithmId": algorithm["id"],
            "algorithmVersion": algorithm["version"],
            "params": algorithm["params"],
        }
        payload = request_json(args.base_url, key, "/v1/replay-runs", "POST", body)
        run = (payload.get("runs") or [None])[0] or {}
        run_id = run.get("id")
        if run_id is None:
            print(f"FAIL {name}: no run in response: {json.dumps(payload)[:200]}")
            failures += 1
            continue
        setups = []
        total = None
        offset = 0
        while True:
            detail = request_json(args.base_url, key,
                                  f"/v1/replay-runs/{run_id}?offset={offset}&limit=500")
            page = detail.get("setups")
            if page is None:
                print(f"FAIL {name}: no setups in detail: {json.dumps(detail)[:200]}")
                failures += 1
                break
            total = detail.get("total", len(setups) + len(page))
            setups.extend(page)
            offset += len(page)
            if not page or len(setups) >= total:
                break
        if total is None:
            continue
        actual_md5 = compact_md5(setups)
        actual_count = len(setups)
        checks = []
        if total is not None and total != actual_count:
            checks.append(f"detail total {total} != fetched {actual_count}")
        if expected.get("setupCount") is not None and expected["setupCount"] != actual_count:
            checks.append(f"setupCount {actual_count} != {expected['setupCount']}")
        if expected.get("setupsMd5") and expected["setupsMd5"] != actual_md5:
            checks.append(f"setupsMd5 {actual_md5} != {expected['setupsMd5']}")
        if args.sql_check and expected.get("sqlSetupsMd5"):
            row = psql(args.compose,
                       f"select md5(setups::text) from replay_runs where id = {run_id}")
            if row != expected["sqlSetupsMd5"]:
                checks.append(f"sqlSetupsMd5 {row} != {expected['sqlSetupsMd5']}")
        status = "PASS" if not checks else "FAIL"
        if checks:
            failures += 1
        print(f"{status} {name}: setups={actual_count} md5={actual_md5} "
              f"cacheHit={run.get('cacheHit')}"
              + (f" ({'; '.join(checks)})" if checks else ""))
        if args.update:
            expected["setupsMd5"] = actual_md5
            expected["setupCount"] = actual_count
            if args.sql_check:
                expected["sqlSetupsMd5"] = psql(
                    args.compose,
                    f"select md5(setups::text) from replay_runs where id = {run_id}")
        if not args.keep:
            psql(args.compose, f"delete from replay_quality_intervals where run_id = {run_id}")
            psql(args.compose, f"delete from replay_runs where id = {run_id}")
    if args.update:
        with open(args.fixtures, "w", encoding="utf-8") as handle:
            json.dump(fixtures, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        print(f"fixtures updated: {args.fixtures}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
