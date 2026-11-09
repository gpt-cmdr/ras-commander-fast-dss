# Release procedure

The Python distribution `ras-commander-fast-dss` is the release artifact. Its version is in
`pyproject.toml` (`project.version`) and in the workspace `Cargo.toml`
(`workspace.package.version`); the two must be equal. The crates are not published
(`publish = false`).

1. Update both versions and `CHANGELOG.md`.
2. If `Cargo.lock` changed, run `python scripts/dependency_notices.py` and
   `python scripts/third_party_licenses.py`.
3. Run `python scripts/verify_sources.py --write` for intentional distribution changes, then
   `python scripts/verify_sources.py` and `python scripts/dependency_notices.py --check`.
4. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and
   `cargo test --locked`, then merge the reviewed pull request with CI green.
5. Tag the merged main commit `vX.Y.Z` and push the tag. `release.yml` checks the versions,
   builds the wheels (`build.yml`) and the sdist, smoke-tests each installed wheel, and creates
   a GitHub Release with the artifacts and `SHA256SUMS`. The wheels are Windows x86_64,
   manylinux x86_64 and aarch64, and musllinux x86_64 and aarch64. macOS is not built or
   tested in CI; macOS users build from the sdist.
6. PyPI publication runs only when the repository variable `PUBLISH_PYPI` is `true`. It uses
   the `pypi` environment: either a PyPI trusted publisher for this repository's
   `release.yml` and the `pypi` environment, or the environment secret `PYPI_API_TOKEN`. No
   token belongs in files, logs or history.
7. If registry authentication is configured after the GitHub Release exists, dispatch
   `release.yml` at the tag with `publish_existing=true`. It verifies the tag, the versions and
   the release checksums, then publishes the existing assets without rebuilding them.
8. Verify the published wheels install on Windows and Linux.

Do not retag a published version or skip a failed artifact.
