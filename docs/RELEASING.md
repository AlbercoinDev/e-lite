# Releasing

The release is a tag; the workflows do the rest. Configuration, indexes and
Core datadirs are never touched by an install or update.

## 1. Prepare the tree

1. Bump `version` in `Cargo.toml` (`[workspace.package]`). Every crate uses
   `version.workspace = true`, so this is the only place. `cargo` updates
   `Cargo.lock` on the next build.
2. Add a `CHANGELOG.md` entry at the top, with the same version and date.
   Describe behaviour changes, defaults and upgrade notes; no unexplained
   internal detail.
3. Update `docs/HANDOFF.md`: latest release, tag link, "Last change" and, when
   a decision or default changed, the binding decisions/risks sections.
4. Update `docs/FEATURES.md` for anything users can now do, and
   `docs/ROADMAP.md` for anything that moved out of the future list.
5. If a default changed, say it in the changelog and in `README`/`INSTALL`
   when it affects the first run.

## 2. Gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-docs.sh
shellcheck install.sh scripts/*.sh tests/e2e/*.sh tests/bench/*.sh tests/manual/*.sh
```

Plus the e2e suites relevant to the change; before a release at least
`tests/e2e/e2e_defaults.sh`, `e2e_regtest.sh` and `e2e_snapshot.sh`. Never tag
with a red gate or an untested default change.

## 3. Commit and tag

```bash
git add -A
git commit -m "alpha.X.Y: short summary"
git tag v0.1.0-alpha.X.Y
git push origin main
git push origin v0.1.0-alpha.X.Y
```

The tag must point at the commit on `main` that passed the gates.

## 4. Workflows

- `ci` (`.github/workflows/ci.yml`) runs on every push to `main` and on pull
  requests: cached `target/` and registry, `concurrency` cancels superseded
  runs, `fmt`, `clippy`, tests, `cargo check --release`, docs and shellcheck.
  It does **not** build the release binary.
- `release` (`.github/workflows/release.yml`) runs on `v*` tags. It restores
  the packaging cache, runs `scripts/package.sh` (Debian 12 container, cached
  `CARGO_TARGET_DIR`/`~/.cargo`/`~/.rustup`), smoke-tests the tarball and the
  `.deb`, and publishes with `softprops/action-gh-release`.
- A manual `workflow_dispatch` run builds and smoke-tests **without**
  publishing; it also warms the cache before a tag. Use it when the packaging
  container or `scripts/package.sh` changed.

Artifacts: versioned and fixed-name `.deb`, tarball, `install.sh`,
`SHA256SUMS`. `install.sh` is the same one-line installer/updater used in
`README.md`.

## 5. Verify the release

```bash
gh run list --repo AlbercoinDev/e-lite --limit 3
gh release view v0.1.0-alpha.X.Y --repo AlbercoinDev/e-lite \
  --json tagName,publishedAt,assets --jq '{tagName,publishedAt,n:(.assets|length)}'
```

- `release` and `ci` green.
- Six assets, `install.sh` included.
- Release marked Latest.
- The one-line install/update commands install this version on a clean
  environment (or on the test host), leaving config and data untouched.

## Notes

- `scripts/package.sh` uses the named Docker volume `e-lite-cargo-target`
  locally; CI passes `E_LITE_TARGET_DIR`, `E_LITE_CARGO_HOME` and
  `E_LITE_RUSTUP_HOME` so the cache survives between runners.
- If a tag turns out to be wrong, do not move it: publish a new patch version.
  `git tag -f` would leave the release and the checksums inconsistent.
- The `.deb` installs templates under `/usr/share/e-lite/`; `install.sh`
  refreshes the reference copies in `~/.local/share/e-lite/`. Existing
  `config.toml`, `public-servers.toml` and `privacy.toml` are never
  overwritten.
