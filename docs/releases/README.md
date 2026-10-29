# Release notes

A file named `<version>.md` here (for example `0.2.0.md`) becomes the notes of
that release. Without one, the release lists the commits since the previous
release.

To release: Actions → CI → Run workflow, then pick patch, minor or major (or
type a version). Every push to `main` also publishes a hotfix build; the newest
three are kept.
