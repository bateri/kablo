#!/bin/sh
# What a build that is not a release adds to its version: `-dev+<commit>`, and for
# a working tree that is not that commit `-dev+<commit>.dirty-<digest>` with a digest
# of what differs (tracked changes and untracked files), so that one name never stands
# for two builds. A release is named by its version alone and does not ask this.
# Run from the repository root; it always prints a suffix (outside a repository
# `-dev+unversioned`), because an empty one would pass a development build off as the
# release.
set -u

commit=$(git rev-parse --short=12 HEAD 2>/dev/null) || {
    echo "-dev+unversioned"
    exit 0
}
if [ -z "$(git status --porcelain 2>/dev/null)" ]; then
    echo "-dev+$commit"
    exit 0
fi
if command -v sha256sum >/dev/null 2>&1; then
    sum="sha256sum"
else
    sum="shasum -a 256"
fi
digest=$(
    {
        git diff HEAD
        git ls-files -o --exclude-standard | sort | while IFS= read -r file; do
            printf '%s\n' "$file"
            cat "$file"
        done
    } | $sum | cut -c1-8
)
echo "-dev+$commit.dirty-$digest"
