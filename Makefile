CARGO ?= cargo

# Prerequisite order is only guaranteed under serial make; under -j the promise
# "cheapest gate first" breaks.
.NOTPARALLEL:
.PHONY: check fmt clippy test linux e2e dist

# Definition of done. Homebrew rustc is not pinned (there is deliberately no
# rust-toolchain.toml): the version is printed first so a clippy failure that
# arrives after a `brew upgrade` can be told apart from a code failure.
check:
	@rustc --version
	@$(MAKE) --no-print-directory fmt clippy test

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace --all-targets

# The same gate on Linux, in the image from `tools/linux/Dockerfile`, with
# `--locked` (a run that would change Cargo.lock fails), then the release build
# the shipped Linux binaries are: the host architecture's musl target, which
# must come out statically linked and must run. Target outputs go to
# `target/linux` so they do not mix with the host's.
#
# The image's Rust tag comes from the FROM line of the Dockerfile and must
# match the local rustc's major.minor — two compilers' clippy are two
# different gates. Without Docker this prints SKIPPED and exits 2: that is a
# gate that did not run, not a pass.
LINUX_DOCKERFILE = tools/linux/Dockerfile
LINUX_RUST = $(shell sed -n 's/^FROM rust:\([0-9]*\.[0-9]*\)-.*/\1/p' $(LINUX_DOCKERFILE))
LINUX_IMAGE = kablo-linux:$(LINUX_RUST)

# Shell fragments the Docker targets share: the image's Rust must be the local
# one, and Docker must answer.
define match_rust
local_rust=$$(rustc --version | sed -n 's/^rustc \([0-9]*\.[0-9]*\).*/\1/p'); \
	if [ -z "$(LINUX_RUST)" ]; then \
		echo "$(1): could not read the rust version from the FROM line of $(LINUX_DOCKERFILE)"; exit 1; fi; \
	if [ "$$local_rust" != "$(LINUX_RUST)" ]; then \
		echo "$(1): local rustc $$local_rust, image rust:$(LINUX_RUST) — the versions do not match; bring the FROM line of $(LINUX_DOCKERFILE) to the local version"; exit 1; fi
endef
define need_docker
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then \
		echo "SKIPPED: Docker is missing or the daemon does not answer — make $(1) could not run"; exit 2; fi
endef

linux:
	@$(call match_rust,linux)
	@$(call need_docker,linux)
	docker build -q -t $(LINUX_IMAGE) -f $(LINUX_DOCKERFILE) tools/linux
	docker run --rm -v "$(CURDIR)":/w -v kablo-linux-cargo:/usr/local/cargo/registry \
		-e CARGO_TARGET_DIR=/w/target/linux $(LINUX_IMAGE) sh -c '\
		set -e; \
		cargo clippy --workspace --all-targets --locked -- -D warnings; \
		cargo test --workspace --all-targets --locked; \
		t="$$(uname -m)-unknown-linux-musl"; \
		cargo build --release --locked -p kablo --target "$$t"; \
		bin="target/linux/$$t/release/kablo"; \
		described=$$(file "$$bin"); echo "$$described"; \
		echo "$$described" | grep -Eq "statically linked|static-pie linked" || { echo "linux: $$bin is not statically linked"; exit 1; }; \
		"$$bin" version'

# End to end, over a real sshd: the Linux binary of `make linux` goes into a
# Debian container with an OpenSSH server (tests/e2e), and the scenarios in
# crates/kablo/tests/e2e.rs run on this machine against it with
# `ssh -T host kablo attach`. The container has no systemd, so the keeper
# outlives the login by the `setsid` route; the server's sshd_config is the
# distribution's, MaxSessions included. Without Docker: SKIPPED, exit 2.
e2e:
	@$(call match_rust,e2e)
	@$(call need_docker,e2e)
	docker build -q -t $(LINUX_IMAGE) -f $(LINUX_DOCKERFILE) tools/linux
	docker run --rm -v "$(CURDIR)":/w -v kablo-linux-cargo:/usr/local/cargo/registry \
		-e CARGO_TARGET_DIR=/w/target/linux $(LINUX_IMAGE) sh -c '\
		set -e; cargo build --release --locked -p kablo --target "$$(uname -m)-unknown-linux-musl"'
	tests/e2e/run.sh "$(CURDIR)/target/linux/$$(docker run --rm $(LINUX_IMAGE) uname -m)-unknown-linux-musl/release/kablo"

# Not built yet; it exits 1 so nothing mistakes it for a pass.
dist:
	@echo "dist: not yet"; exit 1
