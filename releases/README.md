# Release notes

The release body uses `RELEASE_NOTES.md`. Give each feature a title, one or two sentences, and a link to its documentation. Keep configuration details, caveats, measurements, and implementation notes in the linked documents.

`.github/scripts/fork-release-notes.sh` reads the notes from the release commit and pins repository-relative documentation links to that commit. It appends one line identifying the Codex source version, fork release tag, and source commit.
