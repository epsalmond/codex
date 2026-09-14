#!/usr/bin/env python3

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


INSTALL_SCRIPT = Path(__file__).parents[2] / "install.sh"
TAG = "local-features-v0.154.0-r202609131200.abcdef1"
ASSET = "codex-aarch64-apple-darwin.tar.gz"


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
            case "$url" in
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
              printf 'codex-shake test\\n'
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

    env = {
        key: value
        for key, value in os.environ.items()
        if key not in {"CODEX_SHAKE_HOME", "CODEX_SHAKE_BIN_DIR"}
    }
    env.update(
        {
            "CODEX_SHAKE_REPO": repo,
            "CODEX_TEST_ARCHIVE": str(archive),
            "CODEX_TEST_CHECKSUM": str(checksum),
            "CODEX_TEST_HELPER_ENV": str(helper_env),
            "HOME": str(root / "home"),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
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
