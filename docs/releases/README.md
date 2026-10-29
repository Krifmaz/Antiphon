# Release notes

To release: Actions → release → Run workflow, then pick patch, minor or major (or type a version). Every
push to `main` also publishes a hotfix build, `Stoop <version>.<run> (hotfix)`; the newest three are kept.

`release.yml` uses `docs/releases/<version>.md` (for example `0.2.0.md`) as a release's notes when it
exists. Without one, the notes are the commit list since the previous release under `## Changes`. Either
way the workflow adds the Install section with the file names, so a notes file holds only what changed.
