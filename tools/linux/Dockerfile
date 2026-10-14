# The image of `make linux`: the single recipe for building and testing kablo
# on Linux.
#
# The base tag's major.minor MUST match the local rustc — `make linux`
# compares the two first and fails red on a mismatch: two compilers' clippy
# are two different gates (Homebrew's rustc is not pinned; after `brew
# upgrade` this line is updated too). The Makefile reads the tag from here;
# the version lives only on this line.
FROM rust:1.88-bookworm

# `file` reports whether the release binary is statically linked. The musl
# target is the host's own architecture (arm64 containers on Apple silicon,
# x86_64 on CI): the shipped Linux binaries are static so they open on servers
# with an older glibc.
RUN apt-get update \
    && apt-get install -y --no-install-recommends file \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy \
    && rustup target add "$(uname -m)-unknown-linux-musl"

WORKDIR /w
