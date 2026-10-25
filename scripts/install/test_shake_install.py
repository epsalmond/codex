#!/usr/bin/env python3

import hashlib
import json
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


INSTALL_SCRIPT = Path(__file__).parents[2] / "install.sh"
TAG = "local-features-v0.154.0-r202609131200.abcdef1"
ASSET = "codex-aarch64-apple-darwin.tar.gz"
PREVIOUS_SOURCE = "9eaf6d018de977d1e67475204e900ca79564a5d3"
TARGET_SOURCE = "4f39ba3e322901fd202eeb85bd5fdad85cd0d994"
SOURCE_RANGE = f"{PREVIOUS_SOURCE}...{TARGET_SOURCE}"


class ShakeInstallTest(unittest.TestCase):
    def test_default_paths_install_both_wrappers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, home_dir, bin_dir, _ = run_installer(root)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((bin_dir / "codex-shake").is_file())
            self.assertTrue((bin_dir / "codex-shake-update").is_file())
            self.assertFalse((bin_dir / "codex-shake-estimate").exists())
            self.assertEqual(os.readlink(home_dir / "current"), TAG)

    def test_latest_selection_uses_maximum_release_identity(self) -> None:
        release_feed = json.dumps(
            [
                {
                    "tag_name": "local-features-v0.154.0-main-r202609140000.aaaaaaaaaaaa",
                    "draft": False,
                },
                {
                    "tag_name": "local-features-v0.154.0-main-r20260914000000.0000000000000001ffffffffffff",
                    "draft": False,
                },
                {
                    "tag_name": "local-features-v0.154.0-main-r20260914000000.0000000000000002aaaaaaaaaaaa",
                    "draft": False,
                },
                {
                    "tag_name": "local-features-v0.154.0-main-r20260914000000.0000000000000002.aaaaaaaaaaaa",
                    "draft": False,
                },
                {
                    "tag_name": "local-features-v9.9.9-r99999999999999.cccccccccccc",
                    "draft": True,
                },
                {
                    "tag_name": "local-features-vnot-a-release-r1.deadbeef",
                    "draft": False,
                },
            ]
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, home_dir, _, _ = run_installer(
                root,
                tag=None,
                release_feed=release_feed,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                os.readlink(home_dir / "current"),
                "local-features-v0.154.0-main-r20260914000000.0000000000000002aaaaaaaaaaaa",
            )

    def test_latest_selection_treats_source_sequence_hex_as_numeric(self) -> None:
        release_feed = json.dumps(
            [
                {
                    "tag_name": "local-features-v0.154.0-main-r20260914000000.000000000000000Baaaaaaaaaaaa",
                    "draft": False,
                },
                {
                    "tag_name": "local-features-v0.154.0-main-r20260914000000.000000000000000abbbbbbbbbbbb",
                    "draft": False,
                },
            ]
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, home_dir, _, _ = run_installer(
                root,
                tag=None,
                release_feed=release_feed,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                os.readlink(home_dir / "current"),
                "local-features-v0.154.0-main-r20260914000000.000000000000000Baaaaaaaaaaaa",
            )

    def test_latest_selection_accepts_fourteen_digits_and_ignores_fifteen(self) -> None:
        accepted = "local-features-v0.154.0-main-r20260914000000.abcdef123456"
        rejected = "local-features-v0.154.0-main-r202609140000000.ffffffffffff"
        release_feed = json.dumps(
            [
                {"tag_name": rejected, "draft": False},
                {"tag_name": accepted, "draft": False},
            ]
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, home_dir, _, _ = run_installer(
                root,
                tag=None,
                release_feed=release_feed,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(os.readlink(home_dir / "current"), accepted)

    def test_latest_selection_orders_build_runs_and_attempts(self) -> None:
        prefix = "local-features-v0.154.0-main-r20260914000000."
        legacy = prefix + "0000000000000002" + "a" * 12
        run_one = legacy + "0000000000000001" + "00000001"
        run_two = legacy + "0000000000000002" + "00000001"
        attempt_two = legacy + "0000000000000002" + "00000002"
        later_source = (
            prefix + "0000000000000003" + "b" * 12 + "0000000000000001" + "00000001"
        )
        for tags, expected in [
            ([legacy, run_one], run_one),
            ([run_two, run_one], run_two),
            ([run_two, attempt_two], attempt_two),
            ([attempt_two, later_source], later_source),
        ]:
            with self.subTest(tags=tags), tempfile.TemporaryDirectory() as temp_dir:
                result, home_dir, _, _ = run_installer(
                    Path(temp_dir),
                    tag=None,
                    release_feed=json.dumps(
                        [{"tag_name": tag, "draft": False} for tag in tags]
                    ),
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(os.readlink(home_dir / "current"), expected)

    def test_new_install_explains_missing_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result, _, _, _ = run_installer(Path(temp_dir))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("previous release baseline unavailable", result.stderr)
            self.assertIn(
                f"https://github.com/owner/repo/releases/tag/{TAG}", result.stderr
            )

    def test_already_current_does_not_fetch_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, _, _, _ = run_installer(root, previous_tag=TAG)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"already current: {TAG}", result.stderr)
            self.assertNotIn("/compare/", (root / "requests.log").read_text())

    @unittest.skipUnless(shutil.which("jq"), "jq needed for structured summary checks")
    def test_update_range_includes_skipped_builds_and_treats_titles_as_data(
        self,
    ) -> None:
        previous = "local-features-v0.154.0-r202609101200.abcdef1"
        message = "Fix updater $(touch never-execute) `touch never-execute`\x1b[31m\nDetailed body"
        comparison = {
            "status": "ahead",
            "total_commits": 3,
            "commits": [
                {"commit": {"message": "Fix from skipped build"}},
                {"commit": {"message": "Upstream: improve tools"}},
                {"commit": {"message": message}},
            ],
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, home_dir, _, _ = run_installer(
                root, previous_tag=previous, comparison=comparison
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(os.readlink(home_dir / "current"), TAG)
            self.assertIn("including skipped builds", result.stderr)
            self.assertIn("Fix from skipped build", result.stderr)
            self.assertIn("Upstream: improve tools", result.stderr)
            self.assertIn("$(touch never-execute) `touch never-execute`", result.stderr)
            self.assertNotIn("\x1b", result.stderr)
            self.assertNotIn("Detailed body", result.stderr)
            self.assertFalse((root / "never-execute").exists())
            requests = (root / "requests.log").read_text().splitlines()
            self.assertEqual(
                [url for url in requests if "/compare/" in url],
                [
                    f"https://api.github.com/repos/owner/repo/compare/{SOURCE_RANGE}?per_page=100"
                ],
            )

    @unittest.skipUnless(shutil.which("jq"), "jq needed for structured summary checks")
    def test_notes_outage_and_malformed_response_do_not_fail_install(self) -> None:
        for failure in (True, False):
            with (
                self.subTest(outage=failure),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, home_dir, _, _ = run_installer(
                    Path(temp_dir),
                    previous_tag="old-build",
                    comparison_failure=failure,
                    comparison={"message": "not a comparison"},
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(os.readlink(home_dir / "current"), TAG)
                self.assertIn(
                    "comparison unavailable; installation succeeded",
                    result.stderr,
                )
                self.assertIn(f"/releases/tag/{TAG}", result.stderr)
                self.assertNotIn("/compare/", result.stderr)
                self.assertNotIn("curl:", result.stderr)

    @unittest.skipUnless(shutil.which("jq"), "jq needed for structured summary checks")
    def test_large_range_fetches_final_page_and_shows_newest_fork_fix(self) -> None:
        for total in (100, 101, 350):
            with self.subTest(total=total), tempfile.TemporaryDirectory() as temp_dir:
                comparison = {
                    "status": "ahead",
                    "total_commits": total,
                    "commits": [
                        {"commit": {"message": f"Old upstream change {i}"}}
                        for i in range(1, 100)
                    ]
                    + [{"commit": {"message": "Fresh fork fix"}}],
                }
                page = (total + 99) // 100
                final_commits = total - 100 * (page - 1)
                last_page = {
                    "status": "ahead",
                    "total_commits": total,
                    "commits": [
                        {"commit": {"message": f"Recent change {i}"}}
                        for i in range(final_commits - 1)
                    ]
                    + [{"commit": {"message": "Fresh fork fix"}}],
                }
                root = Path(temp_dir)
                result, _, _, _ = run_installer(
                    root,
                    previous_tag="old-build",
                    comparison=comparison,
                    comparison_last_page=last_page,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("  - Fresh fork fix\n", result.stderr)
                self.assertNotIn("  - Old upstream change 1\n", result.stderr)
                shown = min(12, final_commits)
                self.assertIn(
                    f"Showing {shown} of {total} commits (newest first)", result.stderr
                )
                requests = [
                    url
                    for url in (root / "requests.log").read_text().splitlines()
                    if "/compare/" in url
                ]
                self.assertEqual(len(requests), 2 if total > 100 else 1)
                if total > 100:
                    self.assertTrue(requests[-1].endswith(f"&page={page}"))
                    self.assertNotIn("Old upstream change", result.stderr)

    @unittest.skipUnless(shutil.which("jq"), "jq needed for structured summary checks")
    def test_failed_or_malformed_final_page_does_not_show_old_titles(self) -> None:
        for failure in (True, False):
            with (
                self.subTest(outage=failure),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, home_dir, _, _ = run_installer(
                    Path(temp_dir),
                    previous_tag="old-build",
                    comparison={
                        "status": "ahead",
                        "total_commits": 350,
                        "commits": [{"commit": {"message": "Old title"}}],
                    },
                    comparison_last_page={
                        "status": "ahead",
                        "total_commits": 350,
                        "commits": [],
                    },
                    comparison_last_page_failure=failure,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(os.readlink(home_dir / "current"), TAG)
                self.assertIn(
                    "summary unavailable; installation succeeded", result.stderr
                )
                self.assertIn(f"/compare/{SOURCE_RANGE}", result.stderr)
                self.assertNotIn("Old title", result.stderr)
                self.assertNotIn("newest first", result.stderr)

    @unittest.skipUnless(shutil.which("jq"), "jq needed for structured summary checks")
    def test_same_source_rebuild_and_rollback_are_explicit(self) -> None:
        for status, expected in [
            ("identical", "No source changes (rebuild of the same source)."),
            ("behind", "Source histories are behind;"),
        ]:
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temp_dir:
                result, _, _, _ = run_installer(
                    Path(temp_dir),
                    previous_tag="old-build",
                    comparison={"status": status, "total_commits": 0, "commits": []},
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(expected, result.stderr)

    def test_without_jq_selects_release_and_links_entire_update_range(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, home_dir, _, _ = run_installer(
                root,
                tag=None,
                without_jq=True,
                previous_tag="old-build",
                release_feed=json.dumps([{"tag_name": TAG, "draft": False}]),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(os.readlink(home_dir / "current"), TAG)
            self.assertIn("changelog summary unavailable without jq", result.stderr)
            self.assertIn(f"/compare/{SOURCE_RANGE}", result.stderr)
            self.assertIn(
                f"/compare/{SOURCE_RANGE}?per_page=100",
                (root / "requests.log").read_text(),
            )
            self.assertNotIn('"commits"', result.stdout + result.stderr)

    def test_inaccessible_source_comparison_never_emits_broken_link(self) -> None:
        for without_jq in (False, True):
            with (
                self.subTest(without_jq=without_jq),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                result, home_dir, _, _ = run_installer(
                    root,
                    previous_tag="unpushed-local-build",
                    without_jq=without_jq,
                    comparison_failure=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(os.readlink(home_dir / "current"), TAG)
                self.assertIn("comparison unavailable", result.stderr)
                self.assertIn(f"/releases/tag/{TAG}", result.stderr)
                self.assertNotIn("/compare/", result.stderr)
                self.assertNotIn("curl:", result.stderr)
                self.assertIn(
                    f"/compare/{SOURCE_RANGE}?per_page=100",
                    (root / "requests.log").read_text(),
                )

    @unittest.skipUnless(shutil.which("jq"), "jq needed for structured summary checks")
    def test_unpublished_local_build_compares_embedded_full_source_commits(
        self,
    ) -> None:
        previous = (
            "local-features-v0.163.0-main-r202610222029."
            "00000000000029529eaf6d018de9000000000000000100000000"
        )
        target = (
            "local-features-v0.163.0-main-r20261025061926."
            "00000000000029514f39ba3e322900000000000000c900000001"
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, _, _, _ = run_installer(
                root,
                previous_tag=previous,
                tag=target,
                previous_version=(
                    f"codex-cli 0.163.0\nShake feature version: 0.4.0\nCLI build: local 1\n"
                    f"Source commit: {PREVIOUS_SOURCE}\nRelease tag: {previous}\n"
                ),
                target_version=(
                    f"codex-cli 0.163.0\nShake feature version: 0.4.0\nCLI build: 201.1\n"
                    f"Source commit: {TARGET_SOURCE}\nRelease tag: {target}\n"
                ),
                comparison={
                    "status": "ahead",
                    "total_commits": 1,
                    "commits": [
                        {"commit": {"message": "Recovered local build history"}}
                    ],
                },
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Recovered local build history", result.stderr)
            self.assertIn(f"/compare/{SOURCE_RANGE}", result.stderr)
            requests = (root / "requests.log").read_text().splitlines()
            self.assertEqual(
                [url for url in requests if "api.github.com" in url],
                [
                    f"https://api.github.com/repos/owner/repo/compare/{SOURCE_RANGE}?per_page=100"
                ],
            )
            self.assertNotIn(f"/compare/{previous}", result.stderr)

    @unittest.skipUnless(shutil.which("jq"), "jq needed for legacy identity lookup")
    def test_legacy_binaries_resolve_published_tags_to_full_commits(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, _, _, _ = run_installer(
                root,
                previous_tag="legacy-build",
                previous_version="codex-cli 0.154.0",
                target_version="codex-cli 0.154.0",
                commit_sources={
                    "legacy-build": {"sha": PREVIOUS_SOURCE},
                    TAG: {"sha": TARGET_SOURCE},
                },
                comparison={"status": "identical", "total_commits": 0, "commits": []},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("No source changes", result.stderr)
            self.assertIn(f"/compare/{SOURCE_RANGE}", result.stderr)
            requests = (root / "requests.log").read_text()
            self.assertIn("/commits/legacy-build", requests)
            self.assertIn(f"/commits/{TAG}", requests)
            self.assertIn(f"/compare/{SOURCE_RANGE}?per_page=100", requests)
            self.assertEqual(requests.count("api.github.com"), 3)

    @unittest.skipUnless(shutil.which("jq"), "jq needed for legacy identity lookup")
    def test_invalid_or_missing_baseline_is_honest_and_never_invents_compare_link(
        self,
    ) -> None:
        for metadata in (
            "",
            "Source commit: unknown (legacy build)",
            "Source commit: 9eaf6d018de9",
            "Source commit: " + "z" * 40,
            "Source commit: " + "a" * 64,
            f"Source commit: {PREVIOUS_SOURCE}\nSource commit: {PREVIOUS_SOURCE}",
            "Source commit: $(touch never-execute)",
        ):
            with (
                self.subTest(metadata=metadata),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                result, home_dir, _, _ = run_installer(
                    root, previous_tag="unpublished-build", previous_version=metadata
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(os.readlink(home_dir / "current"), TAG)
                self.assertIn("previous source baseline unavailable", result.stderr)
                self.assertIn(f"/releases/tag/{TAG}", result.stderr)
                self.assertNotIn("/compare/", result.stderr)
                self.assertNotIn("/compare/", (root / "requests.log").read_text())
                self.assertNotIn("curl:", result.stderr)
                self.assertFalse((root / "never-execute").exists())

    @unittest.skipUnless(shutil.which("jq"), "jq needed for legacy identity lookup")
    def test_invalid_remote_identity_never_emits_compare_link(self) -> None:
        for identity in (
            {},
            {"sha": None},
            {"sha": PREVIOUS_SOURCE[:12]},
            {"sha": "x" * 40},
        ):
            with (
                self.subTest(identity=identity),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, _, _, _ = run_installer(
                    Path(temp_dir),
                    previous_tag="legacy-build",
                    previous_version="codex-cli 0.154.0",
                    commit_sources={"legacy-build": identity},
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("previous source baseline unavailable", result.stderr)
                self.assertNotIn("/compare/", result.stderr)

    @unittest.skipUnless(shutil.which("jq"), "jq needed for legacy identity lookup")
    def test_unavailable_target_identity_keeps_release_notes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result, _, _, _ = run_installer(
                Path(temp_dir),
                previous_tag="old-build",
                target_version="codex-cli 0.154.0",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("target source identity unavailable", result.stderr)
            self.assertIn(f"/releases/tag/{TAG}", result.stderr)
            self.assertNotIn("/compare/", result.stderr)

    def test_without_jq_legacy_tag_comparison_requires_http_success(self) -> None:
        for failure in (False, True):
            for target_version in (
                f"codex-cli 0.163.0\nSource commit: {TARGET_SOURCE}\n",
                "codex-cli 0.154.0",
            ):
                with (
                    self.subTest(failure=failure, target_version=target_version),
                    tempfile.TemporaryDirectory() as temp_dir,
                ):
                    root = Path(temp_dir)
                    result, home_dir, _, _ = run_installer(
                        root,
                        previous_tag="old-build",
                        previous_version="codex-cli 0.154.0",
                        target_version=target_version,
                        without_jq=True,
                        comparison_failure=failure,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(os.readlink(home_dir / "current"), TAG)
                    self.assertIn(f"/releases/tag/{TAG}", result.stderr)
                    self.assertNotIn("curl:", result.stderr)
                    requests = (root / "requests.log").read_text().splitlines()
                    self.assertEqual(
                        [url for url in requests if "/compare/" in url],
                        [
                            f"https://api.github.com/repos/owner/repo/compare/old-build...{TAG}?per_page=100"
                        ],
                    )
                    if failure:
                        self.assertIn("comparison unavailable", result.stderr)
                        self.assertNotIn("/compare/", result.stderr)
                    else:
                        self.assertIn("summary unavailable without jq", result.stderr)
                        self.assertIn(f"/compare/old-build...{TAG}", result.stderr)

    def test_update_helper_preserves_custom_paths_and_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home_dir = root / "shake home '$not-a-command';"
            bin_dir = root / "bin with spaces '$still-not-a-command'"
            repo = "owner/repo '$x;touch nope'"
            result, _, _, helper_env = run_installer(
                root,
                home_dir=home_dir,
                bin_dir=bin_dir,
                repo=repo,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            helper = bin_dir / "codex-shake-update"
            update = subprocess.run(
                [str(helper)],
                capture_output=True,
                check=False,
                env={
                    **os.environ,
                    "CODEX_TEST_HELPER_ENV": str(helper_env),
                    "PATH": "/usr/bin:/bin",
                },
                text=True,
            )

            self.assertEqual(update.returncode, 0, update.stderr)
            self.assertEqual(
                helper_env.read_text(encoding="utf-8").splitlines(),
                [repo, str(home_dir), str(bin_dir)],
            )
            self.assertFalse((root / "not-a-command").exists())
            self.assertFalse((root / "still-not-a-command").exists())
            self.assertFalse((root / "nope").exists())

    def test_estimator_wrapper_requires_complete_release_payload(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            result, _, bin_dir, _ = run_installer(root, include_estimator=True)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((bin_dir / "codex-shake-estimate").is_file())


def run_installer(
    root: Path,
    *,
    tag: str | None = TAG,
    release_feed: str | None = None,
    home_dir: Path | None = None,
    bin_dir: Path | None = None,
    repo: str = "owner/repo",
    include_estimator: bool = False,
    previous_tag: str | None = None,
    comparison: dict | None = None,
    comparison_failure: bool = False,
    comparison_last_page: dict | None = None,
    comparison_last_page_failure: bool = False,
    without_jq: bool = False,
    previous_version: str = f"codex-cli 0.163.0\nSource commit: {PREVIOUS_SOURCE}\n",
    target_version: str = f"codex-cli 0.163.0\nSource commit: {TARGET_SOURCE}\n",
    commit_sources: dict | None = None,
) -> tuple[subprocess.CompletedProcess[str], Path, Path, Path]:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir()
    write_executable(
        fake_bin / "uname",
        "#!/bin/sh\n"
        'case "$1" in\n'
        "  -s) printf 'Darwin\\n' ;;\n"
        "  -m) printf 'arm64\\n' ;;\n"
        "esac\n",
    )
    archive = root / ASSET
    archive.write_bytes(b"test archive")
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = root / "SHA256SUMS"
    checksum.write_text(f"{digest}  {ASSET}\n", encoding="utf-8")
    helper_env = root / "helper-env.txt"

    write_executable(
        fake_bin / "curl",
        textwrap.dedent(
            f"""\
            #!/bin/sh
            url=""
            output=""
            previous=""
            for arg in "$@"; do
              case "$arg" in https://*) url="$arg" ;; esac
              if [ "$previous" = "-o" ]; then output="$arg"; fi
              previous="$arg"
            done
            printf '%s\\n' "$url" >> "$CODEX_TEST_REQUEST_LOG"
            case "$url" in
              https://api.github.com/*/commits/*)
                ref=${{url##*/}}
                # Missing tag fixtures model unpublished local builds.
                body=$(printf '%s' "$CODEX_TEST_COMMIT_SOURCES" | jq -er --arg ref "$ref" '.[$ref]') || {{
                  printf 'curl: (22) The requested URL returned error: 404\\n' >&2
                  exit 22
                }}
                printf '%s\\n' "$body"
                ;;
              https://api.github.com/*/compare/*)
                if [ "$CODEX_TEST_COMPARISON_FAILURE" = 1 ]; then
                  printf 'curl: (22) The requested URL returned error: 404\\n' >&2
                  exit 22
                fi
                case "$url" in
                  *'&page='*)
                    if [ "$CODEX_TEST_LAST_PAGE_FAILURE" = 1 ]; then
                      printf 'curl: (22) The requested URL returned error: 404\\n' >&2
                      exit 22
                    fi
                    printf '%s\\n' "$CODEX_TEST_LAST_PAGE" > "${{output:-/dev/stdout}}"
                    ;;
                  *) printf '%s\\n' "$CODEX_TEST_COMPARISON" > "${{output:-/dev/stdout}}" ;;
                esac
                ;;
              https://api.github.com/*/releases*)
                printf '%s\\n' "$CODEX_TEST_RELEASE_FEED"
                ;;
              https://github.com/*/releases/download/*/{ASSET}) cp "$CODEX_TEST_ARCHIVE" "$output" ;;
              https://github.com/*/releases/download/*/SHA256SUMS) cp "$CODEX_TEST_CHECKSUM" "$output" ;;
              *) exit 22 ;;
            esac
            """,
        ),
    )
    estimator_payload = (
        textwrap.dedent(
            """\
            cat > "$destination/codex-shake-estimate" <<'ESTIMATOR'
            #!/bin/sh
            exit 0
            ESTIMATOR
            : > "$destination/shake-savings-estimate.py"
            : > "$destination/pricing_lib.py"
            : > "$destination/pricing.json"
            chmod 755 "$destination/codex-shake-estimate"
            """
        )
        if include_estimator
        else ""
    )
    write_executable(
        fake_bin / "tar",
        textwrap.dedent(
            """\
            #!/bin/sh
            destination=""
            previous=""
            for arg in "$@"; do
              if [ "$previous" = "-C" ]; then destination="$arg"; fi
              previous="$arg"
            done
            mkdir -p "$destination"
            cat > "$destination/codex" <<'CODEX'
            #!/bin/sh
            if [ "$1" = "--version" ]; then
              printf '%s\\n' "$CODEX_TEST_TARGET_VERSION"
            elif [ "$1" = "update" ]; then
              printf '%s\\n' "$CODEX_SHAKE_REPO" "$CODEX_SHAKE_HOME" "$CODEX_SHAKE_BIN_DIR" > "$CODEX_TEST_HELPER_ENV"
            fi
            CODEX
            cat > "$destination/codex-code-mode-host" <<'HOST'
            #!/bin/sh
            exit 0
            HOST
            chmod 755 "$destination/codex" "$destination/codex-code-mode-host"
            """,
        )
        + estimator_payload,
    )

    use_default_home = home_dir is None
    use_default_bin = bin_dir is None
    if use_default_home:
        home_dir = root / "home" / ".local" / "share" / "codex-shake"
    if use_default_bin:
        bin_dir = root / "home" / ".local" / "bin"
    home_dir.parent.mkdir(parents=True, exist_ok=True)
    bin_dir.parent.mkdir(parents=True, exist_ok=True)

    if previous_tag is not None:
        assert home_dir is not None
        previous_dir = home_dir / previous_tag
        previous_dir.mkdir(parents=True)
        write_executable(
            previous_dir / "codex",
            '#!/bin/sh\nprintf "%s\\n" "$CODEX_TEST_PREVIOUS_VERSION"\n',
        )
        (home_dir / "current").symlink_to(previous_tag)

    # A real PATH without jq exercises the POSIX fallback, even on hosts where
    # jq is installed globally. All network/extraction tools remain fixtures.
    test_path = f"{fake_bin}:/usr/bin:/bin"
    if without_jq:
        for name in [
            "mkdir",
            "cp",
            "sha256sum",
            "cut",
            "sed",
            "mktemp",
            "rm",
            "mv",
            "ln",
            "chmod",
            "grep",
            "readlink",
            "awk",
            "sort",
            "tail",
            "tr",
            "cat",
        ]:
            tool = shutil.which(name, path="/usr/bin:/bin")
            assert tool is not None, name
            (fake_bin / name).symlink_to(tool)
        test_path = str(fake_bin)

    env = {
        key: value
        for key, value in os.environ.items()
        if key not in {"CODEX_SHAKE_HOME", "CODEX_SHAKE_BIN_DIR", "CODEX_SHAKE_TAG"}
    }
    env.update(
        {
            "CODEX_SHAKE_REPO": repo,
            "CODEX_TEST_ARCHIVE": str(archive),
            "CODEX_TEST_CHECKSUM": str(checksum),
            "CODEX_TEST_HELPER_ENV": str(helper_env),
            "CODEX_TEST_REQUEST_LOG": str(root / "requests.log"),
            "CODEX_TEST_PREVIOUS_VERSION": previous_version,
            "CODEX_TEST_TARGET_VERSION": target_version,
            "CODEX_TEST_COMMIT_SOURCES": json.dumps(commit_sources or {}),
            "CODEX_TEST_COMPARISON": json.dumps(comparison or {}),
            "CODEX_TEST_COMPARISON_FAILURE": "1" if comparison_failure else "0",
            "CODEX_TEST_LAST_PAGE": json.dumps(comparison_last_page or {}),
            "CODEX_TEST_LAST_PAGE_FAILURE": "1"
            if comparison_last_page_failure
            else "0",
            "HOME": str(root / "home"),
            "PATH": test_path,
        }
    )
    if tag is not None:
        env["CODEX_SHAKE_TAG"] = tag
    if release_feed is not None:
        env["CODEX_TEST_RELEASE_FEED"] = release_feed
    if not use_default_home:
        env["CODEX_SHAKE_HOME"] = str(home_dir)
    if not use_default_bin:
        env["CODEX_SHAKE_BIN_DIR"] = str(bin_dir)
    result = subprocess.run(
        ["/bin/sh", str(INSTALL_SCRIPT)],
        capture_output=True,
        check=False,
        env=env,
        text=True,
    )
    return result, home_dir, bin_dir, helper_env


def write_executable(path: Path, contents: str) -> None:
    path.write_text(contents, encoding="utf-8")
    path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
