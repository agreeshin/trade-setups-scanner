#!/bin/sh
set -eu
set -o pipefail
umask 077

scope="${BACKUP_SCOPE:?BACKUP_SCOPE is required}"
backup_dir="${BACKUP_DIR:-/var/lib/trade-setups/backups}"
marker="${backup_dir}/${scope}.restore.pending"
pending="${backup_dir}/${scope}.restore.pending.dump"
status="${backup_dir}/${scope}.restore.last.json"

restore_dump() {
  dump="$1"
  pg_restore --file=- --clean --if-exists --no-owner --no-privileges "${dump}" |
    psql --dbname="${DATABASE_URL}" --quiet --set ON_ERROR_STOP=1
}

if [ -f "${marker}" ]; then
  if [ ! -f "${pending}" ]; then
    printf '{"status":"failed","error":"pending dump is missing"}\n' > "${status}"
    exit 1
  fi

  timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
  rollback="${backup_dir}/${scope}.pre-restore-${timestamp}.dump"
  rollback_partial="${rollback}.partial"

  if ! pg_dump --dbname="${DATABASE_URL}" --format=custom --compress=6 --no-owner --no-privileges > "${rollback_partial}"; then
    rm -f "${rollback_partial}"
    printf '{"status":"failed","error":"pre-restore backup failed"}\n' > "${status}"
    exit 1
  fi
  mv "${rollback_partial}" "${rollback}"

  if restore_dump "${pending}"; then
    printf '{"status":"applied","appliedAt":"%s","rollback":"%s"}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(basename "${rollback}")" > "${status}"
    rm -f "${marker}" "${pending}"
  else
    if restore_dump "${rollback}"; then
      failed="${backup_dir}/${scope}.restore-failed-${timestamp}.dump"
      mv "${pending}" "${failed}"
      rm -f "${marker}"
      printf '{"status":"failed_rolled_back","failedAt":"%s","failedDump":"%s","rollback":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(basename "${failed}")" "$(basename "${rollback}")" > "${status}"
    else
      printf '{"status":"failed_rollback","failedAt":"%s","rollback":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(basename "${rollback}")" > "${status}"
      exit 1
    fi
  fi

  count=0
  for file in $(ls -1t "${backup_dir}/${scope}.pre-restore-"*.dump 2>/dev/null || true); do
    count=$((count + 1))
    if [ "${count}" -gt "${BACKUP_ROLLBACK_RETENTION:-3}" ]; then rm -f "${file}"; fi
  done
fi

exec "$@"
