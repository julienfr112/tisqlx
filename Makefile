# Single entry point for local development and CI.
#
#   make db-up          start the dedicated test TiDB (v8.5.8, port 4100, TLS + socket)
#   make test           unit + live tests (starts nothing; run db-up first)
#   make ci             everything CI runs
#   make db-cluster     start the cluster tier (tiup playground: 2 TiDB, PD, TiKV, TiFlash)
#   make test-cluster   tests that need the real cluster
#   make bench          throughput against the test DB (10 connections, 10 s per workload)
#   make bench-cluster  the same on a production-shaped playground (1 TiDB, 3 TiKV), started and stopped
#
# Tests never use a shared server: TISQLX_TEST_URL points at the dedicated instance.

TIDB_PORT   ?= 4100
TEST_URL    ?= tidb://root@127.0.0.1:$(TIDB_PORT)
TEST_SOCKET ?= $(CURDIR)/target/tidb/tidb.sock
MACROS_URL  := $(TEST_URL)/tisqlx_macros
MSRV        := 1.88
CLI         := cargo run -q -p tisqlx-cli --

# Live tier: tests run against the test DB, and the query macros are checked online too.
LIVE_ENV := TISQLX_TEST_URL=$(TEST_URL) TISQLX_TEST_SOCKET=$(TEST_SOCKET) TISQLX_TEST_REQUIRE=1 \
	DATABASE_URL=$(MACROS_URL) TISQLX_OFFLINE=false

.PHONY: help bench bench-cluster db-up db-down db-cluster db-cluster-down test-cluster fmt lint test-unit test test-live macros-db prepare prepare-check features msrv audit coverage ci

help:
	@sed -n 's/^#   \(make .*\)/\1/p' Makefile

db-up:
	TIDB_PORT=$(TIDB_PORT) scripts/tidb.sh up

db-down:
	TIDB_PORT=$(TIDB_PORT) scripts/tidb.sh down

db-cluster:
	scripts/tidb.sh cluster-up

db-cluster-down:
	scripts/tidb.sh cluster-down

# Tests that need PD/TiKV/TiFlash and two TiDB nodes (skipped by the other tiers).
test-cluster:
	TISQLX_TEST_URL=$(TEST_URL) TISQLX_TEST_CLUSTER=$$(scripts/tidb.sh cluster-urls) TISQLX_TEST_REQUIRE_CLUSTER=1 \
		cargo test -p tisqlx-core --all-features --test live cluster:: -- --include-ignored

fmt:
	cargo fmt --all

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

# No database needed: live tests skip, macros compile from .tisqlx/.
test-unit:
	cargo test --workspace --all-features
	cargo test -q -p tisqlx-macros-core --all-features --lib
	cargo test -q -p tisqlx-macros-core --no-default-features --features time,bigdecimal --lib

# Unit + live tests (the `#[ignore]`d ones need the database); fails if it is unreachable.
test-live test: macros-db
	$(LIVE_ENV) cargo test --workspace --all-features -- --include-ignored --skip cluster::
	cargo test -q -p tisqlx-macros-core --all-features --lib
	cargo test -q -p tisqlx-macros-core --no-default-features --features time,bigdecimal --lib

# Schema the query macros are checked against.
macros-db:
	DATABASE_URL=$(MACROS_URL) $(CLI) database setup --source tisqlx/tests/migrations

# Regenerate the offline query data (.tisqlx/) against the test database.
prepare: macros-db
	DATABASE_URL=$(MACROS_URL) $(CLI) prepare -- --all-features

# Fail if the committed offline data is stale.
prepare-check: macros-db
	DATABASE_URL=$(MACROS_URL) $(CLI) prepare --check -- --all-features

# Every supported feature combination of the facade builds.
features:
	cargo check -q -p tisqlx
	cargo check -q -p tisqlx --no-default-features
	cargo check -q -p tisqlx --no-default-features --features macros
	cargo check -q -p tisqlx --no-default-features --features chrono,decimal
	cargo check -q -p tisqlx --no-default-features --features macros,time,bigdecimal,uuid
	cargo check -q -p tisqlx --all-features

msrv:
	CARGO_TARGET_DIR=target/msrv cargo +$(MSRV) check -q --workspace --all-targets --all-features

# Throughput (release build). Override e.g. `make bench BENCH_ARGS="--connections 50 --workload select"`.
BENCH_ARGS ?=
bench:
	cargo run -q --release -p tisqlx-core --example bench -- --url $(TEST_URL) $(BENCH_ARGS)

# Not the default benchmark: a production-shaped cluster (3 TiKV, so every write is replicated
# 3 times; 1 TiDB; no TiFlash) at port offset 20000, started for the run and stopped after.
BENCH_CLUSTER := CLUSTER_PORT_OFFSET=20000 CLUSTER_DB=1 CLUSTER_KV=3 CLUSTER_TIFLASH=0
bench-cluster:
	$(BENCH_CLUSTER) scripts/tidb.sh cluster-up
	cargo run -q --release -p tisqlx-core --example bench -- --url $$($(BENCH_CLUSTER) scripts/tidb.sh cluster-url) $(BENCH_ARGS); \
		status=$$?; $(BENCH_CLUSTER) scripts/tidb.sh cluster-down; exit $$status

audit:
	cargo audit

coverage: macros-db
	$(LIVE_ENV) cargo llvm-cov --workspace --all-features --html -- --include-ignored --skip cluster::
	@echo "report: target/llvm-cov/html/index.html"

ci: lint features test-live prepare-check
