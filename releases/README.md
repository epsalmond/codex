# Release entries

Each pull request that changes user-visible behaviour on `eric/local-features`
adds one file here: `releases/YYYY-MM-DD-slug.md`. The date is the day the
entry is added, in UTC, as an absolute date (never "today" or "yesterday").

Format:

```
# Title

One-sentence summary paragraph. This becomes the changelog bullet on the
next GitHub release, so it should read as a complete sentence on its own.

Anything else — implementation notes, config keys, caveats, links to deeper
docs — goes below, in as much detail as useful.
```

- Line 1 is `# Title`.
- Line 2 is blank.
- Line 3 starts the one-sentence summary paragraph; it may wrap across more
  lines, but ends at the next blank line. `.github/scripts/fork-release-notes.sh`
  joins those lines for the release changelog, so keep it one accurate,
  self-contained sentence.
- Everything after that paragraph is free-form detail for readers who click
  through.

`.github/scripts/fork-release-notes.sh` finds the previous fork release tag,
lists every entry file added since then (`git diff --diff-filter=A --name-only
prev..release -- releases/`, skipping this README), and renders each as
`- **Title**: summary ([details](link to the file at the release commit))`.
If no entries were added since the previous release, it falls back to listing
merged fork pull requests on the first-parent path instead. Do not add a
`releases/README.md`-named entry — the script always skips it.
