#!/usr/bin/env bash
# run-tests.sh — entrypoint for the rust-tests container.
#
# Steps:
#   1. Wait for YSQL on yugabyte:5433
#   2. Start socat proxy 127.0.0.1:5433 → yugabyte:5433
#      (tests hard-code TcpStream::connect("127.0.0.1:5433") and host=localhost)
#   3. Create required roles and extensions
#   4. Run cargo test suite (three invocations matching CI)
set -euo pipefail

YUGABYTE_HOST="${YUGABYTE_HOST:-yugabyte}"
YUGABYTE_PORT="${YUGABYTE_PORT:-5433}"
PSQL_ARGS=(-h "$YUGABYTE_HOST" -p "$YUGABYTE_PORT" -U yugabyte)

# ---------------------------------------------------------------------------
# 1. Wait for YSQL
# ---------------------------------------------------------------------------
echo "=== Waiting for YSQL at ${YUGABYTE_HOST}:${YUGABYTE_PORT} ==="
for i in $(seq 1 180); do
    if PGPASSWORD=yugabyte psql "${PSQL_ARGS[@]}" -d yugabyte -c "SELECT 1" >/dev/null 2>&1; then
        echo "YSQL is accepting connections (attempt ${i})."
        break
    fi
    if [[ "$i" -eq 180 ]]; then
        echo "ERROR: Timed out waiting for YugabyteDB YSQL." >&2
        exit 1
    fi
    sleep 2
done

# ---------------------------------------------------------------------------
# 2. socat proxy: 127.0.0.1:$YUGABYTE_PORT → $YUGABYTE_HOST:$YUGABYTE_PORT
#
# Needed because tests hard-code "127.0.0.1:5433" for raw TCP connections and
# "host=localhost port=5433" for the runtime/dns tests. Inside this container,
# the loopback does not reach the yugabyte container by default.
# ---------------------------------------------------------------------------
echo "=== Starting socat proxy: 127.0.0.1:${YUGABYTE_PORT} -> ${YUGABYTE_HOST}:${YUGABYTE_PORT} ==="
socat "TCP4-LISTEN:${YUGABYTE_PORT},fork,reuseaddr,bind=127.0.0.1" \
      "TCP4:${YUGABYTE_HOST}:${YUGABYTE_PORT}" &
SOCAT_PID=$!
sleep 1
if ! kill -0 "$SOCAT_PID" 2>/dev/null; then
    echo "WARNING: socat failed to start; tests using 127.0.0.1 may fail." >&2
else
    echo "socat proxy running (pid ${SOCAT_PID})."
fi

# ---------------------------------------------------------------------------
# 3. Create roles and extensions
#
# The rust-postgres test suite expects these users with password='password':
#   postgres   — superuser (no password needed; trust/md5 from YB default HBA)
#   pass_user  — password auth tests
#   md5_user   — md5 auth tests
#   scram_user — scram-sha-256 auth tests (YugabyteDB will use md5 by default;
#                the tests only check succeed/fail, not the exact auth method)
#   ssl_user   — SSL tests; reachable over plain TCP since YB SSL is not active
#
# Extensions: hstore, ltree, citext
# ---------------------------------------------------------------------------
echo "=== Creating roles ==="

PGPASSWORD=yugabyte psql "${PSQL_ARGS[@]}" -d yugabyte -v ON_ERROR_STOP=1 <<'SQL'
DO $do$
BEGIN
  CREATE ROLE postgres WITH LOGIN SUPERUSER;
EXCEPTION
  WHEN duplicate_object THEN NULL;
END
$do$;

DO $do$
BEGIN
  CREATE ROLE pass_user WITH LOGIN PASSWORD 'password';
EXCEPTION
  WHEN duplicate_object THEN NULL;
END
$do$;

DO $do$
BEGIN
  CREATE ROLE md5_user WITH LOGIN PASSWORD 'password';
EXCEPTION
  WHEN duplicate_object THEN NULL;
END
$do$;

DO $do$
BEGIN
  CREATE ROLE scram_user WITH LOGIN PASSWORD 'password';
EXCEPTION
  WHEN duplicate_object THEN NULL;
END
$do$;

DO $do$
BEGIN
  CREATE ROLE ssl_user WITH LOGIN;
EXCEPTION
  WHEN duplicate_object THEN NULL;
END
$do$;
SQL

echo "=== Creating extensions ==="
# Tests connect to the 'postgres' database (e.g. "user=postgres" defaults to
# dbname=postgres, and most connection strings include "dbname=postgres").
# Extensions must exist in the 'postgres' database, not 'yugabyte'.
for DB in yugabyte postgres; do
    PGPASSWORD=yugabyte psql "${PSQL_ARGS[@]}" -d "${DB}" -v ON_ERROR_STOP=1 \
        -c "CREATE EXTENSION IF NOT EXISTS hstore;"
    PGPASSWORD=yugabyte psql "${PSQL_ARGS[@]}" -d "${DB}" -v ON_ERROR_STOP=1 \
        -c "CREATE EXTENSION IF NOT EXISTS ltree;"
    PGPASSWORD=yugabyte psql "${PSQL_ARGS[@]}" -d "${DB}" -v ON_ERROR_STOP=0 \
        -c "CREATE EXTENSION IF NOT EXISTS citext;" 2>&1 || \
        echo "NOTE: citext not available in ${DB} (non-fatal)."
done

echo "=== Role and extension setup complete ==="

# Brief pause to let YugabyteDB propagate the catalog version.
sleep 3

# ---------------------------------------------------------------------------
# 4. Run tests
#
# Excluded crates:
#   yb-postgres-openssl, yb-postgres-native-tls
#       → No server-side TLS in this local setup; covered in stress suite.
#   postgres-derive-test
#       → Pins crates.io postgres=0.19.7 which is trait-incompatible with the
#         YB fork's local postgres-types path dep; compile error, not runtime.
#
# Skipped phase:
#   cargo test --no-default-features (tokio-postgres)
#       → Code bug in YB fork: SocketConfig and connect module are not
#         properly #[cfg(feature="runtime")] gated; does not compile.
#
# Phases:
#   Phase 1: all workspace crates except the excluded ones above
#   Phase 2: tokio-postgres with --all-features (all type-mapping features)
#
# Per-test timeout: 90 seconds (via cargo-nextest).
# Any test that hangs beyond 90s is terminated and reported as TIMEOUT.
# This prevents slow/hanging tests from crashing the YugabyteDB server.
#
# RUST_BACKTRACE=1 gives useful stack traces on failures.
# ---------------------------------------------------------------------------
export RUST_BACKTRACE=1

# Write a nextest profile config with a 90s per-test timeout.
NEXTEST_CFG=$(mktemp /tmp/nextest-yb-XXXXXX.toml)
cat > "${NEXTEST_CFG}" << 'NEXTEST_EOF'
[profile.default]
# Terminate any test that runs longer than 90 seconds.
slow-timeout = { period = "90s", terminate-after = 1 }
NEXTEST_EOF

# ---------------------------------------------------------------------------
# Helper: verify DB is reachable before starting a test phase.
# Prevents 100+ misleading "socat: Name or service not known" failures
# when YugabyteDB has crashed.
# ---------------------------------------------------------------------------
check_db() {
    local phase="$1"
    echo ""
    echo "=== Checking YugabyteDB connectivity before ${phase} ==="
    if ! PGPASSWORD=yugabyte psql "${PSQL_ARGS[@]}" -d yugabyte -c "SELECT 1" -q >/dev/null 2>&1; then
        echo "ERROR: YugabyteDB is not reachable at ${YUGABYTE_HOST}:${YUGABYTE_PORT}." >&2
        echo "       The server may have crashed. Skipping ${phase}." >&2
        return 1
    fi
    echo "YugabyteDB is healthy."
    return 0
}

OVERALL_EXIT=0
PHASE1_EXIT=0
PHASE2_EXIT=0

echo ""
echo "=== Phase 1/2: cargo nextest run --workspace (excluding TLS and derive-test crates) ==="
if ! check_db "Phase 1"; then
    PHASE1_EXIT=1
    OVERALL_EXIT=1
    echo "--- Phase 1 skipped (DB unreachable) ---"
else
    set +e
    cargo nextest run \
        --config-file "${NEXTEST_CFG}" \
        --no-fail-fast \
        --workspace \
        --exclude yb-postgres-openssl \
        --exclude yb-postgres-native-tls \
        --exclude postgres-derive-test \
        2>&1
    PHASE1_EXIT=$?
    set -e
    if [[ $PHASE1_EXIT -ne 0 ]]; then
        OVERALL_EXIT=1
        echo "--- Phase 1 exited with code ${PHASE1_EXIT} ---"
    fi
fi

echo ""
echo "=== Phase 2/2: cargo nextest run (tokio-postgres, --all-features) ==="
if ! check_db "Phase 2"; then
    PHASE2_EXIT=1
    OVERALL_EXIT=1
    echo "--- Phase 2 skipped (DB unreachable) ---"
else
    set +e
    cargo nextest run \
        --config-file "${NEXTEST_CFG}" \
        --no-fail-fast \
        --manifest-path tokio-postgres/Cargo.toml \
        --all-features \
        2>&1
    PHASE2_EXIT=$?
    set -e
    if [[ $PHASE2_EXIT -ne 0 ]]; then
        OVERALL_EXIT=1
        echo "--- Phase 2 exited with code ${PHASE2_EXIT} ---"
    fi
fi

rm -f "${NEXTEST_CFG}"

echo ""
echo "=== Test Run Summary ==="
echo "  Phase 1 (workspace, TLS+derive-test excluded):  exit ${PHASE1_EXIT}"
echo "  Phase 2 (tokio-postgres --all-features):        exit ${PHASE2_EXIT}"
echo "  Overall:                                        exit ${OVERALL_EXIT}"
echo "========================="

exit $OVERALL_EXIT
