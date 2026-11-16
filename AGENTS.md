# Instructions for coding agents

These rules apply to automated coding agents working in this repository. Contributors
should read `docs/releasing.md` and `CHANGELOG.md` as well.

## GitHub Actions runners

- **Routine CI is Linux and Windows only.** `ci.yml` runs on pushes to `main` and on pull
  requests, on GitHub-hosted Linux and Windows runners. Do not add macOS, or any other
  runner type, to routine CI.
- **Approved release builds may use GitHub-hosted runners, including macOS,** only after
  the maintainer approves each release run through the protected `pypi` environment.
  - `release.yml` runs only when the maintainer publishes a GitHub Release. Nothing in it
    runs on a push, a tag push, a schedule, a dispatch or another workflow's completion.
  - Every job of `release.yml` and of `build.yml`, the wheel builds (macOS included), the
    sdist and the publish job, names `environment: pypi`. The environment's required
    reviewer must approve before any of those runners start. A gate on the publish job
    alone would not gate the builds, so do not remove the environment from a build job.
  - `build.yml` has no trigger of its own; only `release.yml` calls it.
- **Agents never start hosted jobs on their own.** Do not push, open or update a pull
  request, publish a release, dispatch, re-run or approve a run unless the maintainer has
  approved that specific run. A request to implement, test or commit something is not
  such an approval. Approval of one run does not cover retries or later runs.
- Do not add automatic hosted execution (push, tag, schedule or workflow chaining) to the
  release path, and do not loosen the environment gate.

## Releases

- Release notes are written by hand in the GitHub Release. Do not generate them
  automatically, and do not put `@`-mentions in them.
- Publish only the artifacts the release workflow builds; never upload locally built wheels.
- A release wheel must not contain fault-injection entry points. `tests/smoke_wheel.py` and
  `tests/python/test_release_surface.py` check this; keep both.
