CARGO ?= cargo

# Prerequisite order is only guaranteed under serial make; under -j the promise
# "cheapest gate first" breaks.
.NOTPARALLEL:
.PHONY: check fmt clippy test linux e2e dist dist-signed notices notices-check release-gate release publish ship

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

# The terminal parser's fork (`vendor/vte`) is not a workspace member — the gate
# does not reformat or lint upstream's code — but its tests, upstream's and ours,
# run here, and bateri relies on them: it takes this directory by a pinned commit.
# `-p vte` finds it through `alacritty_terminal`, which also turns on the `ansi`
# feature the fork's changes to the handler live behind (without it most of them
# would go untested, so a graph that stops using `alacritty_terminal` stops
# testing them: look for the 77 tests of its lib target in the output).
VTE_TEST = test -p vte --all-targets

# The same for the emulator's fork (`vendor/alacritty_terminal`), whose tests are upstream's
# and ours. It has dev-dependencies and is not a member, so cargo will not test it as `-p` of
# this workspace: it is tested as the package it is, on the Cargo.lock upstream shipped with
# it (`--locked`: the versions upstream tested), with its default features, which is also how
# upstream's reference tests (they need `serde`) run.
ALACRITTY_TEST = test --manifest-path vendor/alacritty_terminal/Cargo.toml --all-targets --locked

test:
	$(CARGO) test --workspace --all-targets
	$(CARGO) $(VTE_TEST)
	$(CARGO) $(ALACRITTY_TEST)

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
		cargo $(VTE_TEST) --locked; \
		cargo $(ALACRITTY_TEST); \
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

# The notice for the third-party code in the binaries (MIT and Apache-2.0 ask
# for it to travel with copies): generated from the product graph of the four
# targets, kept in the repository, shipped next to the binaries. `make notices`
# rewrites it, and `make dist` refuses to run when it is out of date — a new or
# upgraded dependency, or a different license on one, shows up there first.
NOTICES = THIRD-PARTY-LICENSES.txt
NOTICES_TOOL = tools/third_party_notices.py

notices:
	@python3 -I $(NOTICES_TOOL) >$(NOTICES).partial || { rm -f $(NOTICES).partial; exit 1; }
	@mv $(NOTICES).partial $(NOTICES)
	@echo "$(NOTICES) written"

notices-check:
	@t=$$(mktemp) && trap 'rm -f "$$t"' EXIT && python3 -I $(NOTICES_TOOL) >"$$t" || exit 1; \
	cmp -s "$$t" $(NOTICES) || { echo "notices: $(NOTICES) is out of date (the product graph or the tool changed); run: make notices"; exit 1; }

# The binaries a release ships: dist/kablo-<version>-<target> for the four
# targets, the licenses next to them (the project's own and the notice above),
# and SHA256SUMS over all of them (tools/dist.sh is the how). The targets
# and the names are the ones `kablo_proto::install` gives clients, and a test
# reads these lines to prove it. The Linux ones are static musl builds made
# and opened in a container of their own architecture (the one that is not the
# machine's runs emulated); the macOS ones are cross-built by the rustup
# toolchain, because Homebrew's rustc cannot add the standard library of the
# Apple targets (`rustup target add <target> --toolchain $(DIST_TOOLCHAIN)`),
# so both come from that one compiler, which is not the one `make check` uses.
# Docker and the Apple targets are required, and a dist/ without SHA256SUMS is
# a build that did not finish.
DIST_VERSION = $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
# What the files are named, and what `kablo version` says they are. A release is named by
# its version alone (`release`, and `publish` and `ship` with it, which look for those
# files). Any other `make dist` builds a development binary and names it
# `<version>-dev+<commit>` (`tools/dev-suffix.sh`: a working tree that is not the commit
# adds a digest of what differs), so that no two builds of a version share a name, which
# is what a server's directory per version and a session's "which build am I" rest on.
DIST_RELEASING = $(filter release release-gate publish ship,$(MAKECMDGOALS))
DIST_SUFFIX = $(eval DIST_SUFFIX := $(if $(DIST_RELEASING),,$(shell sh tools/dev-suffix.sh)))$(DIST_SUFFIX)
DIST_ID = $(DIST_VERSION)$(DIST_SUFFIX)
DIST_LINUX_TARGETS = x86_64-unknown-linux-musl aarch64-unknown-linux-musl
DIST_MACOS_TARGETS = aarch64-apple-darwin x86_64-apple-darwin
DIST_NAME = kablo-$(DIST_ID)-%
DIST_SUMS = SHA256SUMS
DIST_LICENSES = LICENSE $(NOTICES)
DIST_TOOLCHAIN ?= stable

DIST_ENV = DIST_VERSION="$(DIST_VERSION)" DIST_ID="$(DIST_ID)" DIST_NAME="$(DIST_NAME)" DIST_SUMS="$(DIST_SUMS)" \
	DIST_IMAGE="$(LINUX_IMAGE)" DIST_DOCKERFILE="$(LINUX_DOCKERFILE)" \
	DIST_TOOLCHAIN="$(DIST_TOOLCHAIN)" DIST_LINUX_TARGETS="$(DIST_LINUX_TARGETS)" \
	DIST_MACOS_TARGETS="$(DIST_MACOS_TARGETS)" DIST_LICENSES="$(DIST_LICENSES)"

dist: notices-check
	@$(call match_rust,dist)
	@$(call need_docker,dist)
	$(DIST_ENV) tools/dist.sh

# The same files, with the macOS binaries signed and notarized: what a release
# ships, so that a Mac opens them without a prompt. Signing is with a
# Developer ID Application identity from the keychain, found by that name
# (`SIGN_ID="..."` names another), with the hardened runtime and a secure
# timestamp; notarization is by `xcrun notarytool` with the keychain profile
# NOTARY_PROFILE, set up once with `xcrun notarytool store-credentials` (the
# password does not enter the repo; bateri's releases use the same Apple
# account and the same profile). It needs the network and takes minutes, which
# is why `dist` alone does neither: it is what development builds.
SIGN_ID ?= $(shell security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1)
NOTARY_PROFILE ?= bateri-notary

dist-signed: notices-check
	@test -n '$(SIGN_ID)' || { echo "dist-signed: there is no Developer ID Application identity in the keychain (security find-identity -v -p codesigning), and the macOS binaries of a release are signed"; exit 1; }
	@$(call match_rust,dist-signed)
	@$(call need_docker,dist-signed)
	$(DIST_ENV) DIST_SIGN_ID='$(SIGN_ID)' DIST_NOTARY_PROFILE='$(NOTARY_PROFILE)' tools/dist.sh

# Publishing a version is one command, on this Mac:
#
#   make ship      release + push main + publish, end to end
#
# Its two halves also run alone, because the split is the point: what a
# release carries is tried on this Mac before it is opened to everyone.
#
#   make release   the gates, then the files -> $(RELEASE_DIR); nothing leaves
#                  the machine
#   make publish   tags the commit `release` built as v<version>, pushes the
#                  tag and opens the GitHub release with the files and the
#                  note (`gh`)
#
# **Gates first, the build after** (`release-gate` is cheap and the build
# takes a long time): the version has to come from a known commit — the tree
# is clean, and `v<version>` exists neither here nor on origin — and has to
# come with a note. The note's only source is CHANGELOG.md at the root: the
# gate cuts out its `## [<version>]` section and stops when there is none or
# it is empty, so a version nobody wrote a note for is not released. The
# version is Cargo.toml's (`dist` names the files from it); nobody passes
# `VERSION=`.
#
# Then every gate the project has, none skipped: the notice is current
# (cheap), `check`, `linux`, `e2e`, `dist-signed` (the build, with the macOS
# binaries signed and notarized). Without Docker `linux` and `e2e` print SKIPPED and exit 2, which in
# a release is a stop, not an allowed gap. The tree must be as clean at the
# end as at the start (a gate that rewrites Cargo.lock means the binaries are
# not the commit's) and HEAD the same commit; only then `release` writes that
# commit to $(RELEASE_DIR)/commit — last, so a directory without it is a
# release that did not finish. `publish` tags **that** commit, not whatever
# HEAD is by then, so the tag always names the code in the binaries, and it
# stops unless the commit is on origin/main, otherwise it would publish code
# no branch holds.
#
# A release that went out wrong is replaced by a new version, never by the
# same number again: apps keep one directory per version on a server and never
# overwrite it (see `kablo_proto::install`), so a corrected build under an old
# number would reach nobody who already has it.
REPO = bateri/kablo
RELEASE_TAG = v$(DIST_VERSION)
RELEASE_DIR = target/release-$(DIST_VERSION)
RELEASE_NOTES = $(RELEASE_DIR)/notes.md
# What a release uploads is what `dist` summed, by the same lists.
RELEASE_FILES = $(patsubst %,$(DIST_NAME),$(DIST_LINUX_TARGETS) $(DIST_MACOS_TARGETS)) $(DIST_LICENSES) $(DIST_SUMS)

# Fails unless origin is reachable and has no tag $(RELEASE_TAG): a question
# that could not be asked is not an answer of "free".
define origin_lacks_tag
listed=$$(git ls-remote --tags origin 'refs/tags/$(RELEASE_TAG)') || { echo "$(1): could not ask origin about the tag $(RELEASE_TAG)"; exit 1; }; \
	test -z "$$listed" || { echo "$(1): tag $(RELEASE_TAG) already exists on origin"; exit 1; }
endef
# The files in $(RELEASE_DIR) are the ones SHA256SUMS lists, no more and no
# fewer, and each has its sum.
define check_release_sums
cd $(RELEASE_DIR) && \
	awk '{ print $$2 }' $(DIST_SUMS) | sort >.listed && printf '%s\n' $(filter-out $(DIST_SUMS),$(RELEASE_FILES)) | sort >.expected && \
	{ cmp -s .listed .expected || { rm -f .listed .expected; echo "$(1): $(DIST_SUMS) does not list exactly the files of the release"; exit 1; }; } && rm -f .listed .expected && \
	if command -v sha256sum >/dev/null 2>&1; then sha256sum -c $(DIST_SUMS); else shasum -a 256 -c $(DIST_SUMS); fi
endef

release-gate:
	@echo '$(DIST_VERSION)' | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$' || { echo "release: version '$(DIST_VERSION)' is not x.y.z (Cargo.toml)"; exit 1; }
	@test -z "$$(git status --porcelain)" || { echo "release: the working tree is not clean, the version must come from a known commit"; exit 1; }
	@! git rev-parse -q --verify 'refs/tags/$(RELEASE_TAG)' >/dev/null || { echo "release: tag $(RELEASE_TAG) already exists — bump the version in Cargo.toml"; exit 1; }
	@$(call origin_lacks_tag,release)
	@rm -rf $(RELEASE_DIR) && mkdir -p $(RELEASE_DIR); \
	awk -v v='$(DIST_VERSION)' '/^## \[/ { if (on) exit; if (index($$0, "## [" v "]") == 1) { on = 1; next } } on' CHANGELOG.md \
		| awk 'NF { p = 1 } p' | awk '{ l[NR] = $$0 } END { n = NR; while (n > 0 && l[n] ~ /^[[:space:]]*$$/) n--; for (i = 1; i <= n; i++) print l[i] }' >$(RELEASE_NOTES); \
	grep -q '[^[:space:]]' $(RELEASE_NOTES) || { rm -rf $(RELEASE_DIR); echo "release: CHANGELOG.md has no '## [$(DIST_VERSION)]' section or it is empty (turn Unreleased into the version)"; exit 1; }
	git rev-parse HEAD >$(RELEASE_DIR)/.gate

release: release-gate notices-check check linux e2e dist-signed
	@test -z "$$(git status --porcelain)" && test "$$(git rev-parse HEAD)" = "$$(cat $(RELEASE_DIR)/.gate)" || { echo "release: the tree or HEAD changed while the gates ran, so the binaries are not of one known commit"; exit 1; }
	cp $(addprefix dist/,$(RELEASE_FILES)) $(RELEASE_DIR)/
	@$(call check_release_sums,release)
	@mv $(RELEASE_DIR)/.gate $(RELEASE_DIR)/commit
	@echo "release: $(RELEASE_DIR) ($$(cat $(RELEASE_DIR)/commit)) — try the binary for this machine, then: make publish"

publish:
	@for f in $(RELEASE_FILES) $(notdir $(RELEASE_NOTES)) commit; do \
		test -f $(RELEASE_DIR)/$$f || { echo "publish: $(RELEASE_DIR)/$$f is missing — run make release first"; exit 1; }; \
	done
	@command -v gh >/dev/null 2>&1 || { echo "publish: gh is missing"; exit 1; }
	@gh auth status >/dev/null 2>&1 || { echo "publish: gh is not logged in (gh auth login)"; exit 1; }
	@! git rev-parse -q --verify 'refs/tags/$(RELEASE_TAG)' >/dev/null || { echo "publish: tag $(RELEASE_TAG) already exists here"; exit 1; }
	@$(call origin_lacks_tag,publish)
	git fetch -q origin main
	@git merge-base --is-ancestor "$$(cat $(RELEASE_DIR)/commit)" origin/main || { echo "publish: the built commit is not on origin/main — push main first"; exit 1; }
	@$(call check_release_sums,publish)
	git tag -a '$(RELEASE_TAG)' -m 'kablo $(DIST_VERSION)' "$$(cat $(RELEASE_DIR)/commit)"
	git push origin '$(RELEASE_TAG)' || { echo "publish: the tag could not be pushed and exists only here — remove it (git tag -d $(RELEASE_TAG)) and run make publish again"; exit 1; }
	gh release create '$(RELEASE_TAG)' $(addprefix $(RELEASE_DIR)/,$(RELEASE_FILES)) \
		--repo '$(REPO)' --verify-tag --latest --title 'kablo $(DIST_VERSION)' --notes-file $(RELEASE_NOTES) \
		|| { echo "publish: the release was not completed and the tag is on origin. Remove what exists and start again: gh release delete $(RELEASE_TAG) --repo $(REPO) --cleanup-tag (if no release was created: git push origin :refs/tags/$(RELEASE_TAG)), then git tag -d $(RELEASE_TAG), then make publish"; exit 1; }
	@echo "publish: $(RELEASE_TAG) — https://github.com/$(REPO)/releases/tag/$(RELEASE_TAG)"

# Stops on a branch other than main BEFORE the gates, not after: `publish`
# wants the commit on origin/main and only main is pushed here.
ship:
	@test "$$(git branch --show-current)" = main || { echo "ship: branch is not main"; exit 1; }
	$(MAKE) release
	git push origin main
	$(MAKE) publish
