# Single entry point for local development and CI.
#
#   make db-up          start the dedicated test TiDB (v8.5.8, port 4100, TLS + socket)
#   make test           unit + live tests (starts nothing; run db-up first)
#   make ci             everything CI runs
#   make db-cluster     start the cluster tier (tiup playground: 2 TiDB, PD, TiKV, TiFlash)
#   make test-cluster   tests that need the real cluster
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

.PHONY: help db-up db-down db-cluster db-cluster-down test-cluster fmt lint test-unit test test-live macros-db prepare prepare-check features msrv audit coverage ci

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
		cargo test -p tisqlx-core --all-features --test live cluster::

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

# Unit + live tests; fails if the test database is unreachable.
test-live test: macros-db
	$(LIVE_ENV) cargo test --workspace --all-features
	$(LIVE_ENV) cargo test -q -p tisqlx --all-features --test test_attr -- --include-ignored
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

audit:
	cargo audit

coverage: macros-db
	$(LIVE_ENV) cargo llvm-cov --workspace --all-features --html
	@echo "report: target/llvm-cov/html/index.html"

ci: lint features test-live prepare-check
