#!/bin/sh
# The work of `make dist` (the Makefile says which targets, which names and
# which toolchain; this is how). Run from the repository root.
#
# dist/ is rebuilt from nothing and SHA256SUMS is written last, only when all
# the binaries are there and each has been looked at, and run where this
# machine can run it: a dist/ without SHA256SUMS is a build that did not
# finish, and a stale binary can never pass for a new one. The licenses go in
# next to the binaries and are summed with them, so what a release carries
# is exactly what SHA256SUMS lists.
#
# With DIST_SIGN_ID (a release) the macOS binaries are signed with that
# identity, with the hardened runtime and a secure timestamp, before they are
# run; with DIST_NOTARY_PROFILE too they are then notarized by Apple
# (`notarytool`, that keychain profile), and Apple's ticket has to cover each
# binary as it is in dist/.
# A bare executable cannot have its ticket stapled, so a Mac that opens one a
# browser saved asks Apple about it once, online; one fetched with curl or
# copied over ssh carries no quarantine and is not asked about. The sums come
# after all of it: signing changes the files.
set -eu

: "${DIST_VERSION:?}" "${DIST_NAME:?}" "${DIST_SUMS:?}" "${DIST_IMAGE:?}" \
    "${DIST_DOCKERFILE:?}" "${DIST_TOOLCHAIN:?}" \
    "${DIST_LINUX_TARGETS:?}" "${DIST_MACOS_TARGETS:?}" "${DIST_LICENSES:?}"
sign_id=${DIST_SIGN_ID:-}
notary_profile=${DIST_NOTARY_PROFILE:-}

fail() {
    echo "dist: $*" >&2
    exit 1
}

# The file name of the binary for target $1.
name_of() {
    printf '%s\n' "$DIST_NAME" | sed "s/%/$1/"
}

# $1 is the output of `kablo version`: it must be this version's.
check_version() {
    case $1 in
        "version=$DIST_VERSION "*) ;;
        *) fail "$2 says '$1', not version $DIST_VERSION" ;;
    esac
}

[ -z "$notary_profile" ] || [ -n "$sign_id" ] || fail "notarizing needs signed binaries (DIST_SIGN_ID)"
if [ -n "$sign_id" ]; then
    security find-identity -v -p codesigning | grep -qF "\"$sign_id\"" ||
        fail "there is no valid code-signing identity named '$sign_id' in the keychain (security find-identity -v -p codesigning)"
fi

# The macOS binaries come from the rustup toolchain: Homebrew's rustc cannot
# add the standard library of a target. Look before building anything.
[ "$(uname -s)" = Darwin ] || fail "the macOS binaries are built on a Mac"
command -v rustup >/dev/null 2>&1 || fail "rustup is missing (the macOS targets need its toolchain: https://rustup.rs)"
installed=$(rustup target list --installed --toolchain "$DIST_TOOLCHAIN") ||
    fail "rustup has no toolchain '$DIST_TOOLCHAIN'"
# The toolchain's own compiler, named: `rustup run` leaves a Homebrew rustc
# that is earlier on PATH in front of it, and that one has no Apple targets
# but its own.
cargo=$(rustup which cargo --toolchain "$DIST_TOOLCHAIN")
rustc=$(rustup which rustc --toolchain "$DIST_TOOLCHAIN")
for target in $DIST_MACOS_TARGETS; do
    printf '%s\n' "$installed" | grep -qx "$target" ||
        fail "the '$DIST_TOOLCHAIN' toolchain has no standard library for $target; run: rustup target add $target --toolchain $DIST_TOOLCHAIN"
done

for file in $DIST_LICENSES; do
    [ -s "$file" ] || fail "$file is missing or empty (it ships in dist/)"
done

rm -rf dist
mkdir dist
# shellcheck disable=SC2086 # the names are words on purpose
cp $DIST_LICENSES dist/

# Linux: a static musl binary, built and then opened in a container of its own
# architecture. The other architecture than the machine's runs emulated, which
# is slow the first time and exact. Each architecture has a target directory of
# its own: the build scripts of one cannot run in the other.
for target in $DIST_LINUX_TARGETS; do
    arch=${target%%-*}
    case $arch in
        x86_64) platform=linux/amd64 want='x86-64' ;;
        aarch64) platform=linux/arm64 want='ARM aarch64' ;;
        *) fail "no container platform for $target" ;;
    esac
    image="$DIST_IMAGE-$arch"
    name=$(name_of "$target")
    echo "==> $target, in a $platform container"
    docker build -q --platform "$platform" -t "$image" -f "$DIST_DOCKERFILE" tools/linux >/dev/null
    docker run --rm --platform "$platform" \
        -v "$PWD":/w -v kablo-linux-cargo:/usr/local/cargo/registry \
        -e CARGO_TARGET_DIR="/w/target/dist-$arch" "$image" \
        cargo build --release --locked -p kablo --target "$target"
    cp "target/dist-$arch/$target/release/kablo" "dist/$name"

    described=$(docker run --rm --platform "$platform" -v "$PWD/dist":/dist:ro "$image" file "/dist/$name")
    echo "$described"
    case $described in
        *"statically linked"* | *"static-pie linked"*) ;;
        *) fail "dist/$name is not statically linked" ;;
    esac
    case $described in
        *"$want"*) ;;
        *) fail "dist/$name is not a $arch binary" ;;
    esac
    shown=$(docker run --rm --platform "$platform" -v "$PWD/dist":/dist:ro "$image" "/dist/$name" version)
    echo "$shown"
    check_version "$shown" "dist/$name"
done

# macOS: cross-built by one compiler for both targets, in a target directory
# that is not the one `make check` fills with Homebrew's. A binary is run when
# this Mac can run its architecture (Rosetta for x86_64); otherwise it is only
# looked at, and the line says so.
for target in $DIST_MACOS_TARGETS; do
    case $target in
        aarch64-*) arch=arm64 ;;
        x86_64-*) arch=x86_64 ;;
        *) fail "no architecture for $target" ;;
    esac
    name=$(name_of "$target")
    echo "==> $target, with $("$rustc" --version)"
    CARGO_TARGET_DIR=target/dist RUSTC="$rustc" \
        "$cargo" build --release --locked -p kablo --target "$target"
    cp "target/dist/$target/release/kablo" "dist/$name"
    if [ -n "$sign_id" ]; then
        # Over the linker's ad-hoc signature. What notarization asks for: a
        # Developer ID, the hardened runtime and a timestamp from Apple.
        codesign --force --options runtime --timestamp --sign "$sign_id" "dist/$name"
        codesign --verify --strict "dist/$name" || fail "dist/$name does not verify after signing"
        signed=$(codesign -dv "dist/$name" 2>&1)
        case $signed in
            *"flags="*"(runtime)"*) ;;
            *) fail "dist/$name is not signed with the hardened runtime" ;;
        esac
        printf '%s\n' "$signed" | grep -q '^Timestamp=' || fail "dist/$name has no secure timestamp"
        echo "signed: dist/$name ($sign_id)"
    fi

    described=$(file "dist/$name")
    echo "$described"
    case $described in
        *"Mach-O 64-bit executable $arch"*) ;;
        *) fail "dist/$name is not a $arch binary" ;;
    esac
    if arch "-$arch" /usr/bin/true >/dev/null 2>&1; then
        shown=$(arch "-$arch" "dist/$name" version)
        echo "$shown"
        check_version "$shown" "dist/$name"
    else
        echo "dist: dist/$name was not run: this Mac cannot run $arch binaries"
    fi
done

# Notarization: both binaries in one submission, waited for. A refusal stops
# the build with Apple's own account of why.
if [ -n "$notary_profile" ]; then
    work=target/dist/notarize
    rm -rf "$work"
    mkdir -p "$work/kablo"
    for target in $DIST_MACOS_TARGETS; do
        cp "dist/$(name_of "$target")" "$work/kablo/"
    done
    ditto -c -k --keepParent "$work/kablo" "$work/kablo.zip"
    echo "==> notarizing the macOS binaries (notarytool, profile $notary_profile)"
    answer=$(xcrun notarytool submit "$work/kablo.zip" --keychain-profile "$notary_profile" \
        --wait --output-format json) || fail "notarytool could not submit or wait: $answer"
    status=$(printf '%s' "$answer" | plutil -extract status raw -o - -) || fail "notarytool said: $answer"
    id=$(printf '%s' "$answer" | plutil -extract id raw -o - -) || id=
    if [ "$status" != Accepted ]; then
        [ -z "$id" ] || xcrun notarytool log "$id" --keychain-profile "$notary_profile" >&2 || true
        fail "Apple did not accept the macOS binaries: $status (submission $id)"
    fi
    echo "notarized: submission $id"
    # What Apple's ticket covers: the code-directory hash of each binary as it
    # is in dist/ must be in the submission's log, or what ships is not what
    # was notarized. That is the check, not Gatekeeper's verdict on this Mac:
    # with no ticket stapled to it, Gatekeeper answers from what it has looked
    # up before, and right after a submission that is "unnotarized" for a
    # while. Its verdict is printed, and is no reason to stop.
    xcrun notarytool log "$id" --keychain-profile "$notary_profile" "$work/log.json" >/dev/null ||
        fail "notarytool could not fetch the log of submission $id"
    for target in $DIST_MACOS_TARGETS; do
        name=$(name_of "$target")
        cdhash=$(codesign -dvvv "dist/$name" 2>&1 | sed -n 's/^CDHash=//p')
        [ -n "$cdhash" ] || fail "dist/$name has no code-directory hash"
        grep -qF "\"$cdhash\"" "$work/log.json" ||
            fail "Apple's ticket for submission $id does not cover dist/$name ($cdhash)"
        echo "notarized: dist/$name ($cdhash)"
        echo "gatekeeper on this Mac, for now: $(spctl --assess --type execute --verbose=2 "dist/$name" 2>&1 | tr '\n' ' ')"
    done
    rm -rf "$work"
fi

# The sums, from inside dist/ so the names in the file are the names there.
if command -v sha256sum >/dev/null 2>&1; then
    sum="sha256sum"
else
    sum="shasum -a 256"
fi
names=""
for target in $DIST_LINUX_TARGETS $DIST_MACOS_TARGETS; do
    names="$names $(name_of "$target")"
done
names="$names $DIST_LICENSES"
(
    cd dist
    # Written under another name and renamed once it checks out, so that a
    # file called SHA256SUMS is always a whole one.
    # shellcheck disable=SC2086 # the names are words on purpose
    $sum $names >"$DIST_SUMS.partial"
    $sum -c "$DIST_SUMS.partial"
    mv "$DIST_SUMS.partial" "$DIST_SUMS"
)
echo "==> dist/ holds $(wc -l <"dist/$DIST_SUMS" | tr -d ' ') files, all in $DIST_SUMS"
