#!/usr/bin/env bash
# Dedicated TiDB for tisqlx tests: a standalone tidb-server (unistore, no PD/TiKV)
# on its own port, with TLS and a Unix socket. Never point tests at a shared server.
#
#   scripts/tidb.sh up | down | status | url
#   scripts/tidb.sh cluster-up | cluster-down | cluster-urls
#
# Uses the tidb-server binary from tiup (`tiup install tidb:$TIDB_VERSION`), or
# docker (`TIDB_RUNNER=docker`) with the pingcap/tidb image of the same version.
#
# `cluster-up` starts a real cluster for the cluster test tier with `tiup playground`:
# 2 TiDB, 1 PD, 1 TiKV, 1 TiFlash, every port shifted by $CLUSTER_PORT_OFFSET (10000)
# so it can run next to a default playground. It takes ~30 s and a few GB of RAM.
set -euo pipefail

TIDB_VERSION="${TIDB_VERSION:-v8.5.8}"
TIDB_PORT="${TIDB_PORT:-4100}"
TIDB_STATUS_PORT="${TIDB_STATUS_PORT:-10180}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE="${TIDB_STATE_DIR:-$ROOT/target/tidb}"
SOCKET="$STATE/tidb.sock"
RUNNER="${TIDB_RUNNER:-tiup}"
CONTAINER="tisqlx-tidb-$TIDB_PORT"

url() { echo "tidb://root@127.0.0.1:$TIDB_PORT"; }

wait_ready() {
  for _ in $(seq 1 120); do
    if curl -sf "http://127.0.0.1:$TIDB_STATUS_PORT/status" >/dev/null; then return 0; fi
    sleep 0.5
  done
  echo "TiDB did not become ready; see $STATE/tidb.log" >&2
  return 1
}

up() {
  mkdir -p "$STATE"
  if curl -sf "http://127.0.0.1:$TIDB_STATUS_PORT/status" >/dev/null; then
    echo "TiDB already running: $(url)"; return 0
  fi
  cat >"$STATE/tidb.toml" <<TOML
[security]
auto-tls = true
TOML
  case "$RUNNER" in
    tiup)
      local bin="${TIDB_SERVER:-$HOME/.tiup/components/tidb/$TIDB_VERSION/tidb-server}"
      [ -x "$bin" ] || { echo "missing $bin: run \`tiup install tidb:$TIDB_VERSION\` or set TIDB_RUNNER=docker" >&2; exit 1; }
      rm -f "$SOCKET"
      nohup "$bin" -P "$TIDB_PORT" -status "$TIDB_STATUS_PORT" --host 127.0.0.1 --status-host 127.0.0.1 \
        --store unistore --path "$STATE/data" --socket "$SOCKET" \
        --config "$STATE/tidb.toml" --log-file "$STATE/tidb.log" >"$STATE/stdout.log" 2>&1 &
      echo $! >"$STATE/tidb.pid"
      ;;
    docker)
      docker run -d --rm --name "$CONTAINER" \
        -p "127.0.0.1:$TIDB_PORT:4000" -p "127.0.0.1:$TIDB_STATUS_PORT:10080" \
        -v "$STATE/tidb.toml:/tidb.toml:ro" \
        "pingcap/tidb:$TIDB_VERSION" --store unistore --path /data --config /tidb.toml >/dev/null
      ;;
    *) echo "unknown TIDB_RUNNER=$RUNNER" >&2; exit 1 ;;
  esac
  wait_ready
  echo "TiDB $TIDB_VERSION ready: $(url)"
}

down() {
  case "$RUNNER" in
    tiup)
      if [ -f "$STATE/tidb.pid" ]; then
        kill "$(cat "$STATE/tidb.pid")" 2>/dev/null || true
        rm -f "$STATE/tidb.pid"
      fi
      ;;
    docker) docker rm -f "$CONTAINER" >/dev/null 2>&1 || true ;;
  esac
}

CLUSTER_PORT_OFFSET="${CLUSTER_PORT_OFFSET:-10000}"
CLUSTER_STATE="$ROOT/target/tidb-cluster"

cluster_urls() {
  echo "tidb://root@127.0.0.1:$((4000 + CLUSTER_PORT_OFFSET)),tidb://root@127.0.0.1:$((4001 + CLUSTER_PORT_OFFSET))"
}

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# Ready once PD lists a TiFlash store and TiDB answers on its status port.
cluster_ready() {
  curl -sf "http://127.0.0.1:$((10080 + CLUSTER_PORT_OFFSET))/status" >/dev/null &&
    curl -sf "http://127.0.0.1:$((2379 + CLUSTER_PORT_OFFSET))/pd/api/v1/stores" | grep -q tiflash
}

cluster_up() {
  if cluster_ready; then echo "cluster already running: $(cluster_urls)"; return 0; fi
  # Refuse to start over someone else's ports (e.g. another project's playground).
  for p in 4000 4001 2379 2380 20160 20180 3930 3931 9000 8234 10080 10081; do
    if port_busy $((p + CLUSTER_PORT_OFFSET)); then
      echo "port $((p + CLUSTER_PORT_OFFSET)) is in use; set CLUSTER_PORT_OFFSET to another value" >&2
      exit 1
    fi
  done
  mkdir -p "$CLUSTER_STATE"
  # No `--tag`: the playground then deletes its data directory itself when it exits.
  nohup tiup playground "$TIDB_VERSION" --db 2 --kv 1 --pd 1 --tiflash 1 \
    --without-monitor --port-offset "$CLUSTER_PORT_OFFSET" >"$CLUSTER_STATE/playground.log" 2>&1 &
  echo $! >"$CLUSTER_STATE/playground.pid"
  for _ in $(seq 1 120); do
    if cluster_ready; then echo "cluster ready: $(cluster_urls)"; return 0; fi
    sleep 2
  done
  echo "cluster did not become ready; see $CLUSTER_STATE/playground.log" >&2
  return 1
}

cluster_down() {
  if [ -f "$CLUSTER_STATE/playground.pid" ]; then
    local pid
    pid="$(cat "$CLUSTER_STATE/playground.pid")"
    kill "$pid" 2>/dev/null || true
    # The playground stops its components one by one (TiFlash takes a while), then deletes
    # its data directory: wait for that.
    for _ in $(seq 1 90); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
    rm -f "$CLUSTER_STATE/playground.pid"
  fi
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  status) curl -sf "http://127.0.0.1:$TIDB_STATUS_PORT/status" && echo ;;
  url) url ;;
  socket) echo "$SOCKET" ;;
  cluster-up) cluster_up ;;
  cluster-down) cluster_down ;;
  cluster-urls) cluster_urls ;;
  *) echo "usage: $0 up|down|status|url|socket|cluster-up|cluster-down|cluster-urls" >&2; exit 2 ;;
esac
