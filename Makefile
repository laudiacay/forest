SER_TESTS = "tests/serialization_tests"
CONF_TESTS = "tests/conformance_tests"

install:
	cargo install --locked --path forest --force

clean-all:
	cargo clean

clean:
	@echo "Cleaning local packages..."
	@cargo clean -p forest
	@cargo clean -p fil_clock
	@cargo clean -p forest_libp2p
	@cargo clean -p forest_blocks
	@cargo clean -p chain_sync
	@cargo clean -p forest_vm
	@cargo clean -p forest_address
	@cargo clean -p forest_actor
	@cargo clean -p forest_message
	@cargo clean -p forest_runtime
	@cargo clean -p state_tree
	@cargo clean -p state_manager
	@cargo clean -p interpreter
	@cargo clean -p forest_crypto
	@cargo clean -p forest_encoding
	@cargo clean -p forest_cid
	@cargo clean -p forest_ipld
	@cargo clean -p ipld_hamt
	@cargo clean -p ipld_amt
	@cargo clean -p forest_bigint
	@cargo clean -p forest_bitfield
	@cargo clean -p commcid
	@cargo clean -p fil_types
	@cargo clean -p ipld_blockstore
	@cargo clean -p rpc
	@cargo clean -p key_management
	@cargo clean -p forest_json_utils
	@cargo clean -p test_utils
	@cargo clean -p message_pool
	@cargo clean -p genesis
	@cargo clean -p actor_interface
	@cargo clean -p forest_hash_utils
	@cargo clean -p networks
	@echo "Done cleaning."

lint: license clean
	cargo fmt --all
	cargo clippy -- -D warnings -A clippy::upper_case_acronyms

build:
	cargo build --bin forest

release:
	cargo build --release --bin forest

interopnet:
	cargo build --release --manifest-path=forest/Cargo.toml --no-default-features --features "rocksdb, interopnet"

devnet:
	cargo build  --manifest-path=forest/Cargo.toml --no-default-features --features "devnet, rocksdb"

docker-run:
	docker build -t forest:latest -f ./Dockerfile . && docker run forest

# Git submodule test vectors
pull-serialization-tests:
	git submodule update --init

run-serialization-vectors:
	cargo test --release --manifest-path=$(SER_TESTS)/Cargo.toml --features "submodule_tests"

run-conformance-vectors:
	cargo test --release --manifest-path=$(CONF_TESTS)/Cargo.toml --features "submodule_tests" -- --nocapture

run-vectors: run-serialization-vectors run-conformance-vectors

test-vectors: pull-serialization-tests run-vectors

# Test all without the submodule test vectors with release configuration
test:
	cargo test --all --exclude serialization_tests --exclude conformance_tests --exclude forest_message --exclude forest_crypto
	cargo test -p forest_crypto --features blst --no-default-features
	cargo test -p forest_crypto --features pairing --no-default-features
	cargo test -p forest_message --features blst --no-default-features
	cargo test -p forest_message --features pairing --no-default-features

test-release:
	cargo test --release --all --exclude serialization_tests --exclude conformance_tests --exclude forest_message --exclude forest_crypto
	cargo test --release -p forest_crypto --features blst --no-default-features
	cargo test --release -p forest_crypto --features pairing --no-default-features
	cargo test --release -p forest_message --features blst --no-default-features
	cargo test --release -p forest_message --features pairing --no-default-features

smoke-test:
	./scripts/smoke_test.sh

test-all: test-release run-vectors

# Checks if all headers are present and adds if not
license:
	./scripts/add_license.sh

docs:
	cargo doc --no-deps

mdbook:
	mdbook serve documentation

mdbook-build:
	mdbook build ./documentation

.PHONY: clean clean-all lint build release test test-all test-release license test-vectors run-vectors pull-serialization-tests install docs run-serialization-vectors run-conformance-vectors
