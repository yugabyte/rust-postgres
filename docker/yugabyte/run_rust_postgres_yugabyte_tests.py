#!/usr/bin/env python3
"""
rust-postgres / YugabyteDB Docker Test Runner
----------------------------------------------
Runs YugabyteDB in one Compose service and cargo test (rust-postgres) in another.
By default runs the full test suite TWICE — once with the YSQL connection manager
OFF and once with it ON — producing exactly two fixed log files per invocation:

  docker/logs/rust-postgres-yugabyte-cmoff.log
  docker/logs/rust-postgres-yugabyte-cmon.log

Output streams to the terminal and is also written to the log files.

Edit RUNNER_DEFAULTS below to change permanent settings.
Any value can also be overridden by an environment variable (env wins over defaults).

Environment variables
---------------------
  RUST_DOCKER_DIR           Directory containing docker-compose.yml (default: this script's dir)
  RUST_COMPOSE_FILE         Full path to compose file (default: RUST_DOCKER_DIR/docker-compose.yml)
  RUST_REPO_ROOT            Rust workspace root (default: parent of RUST_DOCKER_DIR)
  COMPOSE_PROJECT_NAME      Docker Compose project name

  RUST_YSQL_CONN_MGR        "off" | "on" | "both"  (default: "both")
                            Controls which connection-manager modes are tested.

  RUST_SKIP_CLEANUP         1/true: skip initial  `compose down`
  RUST_SKIP_PULL            1/true: skip          `compose pull yugabyte`
  RUST_SKIP_SETUP           1/true: skip pull + build (run stack/tests only)
  RUST_REMOVE_VOLUMES       1/true: add -v to compose down calls
  RUST_POST_DOWN            1/true: run compose down after tests (default); 0/false: leave up

  YB_WAIT_SEC               Seconds to sleep before first YSQL readiness check
  YB_VERIFY_RETRIES         Max YSQL readiness attempts (each waits 5 s on failure)

  RUST_YB_HOST_YSQL_PORT    Host port → container 5433  (YSQL)
  RUST_YB_HOST_ADMIN_PORT   Host port → container 9000  (admin)
  RUST_YB_HOST_UI_PORT      Host port → container 15433 (UI)
  RUST_YB_HOST_YCQL_PORT    Host port → container 9042  (YCQL)

Usage
-----
  python3 run_rust_postgres_yugabyte_tests.py
  Run from the Rust workspace root, or set RUST_DOCKER_DIR to the directory
  that contains this script and docker-compose.yml.

  The rust-postgres repo must be cloned at:
    <workspace_root>/rust-postgres/
  i.e. one level above docker/, alongside this script's parent directory.

  # Run only connection-manager-off:
  RUST_YSQL_CONN_MGR=off python3 run_rust_postgres_yugabyte_tests.py

  # Run only connection-manager-on:
  RUST_YSQL_CONN_MGR=on  python3 run_rust_postgres_yugabyte_tests.py
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

# =============================================================================
# RUNNER_DEFAULTS
# Edit these values to change permanent defaults.
# Any key can be overridden by the corresponding environment variable above.
# =============================================================================
RUNNER_DEFAULTS: dict = {
    # Docker Compose
    "compose_file":  None,              # None → docker-compose.yml next to this script
    "project_name":  "rust-postgres-yb",

    # Connection-manager mode: "off" | "on" | "both"
    "ysql_conn_mgr": "both",

    # Lifecycle
    "skip_cleanup":   False,   # skip initial compose down
    "skip_pull":      False,   # skip compose pull yugabyte
    "skip_setup":     False,   # skip pull + build
    "remove_volumes": False,   # pass -v to compose down
    "no_post_down":   False,   # set True to leave stack up after tests

    # YugabyteDB readiness
    "yb_wait_sec":       20,   # initial sleep before readiness probes
    "yb_verify_retries": 60,   # max probe attempts (each sleeps 5 s on failure)

    # Published host ports (must not clash with local services)
    "yb_host_ysql_port":  55433,
    "yb_host_admin_port": 59000,
    "yb_host_ui_port":    55434,
    "yb_host_ycql_port":  59042,
}

TOTAL_STEPS = 8


# =============================================================================
# Config helpers
# =============================================================================

def _flag(key: str, default: bool) -> bool:
    v = os.environ.get(key, "").strip().lower()
    if not v:
        return default
    return v in ("1", "true", "yes", "on")


def _int(key: str, default: int) -> int:
    v = os.environ.get(key, "").strip()
    if not v:
        return default
    try:
        return int(v)
    except ValueError:
        return default


def _str(key: str, default: str | None) -> str | None:
    v = os.environ.get(key, "").strip()
    return v if v else default


def _cm_mode() -> str:
    """Return the connection-manager mode: 'off', 'on', or 'both'."""
    raw = os.environ.get("RUST_YSQL_CONN_MGR", "").strip().lower()
    if raw in ("off", "on", "both"):
        return raw
    default = str(RUNNER_DEFAULTS.get("ysql_conn_mgr", "both")).lower()
    return default if default in ("off", "on", "both") else "both"


# =============================================================================
# Config dataclass
# =============================================================================

@dataclass
class RunnerConfig:
    script_dir:         Path
    docker_dir:         Path
    repo_root:          Path
    compose_file:       Path
    project_name:       str
    skip_cleanup:       bool
    skip_pull:          bool
    skip_setup:         bool
    remove_volumes:     bool
    post_down:          bool
    yb_wait_sec:        int
    yb_verify_retries:  int
    yb_host_ysql_port:  int
    yb_host_admin_port: int
    yb_host_ui_port:    int
    yb_host_ycql_port:  int


def load_config(script_dir: Path) -> RunnerConfig:
    d = RUNNER_DEFAULTS

    docker_dir = Path(os.environ.get("RUST_DOCKER_DIR", str(script_dir))).resolve()
    repo_root  = Path(os.environ.get("RUST_REPO_ROOT",  str(docker_dir.parent))).resolve()

    cf_raw = _str("RUST_COMPOSE_FILE", None)
    if cf_raw:
        compose_file = Path(cf_raw).expanduser().resolve()
    elif d.get("compose_file"):
        compose_file = Path(str(d["compose_file"])).expanduser().resolve()
    else:
        compose_file = (docker_dir / "docker-compose.yml").resolve()

    return RunnerConfig(
        script_dir         = script_dir,
        docker_dir         = docker_dir,
        repo_root          = repo_root,
        compose_file       = compose_file,
        project_name       = os.environ.get("COMPOSE_PROJECT_NAME", d["project_name"]),
        skip_cleanup       = _flag("RUST_SKIP_CLEANUP",   bool(d["skip_cleanup"])),
        skip_pull          = _flag("RUST_SKIP_PULL",      bool(d["skip_pull"])),
        skip_setup         = _flag("RUST_SKIP_SETUP",     bool(d["skip_setup"])),
        remove_volumes     = _flag("RUST_REMOVE_VOLUMES", bool(d["remove_volumes"])),
        post_down          = _flag("RUST_POST_DOWN",      not bool(d["no_post_down"])),
        yb_wait_sec        = _int("YB_WAIT_SEC",               int(d["yb_wait_sec"])),
        yb_verify_retries  = _int("YB_VERIFY_RETRIES",         int(d["yb_verify_retries"])),
        yb_host_ysql_port  = _int("RUST_YB_HOST_YSQL_PORT",  int(d["yb_host_ysql_port"])),
        yb_host_admin_port = _int("RUST_YB_HOST_ADMIN_PORT", int(d["yb_host_admin_port"])),
        yb_host_ui_port    = _int("RUST_YB_HOST_UI_PORT",    int(d["yb_host_ui_port"])),
        yb_host_ycql_port  = _int("RUST_YB_HOST_YCQL_PORT",  int(d["yb_host_ycql_port"])),
    )


# =============================================================================
# I/O helpers
# =============================================================================

def tee(log_fp, msg: str) -> None:
    sys.stdout.write(msg)
    sys.stdout.flush()
    log_fp.write(msg)
    log_fp.flush()


def tee_run(cmd: list[str], *, cwd: Path, log_fp, env: dict[str, str] | None = None) -> int:
    """Run cmd, streaming stdout+stderr to terminal and log file. Returns exit code."""
    proc = subprocess.Popen(
        cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, bufsize=1, env=env,
    )
    assert proc.stdout is not None
    try:
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            log_fp.write(line)
            log_fp.flush()
    finally:
        proc.stdout.close()
    return proc.wait()


def is_port_free(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


# =============================================================================
# Summary helpers
# =============================================================================

def _parse_cargo_results(content: str) -> tuple[list[str], list[str]]:
    """
    Return (failed_tests, ignored_tests) from cargo test or cargo nextest output.

    cargo test format:
      test foo::bar ... FAILED
      test foo::bar ... ignored

    cargo nextest format:
      FAIL  [ 0.456s] crate::foo::bar
      TIMEOUT [ 90.000s] crate::foo::bar
      SKIP  [       ] crate::foo::bar
    """
    import re

    failed: list[str] = []
    ignored: list[str] = []
    seen: set[str] = set()

    for line in content.splitlines():
        stripped = line.strip()

        # nextest: "FAIL  [ 0.123s] crate::test::name"
        # nextest: "TIMEOUT [ 90.000s] crate::test::name"
        m = re.match(r'^(FAIL|TIMEOUT)\s+\[[\s\d.]+s\]\s+(\S+)', stripped)
        if m:
            name = m.group(2)
            if name not in seen:
                failed.append(name)
                seen.add(name)
            continue

        # nextest: "SKIP  [       ] crate::test::name"
        m = re.match(r'^SKIP\s+\[[\s]*\]\s+(\S+)', stripped)
        if m:
            name = m.group(1)
            if name not in seen:
                ignored.append(name)
                seen.add(name)
            continue

        # cargo test: "test foo::bar ... FAILED"
        if stripped.startswith("test ") and stripped.endswith("FAILED"):
            parts = stripped.split()
            if len(parts) >= 2:
                name = parts[1]
                if name not in seen:
                    failed.append(name)
                    seen.add(name)
            continue

        # cargo test summary block: "FAILED  foo::bar"
        if stripped.startswith("FAILED") and len(stripped.split()) == 2:
            name = stripped.split()[1]
            if name not in seen:
                failed.append(name)
                seen.add(name)
            continue

        # cargo test: "test foo::bar ... ignored"
        if stripped.startswith("test ") and stripped.endswith("ignored"):
            parts = stripped.split()
            if len(parts) >= 2:
                ignored.append(parts[1])

    return failed, ignored


def emit_summary(log_path: Path, cm_label: str) -> None:
    W = 70
    try:
        content = log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        content = ""

    failed, ignored = _parse_cargo_results(content)

    out: list[str] = [
        "",
        "=" * W,
        f"Connection manager: {cm_label.upper()}",
        f"Full test log: {log_path}",
        "=" * W,
        "",
        "FAILURE SUMMARY",
        "-" * W,
    ]
    if failed:
        for i, name in enumerate(failed, 1):
            out.append(f"  {i}. {name}")
        out += ["", "-" * W, f"Total failures: {len(failed)} — see log for full output."]
    else:
        out += ["  (none)", "", "-" * W, "Total failures: 0"]

    out += ["", "IGNORED / SKIPPED SUMMARY", "-" * W]
    if ignored:
        for i, name in enumerate(ignored, 1):
            out.append(f"  {i}. {name}")
        out += ["", "-" * W, f"Total ignored: {len(ignored)}"]
    else:
        out.append("  (none)")
    out += ["", ""]

    block = "\n".join(out)
    sys.stdout.write(block)
    sys.stdout.flush()
    try:
        with log_path.open("a", encoding="utf-8") as f:
            f.write(block)
    except OSError:
        pass


# =============================================================================
# Single-mode test run  (one CM on/off pass)
# =============================================================================

def run_single(
    cfg: RunnerConfig,
    cm_label: str,          # "on" or "off"
    log_path: Path,
    rust_repo: Path,
    *,
    skip_pull: bool,
    skip_setup: bool,
) -> int:
    """
    Execute the full 8-step lifecycle for one connection-manager mode.
    Returns the test exit code (0 = all tests passed).
    skip_pull / skip_setup are separate from cfg to allow the second run
    in 'both' mode to reuse the already-pulled/built image.
    """
    cm_value = "1" if cm_label == "on" else "0"

    compose_env = os.environ.copy()
    compose_env["COMPOSE_PROJECT_NAME"]    = cfg.project_name
    compose_env["YB_PUBLISH_YSQL"]         = str(cfg.yb_host_ysql_port)
    compose_env["YB_PUBLISH_9000"]         = str(cfg.yb_host_admin_port)
    compose_env["YB_PUBLISH_UI"]           = str(cfg.yb_host_ui_port)
    compose_env["YB_PUBLISH_YCQL"]         = str(cfg.yb_host_ycql_port)
    compose_env["YB_ENABLE_YSQL_CONN_MGR"] = cm_value

    compose = ["docker", "compose", "-f", str(cfg.compose_file)]

    print(f"\n{'=' * 60}")
    print(f"  rust-postgres / YugabyteDB — connection manager {cm_label.upper()}")
    print(f"{'=' * 60}")
    print(f"  log file:  {log_path}")
    print(f"  rust repo: {rust_repo}")
    print(f"  ports:     ysql={cfg.yb_host_ysql_port}  admin={cfg.yb_host_admin_port}"
          f"  ui={cfg.yb_host_ui_port}  ycql={cfg.yb_host_ycql_port}")
    print()

    exit_code = 0

    with log_path.open("w", encoding="utf-8") as log_fp:
        log_fp.write(
            f"=== rust-postgres / YugabyteDB test run ===\n"
            f"UTC time:           {datetime.now(timezone.utc).isoformat()}\n"
            f"connection manager: {cm_label}\n"
            f"compose file:       {cfg.compose_file}\n"
            f"project:            {cfg.project_name}\n"
            f"rust repo:          {rust_repo}\n"
            f"ports:              ysql={cfg.yb_host_ysql_port}  admin={cfg.yb_host_admin_port}"
            f"  ui={cfg.yb_host_ui_port}  ycql={cfg.yb_host_ycql_port}\n"
            f"log file:           {log_path}\n"
            f"===\n\n"
        )

        def run_step(step: int, label: str, cmd: list[str]) -> int:
            tee(log_fp, f"\n[{step}/{TOTAL_STEPS}] {label}\n$ {' '.join(cmd)}\n\n")
            code = tee_run(cmd, cwd=cfg.repo_root, log_fp=log_fp, env=compose_env)
            tee(log_fp, f"\n----- finished (exit {code}) -----\n")
            return code

        # ── Step 1: Cleanup ───────────────────────────────────────────────────
        if not cfg.skip_cleanup:
            cmd = compose + ["down", "--remove-orphans"] + (["-v"] if cfg.remove_volumes else [])
            if run_step(1, "Cleanup (compose down)", cmd) != 0:
                emit_summary(log_path, cm_label)
                return 1
        else:
            tee(log_fp, f"\n[1/{TOTAL_STEPS}] Cleanup skipped (RUST_SKIP_CLEANUP)\n\n")

        # ── Steps 2–3: Pull + build ───────────────────────────────────────────
        if not skip_setup:
            if not skip_pull:
                if run_step(2, "Pull yugabyte image", compose + ["pull", "yugabyte"]) != 0:
                    emit_summary(log_path, cm_label)
                    return 1
            else:
                tee(log_fp, f"\n[2/{TOTAL_STEPS}] Pull skipped\n\n")

            if run_step(3, "Build rust-tests image", compose + ["build", "rust-tests"]) != 0:
                emit_summary(log_path, cm_label)
                return 1
        else:
            tee(log_fp, f"\n[2-3/{TOTAL_STEPS}] Setup skipped\n\n")

        # ── Step 4: Start YugabyteDB ──────────────────────────────────────────
        if run_step(4, f"Start yugabyte (CM {cm_label.upper()}, compose up -d)",
                    compose + ["up", "-d", "yugabyte"]) != 0:
            run_step(5, "Diagnostics (compose logs)", compose + ["logs", "--no-color"])
            emit_summary(log_path, cm_label)
            return 1

        # ── Step 5: YSQL readiness ────────────────────────────────────────────
        tee(log_fp, f"\n[5/{TOTAL_STEPS}] Waiting {cfg.yb_wait_sec}s before readiness checks\n")
        time.sleep(max(0, cfg.yb_wait_sec))

        ysql_probe = compose + [
            "exec", "-T", "yugabyte",
            "/home/yugabyte/bin/ysqlsh",
            "-h", "yugabyte", "-p", "5433", "-U", "yugabyte", "-c", "SELECT 1", "-q",
        ]
        ready = False
        for attempt in range(1, cfg.yb_verify_retries + 1):
            if run_step(5, f"YSQL readiness {attempt}/{cfg.yb_verify_retries}", ysql_probe) == 0:
                ready = True
                break
            if attempt < cfg.yb_verify_retries:
                tee(log_fp, "YSQL not ready yet; retrying in 5s...\n")
                time.sleep(5)

        if not ready:
            run_step(5, "Diagnostics (compose logs)", compose + ["logs", "--no-color"])
            emit_summary(log_path, cm_label)
            return 1

        # ── Step 6: Run tests ─────────────────────────────────────────────────
        test_cmd = compose + ["run", "--rm", "rust-tests"]
        exit_code = run_step(6, f"Run cargo test (CM {cm_label.upper()})", test_cmd)

        # ── Step 7: Diagnostics (on failure) ─────────────────────────────────
        if exit_code != 0:
            run_step(7, "Diagnostics (compose logs)", compose + ["logs", "--no-color"])
        else:
            tee(log_fp, f"\n[7/{TOTAL_STEPS}] All tests passed — skipping diagnostics\n\n")

        # ── Step 8: Post-run cleanup ──────────────────────────────────────────
        if cfg.post_down:
            cmd = compose + ["down", "--remove-orphans"] + (["-v"] if cfg.remove_volumes else [])
            run_step(8, "Post-run cleanup (compose down)", cmd)
        else:
            tee(log_fp, f"\n[8/{TOTAL_STEPS}] Post-run cleanup skipped (RUST_POST_DOWN=0)\n\n")

        tee(log_fp, f"\n=== finished: CM {cm_label.upper()} | exit {exit_code} | log: {log_path} ===\n")

    emit_summary(log_path, cm_label)
    return exit_code


# =============================================================================
# Main
# =============================================================================

def main() -> int:
    cfg      = load_config(Path(__file__).resolve().parent)
    cm_mode  = _cm_mode()        # "off" | "on" | "both"
    logs_dir = cfg.docker_dir / "logs"
    logs_dir.mkdir(parents=True, exist_ok=True)

    if not cfg.compose_file.is_file():
        print(f"error: compose file not found: {cfg.compose_file}", file=sys.stderr)
        return 1

    rust_repo = cfg.repo_root / "rust-postgres"
    if not rust_repo.is_dir():
        print(
            f"error: rust-postgres repo not found at {rust_repo}\n"
            f"  Clone it with:\n"
            f"    git clone https://github.com/yugabyte/rust-postgres {rust_repo}",
            file=sys.stderr,
        )
        return 1

    busy = [
        (name, port)
        for name, port in [
            ("YSQL",  cfg.yb_host_ysql_port),
            ("Admin", cfg.yb_host_admin_port),
            ("UI",    cfg.yb_host_ui_port),
            ("YCQL",  cfg.yb_host_ycql_port),
        ]
        if not is_port_free(port)
    ]
    if busy:
        for name, port in busy:
            print(f"error: {name} host port {port} is already in use", file=sys.stderr)
        print("Set RUST_YB_HOST_*_PORT to choose different ports.", file=sys.stderr)
        return 2

    # Determine which modes to run and their fixed log paths.
    modes: list[str] = []
    if cm_mode in ("off", "both"):
        modes.append("off")
    if cm_mode in ("on", "both"):
        modes.append("on")

    log_paths = {
        "off": logs_dir / "rust-postgres-yugabyte-cmoff.log",
        "on":  logs_dir / "rust-postgres-yugabyte-cmon.log",
    }

    print("=== rust-postgres / YugabyteDB Docker Test Runner ===")
    print(f"  docker dir:         {cfg.docker_dir}")
    print(f"  compose file:       {cfg.compose_file}")
    print(f"  project:            {cfg.project_name}")
    print(f"  connection manager: {cm_mode}")
    print(f"  modes to run:       {', '.join(f'CM {m.upper()}' for m in modes)}")
    for m in modes:
        print(f"  log ({m:3s}):          {log_paths[m]}")
    print()

    overall_exit = 0
    results: dict[str, int] = {}

    for i, mode in enumerate(modes):
        # For the second mode in a "both" run, skip pull and build —
        # the image is already pulled/built by the first run.
        is_second_run = (i > 0)
        skip_pull  = cfg.skip_pull  or is_second_run
        skip_setup = cfg.skip_setup or is_second_run

        code = run_single(
            cfg, mode, log_paths[mode], rust_repo,
            skip_pull=skip_pull,
            skip_setup=skip_setup,
        )
        results[mode] = code
        if code != 0:
            overall_exit = 1

    # ── Final summary across all modes ───────────────────────────────────────
    W = 60
    print(f"\n{'=' * W}")
    print("OVERALL RUN SUMMARY")
    print(f"{'=' * W}")
    for mode in modes:
        status = "PASSED" if results[mode] == 0 else f"FAILED (exit {results[mode]})"
        print(f"  CM {mode.upper():3s}: {status}  →  {log_paths[mode]}")
    print(f"{'=' * W}")

    return overall_exit


if __name__ == "__main__":
    sys.exit(main())
