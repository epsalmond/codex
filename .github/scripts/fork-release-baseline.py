#!/usr/bin/env python3
"""Select a published first-parent ancestor from paginated GitHub release JSON."""

import json
import subprocess
import sys


def main():
    repo, source, target_tag = sys.argv[1:]

    def git(*args):
        return subprocess.check_output(["git", "-C", repo, *args], text=True).strip()

    ancestry = {
        sha: distance
        for distance, sha in enumerate(
            git("rev-list", "--first-parent", source).splitlines()
        )
    }
    pages = json.load(sys.stdin)
    candidates = []
    for page in pages:
        for release in page:
            tag = release["tag_name"]
            published = release.get("published_at")
            if (
                release.get("draft")
                or not published
                or tag == target_tag
                or not tag.startswith("local-features-")
            ):
                continue
            result = subprocess.run(
                [
                    "git",
                    "-C",
                    repo,
                    "rev-parse",
                    "--verify",
                    f"refs/tags/{tag}^{{commit}}",
                ],
                text=True,
                capture_output=True,
            )
            distance = (
                ancestry.get(result.stdout.strip()) if result.returncode == 0 else None
            )
            if distance is not None:
                # Nearest source first; latest publication breaks same-source
                # ties so a true rebuild can report zero source changes.
                candidates.append((-distance, published, tag))
    print(max(candidates)[2] if candidates else "")


if __name__ == "__main__":
    main()
