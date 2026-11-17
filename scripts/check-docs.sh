#!/usr/bin/env bash
# Documentation and release consistency checks. No network, no build.
#
#   1. every relative link points at an existing file;
#   2. every "#anchor" exists in the target Markdown file;
#   3. the version in Cargo.toml is the one in README, HANDOFF and CHANGELOG;
#   4. no old test layout (tests/e2e_*.sh, tests/bench_*.sh, ...) survives;
#   5. files referenced as tests/<something> actually exist;
#   6. source files named in the living documents exist, and the identifiers
#      listed after one, as in `crates/x/src/y.rs` (`function`, `Type`), are
#      still in that file. Moving code without updating the documents fails
#      here (the 29.17.0 module split left a dozen such references behind);
#   7. a bare `tests/<name>.rs` is an error: Rust tests live under a crate
#      (`bin/e-lite/tests/`, `crates/<crate>/tests/`);
#   8. an "AUD-001..N" range in a living document ends at the highest
#      finding in docs/audit/FIXED.md.
#
# Run from anywhere: the repo root is resolved from this script.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

python3 - "$ROOT" <<'PY'
import os
import re
import sys

root = sys.argv[1]
os.chdir(root)

problems = []


def slug(text: str) -> str:
    text = text.strip().lower()
    text = text.replace("`", "")
    text = re.sub(r"[^\w\- ]", "", text, flags=re.UNICODE)
    return text.replace(" ", "-")


def markdown_files():
    out = ["README.md", "AGENTS.md", "CHANGELOG.md"]
    for base, dirs, files in os.walk("docs"):
        dirs[:] = [d for d in dirs if d not in ("target", ".git")]
        for name in files:
            if name.endswith(".md"):
                out.append(os.path.join(base, name))
    return sorted(set(out))


files = markdown_files()
anchors = {}
for path in files:
    seen = {}
    found = set()
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            m = re.match(r"^(#{1,6})\s+(.*)$", line)
            if not m:
                continue
            s = slug(m.group(2))
            if s in seen:
                seen[s] += 1
                found.add(f"{s}-{seen[s]}")
            else:
                seen[s] = 0
                found.add(s)
    anchors[os.path.normpath(path)] = found

LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")
for path in files:
    with open(path, encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    for match in LINK.finditer(text):
        target = match.group(1).strip()
        if target.startswith(("http://", "https://", "mailto:", "#")):
            if target.startswith("#"):
                anchor = target[1:]
                if anchor and anchor not in anchors[os.path.normpath(path)]:
                    problems.append(f"{path}: missing anchor {target}")
            continue
        file_part, _, anchor = target.partition("#")
        if not file_part:
            resolved = os.path.normpath(path)
        else:
            resolved = os.path.normpath(os.path.join(os.path.dirname(path), file_part))
        if not os.path.exists(resolved):
            problems.append(f"{path}: missing link target {target}")
        elif anchor and resolved in anchors and anchor not in anchors[resolved]:
            problems.append(f"{path}: missing anchor {target}")

# Version consistency -------------------------------------------------------
with open("Cargo.toml", encoding="utf-8") as fh:
    cargo = fh.read()
m = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
if not m:
    problems.append("Cargo.toml: cannot read [workspace.package] version")
    version = None
else:
    version = m.group(1)

if version:
    readme = open("README.md", encoding="utf-8").read()
    if f"v{version}" not in readme:
        problems.append(f"README.md: status badge does not mention v{version}")
    handoff = open("docs/HANDOFF.md", encoding="utf-8").read()
    if version not in handoff:
        problems.append(f"docs/HANDOFF.md: does not mention {version}")
    changelog = open("CHANGELOG.md", encoding="utf-8").read()
    if not re.search(rf"^## {re.escape(version)}\b", changelog, re.MULTILINE):
        problems.append(f"CHANGELOG.md: no heading for {version}")

# Repository layout ---------------------------------------------------------
old_paths = re.compile(r"tests/(e2e|bench)_[A-Za-z0-9_]+|tests/ab_umbrel\.sh|tests/query_history\.py")
for path in files:
    with open(path, encoding="utf-8", errors="replace") as fh:
        for number, line in enumerate(fh, 1):
            if old_paths.search(line):
                problems.append(f"{path}:{number}: old test path (moved in 29.16.5)")

# Referenced test scripts must exist.
test_ref = re.compile(r"(tests/(?:e2e|bench|manual|lib)/[A-Za-z0-9_.-]+)")
for path in files:
    with open(path, encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    for ref in sorted(set(test_ref.findall(text))):
        if not os.path.exists(ref):
            problems.append(f"{path}: references missing script {ref}")

# Source references ----------------------------------------------------------
# Histories describe the tree of their time and are not checked.
historical = ("CHANGELOG.md", os.path.join("docs", "audit", "FIXED.md"))
historical_dirs = (os.path.join("docs", "adr"), os.path.join("docs", "benchmarks"))
source_ref = re.compile(r"`((?:crates|bin)/[A-Za-z0-9_\-/]+\.rs)`(?: \(([^)]*)\))?")
identifier = re.compile(r"`([A-Za-z_][A-Za-z0-9_:]*)`")
sources = {}
for path in files:
    if path in historical or path.startswith(historical_dirs):
        continue
    with open(path, encoding="utf-8", errors="replace") as fh:
        for number, line in enumerate(fh, 1):
            for source, listed in source_ref.findall(line):
                if not os.path.isfile(source):
                    problems.append(f"{path}:{number}: source file {source} does not exist")
                    continue
                if source not in sources:
                    with open(source, encoding="utf-8", errors="replace") as src:
                        sources[source] = src.read()
                for name in identifier.findall(listed):
                    if name.split("::")[-1] not in sources[source]:
                        problems.append(
                            f"{path}:{number}: `{name}` is not in {source} (moved or renamed?)"
                        )

# Rust test files are always under a crate ------------------------------------
bare_rust_test = re.compile(r"`(tests/[A-Za-z0-9_]+\.rs)`")
for path in files:
    if path in historical or path.startswith(historical_dirs):
        continue
    with open(path, encoding="utf-8", errors="replace") as fh:
        for number, line in enumerate(fh, 1):
            for ref in bare_rust_test.findall(line):
                problems.append(
                    f"{path}:{number}: `{ref}` has no crate; write bin/<bin>/{ref} or crates/<crate>/{ref}"
                )

# Fixed-finding ranges ---------------------------------------------------------
fixed_text = open(os.path.join("docs", "audit", "FIXED.md"), encoding="utf-8").read()
fixed_ids = [int(n) for n in re.findall(r"AUD-(\d+)", fixed_text)]
highest_fixed = max(fixed_ids) if fixed_ids else 0
aud_range = re.compile(r"AUD-001\.\.(\d+)")
for path in files:
    if path in historical or path.startswith(historical_dirs):
        continue
    with open(path, encoding="utf-8", errors="replace") as fh:
        for number, line in enumerate(fh, 1):
            for end in aud_range.findall(line):
                if int(end) != highest_fixed:
                    problems.append(
                        f"{path}:{number}: AUD-001..{end} but audit/FIXED.md goes up to AUD-{highest_fixed:03d}"
                    )

if problems:
    print(f"check-docs: {len(problems)} problem(s)")
    for problem in problems:
        print(f"  {problem}")
    sys.exit(1)

print(f"check-docs: OK ({len(files)} Markdown files, version {version})")
PY
