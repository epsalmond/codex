# Release page leads with a changelog and a fork overview

The GitHub release body now opens with a short changelog since the previous
fork release, followed by a short overview of what the fork adds, before the
install block, provenance, and the full upstream commit list.

Previously the release body led with the install block and buried the
changelog-equivalent information (a long first-parent commit list against the
upstream stable tag) at the bottom, with no per-release summary of what
changed on the fork itself. The new `releases/` directory holds one entry per
behaviour-changing PR (see `releases/README.md`); `fork-release-notes.sh`
renders the entries added since the previous `local-features-v*` release tag
as the changelog, falling back to a list of merged fork PRs when no entries
were added. The long upstream-and-fork commit list is now collapsed under
`<details>`, with the compare link kept visible outside it.
