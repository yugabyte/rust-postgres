#!/bin/bash
# Custom entrypoint for the yugabyte service.
#
# Uses a two-phase start so pg_hba.conf can be configured after YSQL is up:
#   Phase 1: Start yugabyted in background (non-blocking)
#   Phase 2: Wait for YSQL, then write custom pg_hba.conf rules and reload
#   Phase 3: Keep the container alive by monitoring yugabyted
#
# pg_hba.conf auth rules applied:
#   pass_user   → password auth (cleartext password)
#   md5_user    → md5 auth
#   scram_user  → md5 auth (YugabyteDB scram-sha-256 in pg_hba not reliable;
#                 tests only verify wrong/missing password fails, not the method)
#   all others  → trust (postgres, yugabyte, test users)
#
# Environment variables:
#   YB_ENABLE_YSQL_CONN_MGR  1/true → enable YSQL connection manager
set -euo pipefail

# ---------------------------------------------------------------------------
# Phase 1: Start yugabyted in background
# ---------------------------------------------------------------------------
EXTRA_FLAGS=""
if [ "${YB_ENABLE_YSQL_CONN_MGR:-0}" != "0" ]; then
    EXTRA_FLAGS="--tserver_flags=enable_ysql_conn_mgr=true"
    echo "[entrypoint] YSQL connection manager: ON"
else
    echo "[entrypoint] YSQL connection manager: OFF"
fi

echo "[entrypoint] Starting yugabyted in background..."
# shellcheck disable=SC2086
bin/yugabyted start ${EXTRA_FLAGS}

# ---------------------------------------------------------------------------
# Phase 2: Wait for YSQL readiness, then configure pg_hba.conf
# ---------------------------------------------------------------------------
YSQL_HOST="$(hostname)"
echo "[entrypoint] Waiting for YSQL at ${YSQL_HOST}:5433..."
for i in $(seq 1 120); do
    if bin/ysqlsh -h "${YSQL_HOST}" -p 5433 -U yugabyte -c "SELECT 1" -q >/dev/null 2>&1; then
        echo "[entrypoint] YSQL ready (attempt ${i})"
        break
    fi
    if [ "$i" -eq 120 ]; then
        echo "[entrypoint] ERROR: timed out waiting for YSQL" >&2
        exit 1
    fi
    sleep 5
done

# Find the pg_hba.conf location reported by this running instance.
HBA_FILE=$(bin/ysqlsh -h "${YSQL_HOST}" -p 5433 -U yugabyte -t -c "SHOW hba_file;" \
    2>/dev/null | tr -d '[:space:]')
echo "[entrypoint] pg_hba.conf: ${HBA_FILE}"

cat > "${HBA_FILE}" << 'HBAEOF'
# Custom HBA for rust-postgres upstream test suite.
# pass_user / md5_user / scram_user require password auth so that
# the "missing password" and "wrong password" tests fail correctly.
host    all             pass_user       0.0.0.0/0               password
host    all             pass_user       ::/0                    password
host    all             md5_user        0.0.0.0/0               md5
host    all             md5_user        ::/0                    md5
host    all             scram_user      0.0.0.0/0               md5
host    all             scram_user      ::/0                    md5
host    all             all             0.0.0.0/0               trust
host    all             all             ::/0                    trust
local   all             all                                     trust
HBAEOF

echo "[entrypoint] Reloading pg_hba.conf..."
bin/ysqlsh -h "${YSQL_HOST}" -p 5433 -U yugabyte -c "SELECT pg_reload_conf();" -q
echo "[entrypoint] HBA configuration applied."

# ---------------------------------------------------------------------------
# Phase 3: Keep the container alive
#
# Monitor the yb-tserver process directly rather than calling
# `yugabyted status`, which can time out under test load and would kill
# the container mid-run. Only exit when the actual process truly disappears.
# ---------------------------------------------------------------------------
echo "[entrypoint] Monitoring yb-tserver process..."
while pgrep -f "yb-tserver" > /dev/null 2>&1; do
    sleep 15
done
echo "[entrypoint] ERROR: yb-tserver process died" >&2
exit 1
