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
5. Write the release notes by hand (no `@`-mentions, no generated notes), then create the
   GitHub Release for the tag `vX.Y.Z` on the merged main commit, first as a draft and then
   published. Publishing it is the only trigger of `release.yml`: a push, a tag push or a
   dispatch starts nothing in it.
6. Every job of `release.yml` and of the `build.yml` it calls runs on a GitHub-hosted runner
   and is attached to the protected `pypi` environment, whose required reviewer is the
   maintainer and whose deployment rule allows only `v*` tags. Approve the waiting jobs
   twice:
   - **Builds:** the wheels (Windows x86_64, manylinux x86_64 and aarch64, musllinux x86_64
     and aarch64, macOS arm64 and x86_64) and the sdist. Each job first checks the tag
     against `Cargo.toml` and `pyproject.toml` (`scripts/check_release_tag.py`) and
     smoke-tests its installed artifact. Each macOS wheel (separate wheels; deployment
     targets 11.0 and 10.12) is checked to link only system libraries (`delocate-listdeps`,
     `otool`) and runs the Python suite against the installed release wheel. The README
     states the dependency (numpy) and tested-OS caveats; keep them current.
   - **Publish:** once every build passed, one job attaches the artifacts and `SHA256SUMS` to
     the published release and, when the repository variable `PUBLISH_PYPI` is `true`,
     uploads them to PyPI: either through a PyPI trusted publisher for this repository's
     `release.yml` and the `pypi` environment, or with the environment secret
     `PYPI_API_TOKEN`. No token belongs in files, logs or history.
7. Routine CI (`ci.yml`) stays on Linux and Windows. If a release job fails, fix the cause
   and re-run only that job, which asks for approval again; never rebuild or upload an
   artifact outside the workflow. The publish job is safe to re-run
   (`scripts/release_assets.py`):
   - Release assets already attached with the same bytes are kept; missing ones are
     uploaded. An asset is never replaced: one with other bytes, an incomplete upload, or a
     file this run did not build fails the job before anything is uploaded. To recover,
     delete that asset from the release by hand and re-run the job.
   - Re-run **only the publish job** of the same workflow run, so it uses the same build
     artifacts. Re-running the build jobs can produce wheels whose bytes differ from the
     attached ones, and the publish job then refuses them.
   - To publish to PyPI after a run with `PUBLISH_PYPI` unset, set it to `true` and re-run
     the publish job. Files already on PyPI for the version must be byte-identical to the
     build; the upload skips them and adds the rest, and a final check requires every
     distribution on PyPI with the same SHA-256.
8. Verify the published wheels install on Windows, Linux and macOS.

Do not retag a published version or skip a failed artifact.
