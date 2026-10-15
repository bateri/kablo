#!/usr/bin/env python3
"""Generates the notices for the third-party software that enters the binaries.

Usage (from the repo root):

    python3 -I tools/third_party_notices.py > THIRD-PARTY-LICENSES.txt

`make notices` does exactly that; `make dist` runs the script again and stops
when the file in the repository differs from what it prints.

Why it exists: MIT and Apache-2.0 require the notice to travel with copies, and
the `kablo` binaries are distributed. A hand-kept list silently goes stale
whenever a dependency is added, so the yardstick is `cargo tree`: `kablo`'s
**product** graph (normal + build edges; dev-dependencies are not in a binary),
the union over the four targets `make dist` builds.

Choices:
- A crate with no MIT option whose license is on the allow list
  (`OWN_LICENSES`) is given with its own text: the full Apache-2.0 text (+ NOTICE
  if any). Any other license without an MIT option stops the script: that is a
  dependency decision and is never added to the list silently (GPL-2.0-only
  least of all — kablo is GPL-3.0-or-later).
- Every remaining crate offers MIT as one of its options ("MIT OR Apache-2.0",
  "Unlicense OR MIT", ...); the notice is given under MIT — a copyright line
  per crate, the text once. The copyright line comes from the crate's own
  license file, and from `Cargo.toml`'s authors if it is not in the file.
- Two pieces of code enter the binaries without being a crate in the graph:
  the Rust standard library (offers MIT; in the same list) and, in the Linux
  binaries only, musl libc (MIT; the Rust toolchain's musl targets link the
  musl it ships statically). musl's own COPYRIGHT file is given whole,
  `tools/licenses/musl-COPYRIGHT`, because it also lists the authors and the
  few third-party parts with other licenses; it is the file of the musl
  release the toolchain builds its libc from (musl 1.2.3 for Rust 1.88:
  `src/ci/docker/scripts/musl.sh` in the Rust repository) — replace it when a
  toolchain moves to another release.
- The workspace's own crates are not third-party and are left out by
  membership, not by name.
"""

import json
import os
import subprocess
import sys

TARGETS = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
]
# The Rust version of the Linux build image whose musl `licenses/musl-COPYRIGHT`
# is (a test holds it to the image's FROM line, so that moving the toolchain
# forces the file to be looked at).
MUSL_FOR_RUST = "1.88"
ROOT = "kablo"
HERE = os.path.dirname(os.path.abspath(__file__))
# Licenses without an MIT option that are accepted, each a recorded decision
# (all GPL-3 compatible): Apache-2.0 for alacritty_terminal. Anything else
# without an MIT option stops the script.
OWN_LICENSES = {"Apache-2.0"}
# For crates whose license file carries no copyright line, the project's own
# attribution instead of the author list.
COPYRIGHT = {"alacritty_terminal": ["Copyright 2020 The Alacritty Project"]}
# Code compiled into every binary that is not a crate of the graph: the Rust
# standard library (MIT OR Apache-2.0, used under MIT).
STD_MIT = [
    ("rust-std (the Rust standard library, compiled into every binary)",
     ["Copyright (c) The Rust Project Contributors",
      "https://github.com/rust-lang/rust",
      "The third-party components it bundles are listed with their licenses in",
      "the COPYRIGHT file of that repository."]),
]
MUSL_COPYRIGHT = os.path.join(HERE, "licenses", "musl-COPYRIGHT")
MIT_TEXT = """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""
RULE = "=" * 80


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def graph_crates(target):
    """The (name, version) pairs in `kablo`'s product graph for one target."""
    out = run("cargo", "tree", "-p", ROOT, "-e", "normal,build", "--locked",
              "--target", target, "--prefix", "none", "-f", "{p}")
    crates = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            crates.add((parts[0], parts[1].lstrip("v")))
    return crates


def metadata(target):
    meta = json.loads(run("cargo", "metadata", "--format-version", "1", "--locked",
                          "--filter-platform", target))
    members = set(meta["workspace_members"])
    return meta["packages"], members


def copyright_lines(pkg):
    """The copyright lines in the crate's license file; if none, one line from the authors."""
    if pkg["name"] in COPYRIGHT:
        return COPYRIGHT[pkg["name"]]
    root = os.path.dirname(pkg["manifest_path"])
    for name in ("LICENSE-MIT", "LICENSE-MIT.md", "LICENSE-MIT.txt", "LICENSE.MIT",
                 "LICENSE", "LICENSE.md", "LICENSE.txt", "COPYING"):
        path = os.path.join(root, name)
        if not os.path.isfile(path):
            continue
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = [l.strip() for l in f if l.strip().lower().startswith("copyright")]
        # A line without an owner's name ("Copyright (c) 2016--2017") and a
        # redirect file (Unlicense/COPYING) do not count as a notice; the next
        # candidate, and the authors last.
        lines = [l for l in lines
                 if any(ch.isalpha() for ch in l[9:].replace("(c)", "").replace("(C)", ""))]
        if lines:
            return lines
    authors = [a.split(" <")[0] for a in pkg.get("authors") or []]
    return [f"Copyright (c) {', '.join(authors) or 'the ' + pkg['name'] + ' developers'}"]


def offers_mit(pkg):
    expr = (pkg.get("license") or "").replace("(", " ").replace(")", " ").replace("/", " ")
    words = expr.split()
    # `/` is the old SPDX-less "or" (`Apache-2.0/MIT`). An AND means the other
    # license applies too: that is a decision, not something to read as "or".
    if "AND" in words:
        sys.exit(f"{pkg['name']} {pkg['version']}: license '{pkg.get('license')}' "
                 "combines licenses with AND; read it and decide")
    return "MIT" in words


def own_license(pkg):
    """The accepted non-MIT license of `pkg`, or None if it offers MIT."""
    if offers_mit(pkg):
        return None
    expr = (pkg.get("license") or "").strip()
    if expr not in OWN_LICENSES:
        sys.exit(f"{pkg['name']} {pkg['version']}: license '{expr}' neither offers "
                 "MIT nor is on the allow list")
    return expr


def read_first(root, names):
    for name in names:
        path = os.path.join(root, name)
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as f:
                return f.read().rstrip()
    return None


def main():
    if len(sys.argv) != 1:
        sys.exit("usage: third_party_notices.py (no arguments; the notice goes to stdout)")
    pkgs = {}
    ours = set()
    crates = set()
    for target in TARGETS:
        packages, members = metadata(target)
        for p in packages:
            pkgs[(p["name"], p["version"])] = p
            if p["id"] in members:
                ours.add((p["name"], p["version"]))
        crates |= graph_crates(target)
    crates = sorted(crates - ours)
    if not crates:
        sys.exit("the product graph came out empty")

    out = ["kablo includes, among others, the following third-party software.", ""]

    for name, version in crates:
        pkg = pkgs.get((name, version))
        if pkg is None:
            sys.exit(f"{name} {version}: in the graph but not in the metadata")
        license = own_license(pkg)
        if license is None:
            continue
        root = os.path.dirname(pkg["manifest_path"])
        text = read_first(root, ("LICENSE-APACHE", "LICENSE-APACHE.md", "LICENSE-APACHE.txt",
                                 "LICENSE", "LICENSE.md", "LICENSE.txt"))
        if text is None or "Apache License" not in text:
            sys.exit(f"{name} {version}: Apache-2.0 text not found")
        notice = read_first(root, ("NOTICE", "NOTICE.md", "NOTICE.txt"))
        out += [RULE, name, pkg.get("repository") or "", *copyright_lines(pkg),
                "Licensed under the Apache License, Version 2.0; full text below.",
                RULE, "", *([notice, ""] if notice else []), text, "", ""]

    out += [RULE,
            "The following Rust crates, and the Rust standard library, are used under",
            "the MIT License (each offers MIT, alone or as one of its license options).",
            "The license text follows the list.",
            RULE, ""]
    for name, version in crates:
        pkg = pkgs[(name, version)]
        if own_license(pkg) is not None:
            continue
        out += [f"{name} {version}", *(f"  {l}" for l in copyright_lines(pkg))]
        if pkg.get("repository"):
            out.append(f"  {pkg['repository']}")
        out.append("")
    for title, lines in STD_MIT:
        out += [title, *(f"  {l}" for l in lines), ""]
    out += ["MIT License", "", MIT_TEXT, "", ""]

    with open(MUSL_COPYRIGHT, encoding="utf-8") as f:
        musl = f.read().rstrip()
    out += [RULE, "musl libc", "https://musl.libc.org",
            "Linked statically into the Linux binaries (the Rust toolchain's musl",
            "targets carry it); its COPYRIGHT file follows as it is.", RULE, "", musl, ""]

    sys.stdout.reconfigure(encoding="utf-8")
    sys.stdout.write("\n".join(out))


if __name__ == "__main__":
    main()
