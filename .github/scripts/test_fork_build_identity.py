#!/usr/bin/env python3
"""Focused allocation/ordering regressions for actual Git source identities."""

import concurrent.futures
import importlib.util
import os
import re
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

HELPER = Path(__file__).with_name("fork-build-identity.py")
spec = importlib.util.spec_from_file_location("identity", HELPER)
identity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity)


class BuildIdentityTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Build Identity Test")
        self.commit("first")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], text=True
        ).strip()

    def commit(self, title):
        # Consecutive source revisions in the same second must still order.
        subprocess.run(
            ["git", "-C", str(self.root), "commit", "--allow-empty", "-qm", title],
            check=True,
            env={**os.environ, "GIT_COMMITTER_DATE": "2026-10-21T12:00:00Z"},
        )

    def allocate(self, *args):
        output = subprocess.check_output(
            [
                "python3",
                str(HELPER),
                "allocate",
                "--repo-root",
                str(self.root),
                "--sha",
                "HEAD",
                "--version",
                "0.163.0",
                *args,
            ],
            text=True,
        )
        return dict(line.split("=", 1) for line in output.splitlines())

    def test_ci_retry_and_next_run_are_distinct_and_repeatable(self):
        first = self.allocate(
            "--kind", "ci", "--run-number", "1234", "--run-attempt", "1"
        )
        self.assertEqual(
            first,
            self.allocate("--kind", "ci", "--run-number", "1234", "--run-attempt", "1"),
        )
        retry = self.allocate(
            "--kind", "ci", "--run-number", "1234", "--run-attempt", "2"
        )
        next_run = self.allocate(
            "--kind", "ci", "--run-number", "1235", "--run-attempt", "1"
        )
        self.assertLess(first["release_tag"], retry["release_tag"])
        self.assertLess(retry["release_tag"], next_run["release_tag"])
        self.assertEqual(retry["build_revision"], "1234.2")
        self.assertEqual(len(first["release_tag"].split(".")[-1]), 52)

    def test_source_sequence_increases_even_in_same_second(self):
        first = self.allocate(
            "--kind", "ci", "--run-number", "1234", "--run-attempt", "1"
        )
        self.commit("second")
        second = self.allocate(
            "--kind", "ci", "--run-number", "1235", "--run-attempt", "1"
        )
        self.assertEqual(
            int(second["source_sequence"]), int(first["source_sequence"]) + 1
        )
        self.assertLess(first["release_tag"], second["release_tag"])

    def test_local_counter_is_serialized_and_preview_does_not_reserve(self):
        counter = self.root / "counter"
        args = ("--kind", "local", "--counter-file", str(counter))
        preview = self.allocate(*args, "--preview")
        self.assertFalse(counter.exists())
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            builds = list(pool.map(lambda _: self.allocate(*args), range(12)))
        self.assertEqual(len({b["release_tag"] for b in builds}), 12)
        self.assertEqual(
            {b["build_revision"] for b in builds}, {f"local {n}" for n in range(1, 13)}
        )
        self.assertEqual(counter.read_text(), "12\n")
        self.assertIn(preview["release_tag"], {b["release_tag"] for b in builds})
        self.assertEqual(self.allocate(*args)["build_revision"], "local 13")

    def test_local_installer_previews_shared_identity_without_installing(self):
        tools = self.root / "bin"
        tools.mkdir()
        for name, script in {
            "rustc": "#!/bin/sh\nprintf 'host: x86_64-unknown-linux-gnu\\n'\n",
            "pkg-config": "#!/bin/sh\nexit 0\n",
        }.items():
            tool = tools / name
            tool.write_text(script)
            tool.chmod(0o755)
        install_root = self.root / "installed"
        script = HELPER.parents[2] / "scripts/shake-local-install.sh"
        result = subprocess.run(
            [
                "bash",
                str(script),
                "--dry-run",
                "--repo",
                str(self.root),
                "--version",
                "0.163.0",
                "HEAD",
            ],
            text=True,
            capture_output=True,
            env={
                **os.environ,
                "PATH": str(tools) + os.pathsep + os.environ["PATH"],
                "CODEX_SHAKE_HOME": str(install_root),
                "CODEX_SHAKE_BUILD_COUNTER": str(install_root / ".build-counter"),
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CLI build     local 1", result.stdout)
        self.assertIn(
            "CODEX_FORK_SOURCE_SHA=" + self.git("rev-parse", "HEAD"), result.stdout
        )
        self.assertFalse(install_root.exists())

    def test_local_counter_default_survives_install_root_changes(self):
        # State selection is independent of the host's compiler/libcap setup.
        # Supply dry-run preflight tools without requiring installed packages.
        tools = self.root / "bin"
        tools.mkdir()
        for name, contents in {
            "rustc": "#!/bin/sh\nprintf 'host: x86_64-unknown-linux-gnu\\n'\n",
            "pkg-config": "#!/bin/sh\nexit 0\n",
        }.items():
            tool = tools / name
            tool.write_text(contents)
            tool.chmod(0o755)
        state = self.root / "state"
        counter = state / "codex-shake/local-build-counter"
        counter.parent.mkdir(parents=True)
        counter.write_text("17\n")
        script = HELPER.parents[2] / "scripts/shake-local-install.sh"
        for name in ("install-one", "install-two"):
            install_root = self.root / name
            env = {
                **os.environ,
                "PATH": str(tools) + os.pathsep + os.environ["PATH"],
                "XDG_STATE_HOME": str(state),
                "CODEX_SHAKE_HOME": str(install_root),
            }
            env.pop("CODEX_SHAKE_BUILD_COUNTER", None)
            # This check targets state selection; force the ordinary build path.
            env.pop("CODEX_SKIP_BWRAP_BUILD", None)
            result = subprocess.run(
                [
                    "bash",
                    str(script),
                    "--dry-run",
                    "--repo",
                    str(self.root),
                    "--version",
                    "0.163.0",
                    "HEAD",
                ],
                text=True,
                capture_output=True,
                env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("CLI build     local 18", result.stdout)
            self.assertFalse(install_root.exists())
        self.assertEqual(counter.read_text(), "17\n")

    def test_failed_local_build_resumes_source_cache_with_new_identity(self):
        import fcntl

        source = self.root / "codex-rs"
        source.mkdir()
        (source / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.0"\n')
        (source / "Cargo.lock").write_text(
            'version = 4\n[[package]]\nname = "codex-cli"\nversion = "0.0.0"\n'
        )
        (source / "source.rs").write_text("original source\n")
        (self.root / ".gitignore").write_text("target/\n")
        self.git("add", ".")
        self.commit("build fixture")
        sha = self.git("rev-parse", "HEAD")
        tools = self.root / "tools"
        tools.mkdir()
        for name, contents in {
            "rustc": "#!/bin/sh\nprintf 'host: aarch64-apple-darwin\\n'\n",
            "cargo": textwrap.dedent("""\
                #!/bin/sh
                printf '%s\\n' "$CODEX_FORK_RELEASE_TAG" >> "$LOCAL_TEST_BUILDS"
                mkdir -p target
                : > target/cache-sentinel
                sed 's/version = "0.0.0"/version = "0.163.0"/g' Cargo.lock > Cargo.lock.next
                mv Cargo.lock.next Cargo.lock
                exit 42
                """),
        }.items():
            tool = tools / name
            tool.write_text(contents)
            tool.chmod(0o755)
        artifact = self.root / "v8"
        artifact.write_text("verified fixture\n")
        script = HELPER.parents[2] / "scripts/shake-local-install.sh"
        env = {
            **os.environ,
            "PATH": str(tools) + os.pathsep + os.environ["PATH"],
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "CODEX_SHAKE_HOME": str(self.root / "installed"),
            "CODEX_SHAKE_BUILD_COUNTER": str(self.root / "counter"),
            "RUSTY_V8_ARCHIVE": str(artifact),
            "RUSTY_V8_SRC_BINDING_PATH": str(artifact),
            "LOCAL_TEST_BUILDS": str(self.root / "builds"),
        }
        args = [
            "bash",
            str(script),
            "--repo",
            str(self.root),
            "--version",
            "0.163.0",
            "HEAD",
        ]
        worktree = self.root / "cache/codex-shake-local" / sha[:12] / "src"
        for attempt in (1, 2):
            result = subprocess.run(args, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertIn(f"worktree      {worktree}", result.stdout)
            self.assertIn(f"CLI build     local {attempt}", result.stdout)
            self.assertTrue((worktree / "codex-rs/target/cache-sentinel").exists())
            if attempt == 2:
                self.assertIn("resuming in existing worktree", result.stderr)
        tags = (self.root / "builds").read_text().splitlines()
        self.assertEqual(len(tags), 2)
        self.assertNotEqual(tags[0], tags[1])
        self.assertFalse((self.root / "installed").exists())

        # A competing invocation cannot touch the shared cache or start Cargo.
        with (worktree.parent / ".lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            competing = subprocess.run(args, env=env, capture_output=True, text=True)
            self.assertNotEqual(competing.returncode, 0)
            self.assertIn("another build owns", competing.stderr)
            self.assertIn("--work-dir", competing.stderr)
            self.assertEqual((self.root / "builds").read_text().splitlines(), tags)
            self.assertTrue((worktree / "codex-rs/target/cache-sentinel").exists())

        # Unexpected source edits are preserved and cannot masquerade as SHA.
        (worktree / "codex-rs/source.rs").write_text("user source edit\n")
        rejected = subprocess.run(args, env=env, capture_output=True, text=True)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("preserving cached worktree", rejected.stderr)
        self.assertEqual((self.root / "builds").read_text().splitlines(), tags)
        self.assertEqual(
            (worktree / "codex-rs/source.rs").read_text(), "user source edit\n"
        )

        override = self.root / "custom-build"
        preview = subprocess.run(
            args[0:2] + ["--dry-run", "--work-dir", str(override)] + args[2:],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertIn(f"worktree      {override}/src", preview.stdout)

    def test_local_installer_reads_source_sha_from_extended_installed_tag(self):
        installed_tag = self.allocate(
            "--kind", "ci", "--run-number", "1234", "--run-attempt", "2"
        )["release_tag"]
        install_root = self.root / "installed"
        resources = install_root / installed_tag / "codex-resources"
        resources.mkdir(parents=True)
        bwrap = resources / "bwrap"
        bwrap.write_text("#!/bin/sh\nexit 0\n")
        bwrap.chmod(0o755)
        (install_root / "current").symlink_to(installed_tag)
        script = HELPER.parents[2] / "scripts/shake-local-install.sh"
        result = subprocess.run(
            [
                "bash",
                str(script),
                "--dry-run",
                "--repo",
                str(self.root),
                "--version",
                "0.163.0",
                "HEAD",
            ],
            text=True,
            capture_output=True,
            env={
                **os.environ,
                "CODEX_SHAKE_HOME": str(install_root),
                "CODEX_SHAKE_BUILD_COUNTER": str(install_root / ".build-counter"),
                "CODEX_SKIP_BWRAP_BUILD": "1",
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "sources unchanged since " + self.git("rev-parse", "--short=12", "HEAD"),
            result.stdout,
        )
        self.assertFalse((install_root / ".build-counter").exists())

    def test_version_verifier_rejects_wrong_build_or_source(self):
        build = self.allocate(
            "--kind", "ci", "--run-number", "1234", "--run-attempt", "1"
        )
        source = self.git("rev-parse", "HEAD")
        source_dir = self.root / "tui/src"
        source_dir.mkdir(parents=True)
        (source_dir / "fork_update.rs").write_text(
            'pub(crate) const SHAKE_FEATURE_VERSION: &str = "0.4.0";\n'
        )
        report = self.root / "version.txt"
        expected = (
            "codex-cli 0.163.0\nShake feature version: 0.4.0\nCLI build: 1234.1\n"
            f"Source commit: {source}\nRelease tag: {build['release_tag']}\n"
        )
        binary = self.root / "codex"
        binary.write_text(
            '#!/bin/sh\nif [ "$1" = "-V" ]; then printf "codex-cli 0.163.0\\n"; else cat "$VERSION_REPORT"; fi\n'
        )
        binary.chmod(0o755)
        args = [
            "bash",
            str(HELPER.with_name("fork-release-build.sh")),
            "verify-version",
            "0.163.0",
            build["release_tag"],
            source,
        ]
        env = {
            **os.environ,
            "CODEX_VERSION_BINARY": str(binary),
            "VERSION_REPORT": str(report),
        }
        report.write_text(expected)
        good = subprocess.run(
            args, cwd=self.root, env=env, capture_output=True, text=True
        )
        self.assertEqual(good.returncode, 0, good.stderr)
        for bad_output in (
            expected.replace("1234.1", "1234.2"),
            expected.replace(source, "0" * 40),
        ):
            report.write_text(bad_output)
            bad = subprocess.run(
                args, cwd=self.root, env=env, capture_output=True, text=True
            )
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn("expected source, Shake, and release versions", bad.stderr)

    def test_workflow_rejects_mixed_prepared_attempts_before_build_and_publish(self):
        workflow = HELPER.parents[1] / "workflows/fork-release.yml"
        guards = re.findall(
            r"- name: Verify prepared workflow attempt\n.*?run: \|\n((?:          [^\n]*\n)+)",
            workflow.read_text(),
            re.DOTALL,
        )
        self.assertEqual(len(guards), 2)
        for guard in guards:
            for current, expected_status in (("1", 0), ("2", 1)):
                result = subprocess.run(
                    ["bash", "-c", textwrap.dedent(guard)],
                    text=True,
                    capture_output=True,
                    env={
                        **os.environ,
                        "PREPARED_ATTEMPT": "1",
                        "GITHUB_RUN_ATTEMPT": current,
                    },
                )
                self.assertEqual(result.returncode, expected_status, result.stderr)
                if expected_status:
                    self.assertIn("Use Re-run all jobs", result.stderr)

    def test_legacy_descriptions_and_invalid_allocation(self):
        self.assertEqual(
            identity.describe(
                "local-features-v0.163.0-main-r20261021000000.000000000000007baaaaaaaaaaaa"
            ),
            "source 123 (legacy)",
        )
        result = subprocess.run(
            [
                "python3",
                str(HELPER),
                "allocate",
                "--repo-root",
                str(self.root),
                "--sha",
                "HEAD",
                "--version",
                "0.163.0",
                "--kind",
                "ci",
                "--run-number",
                "1",
                "--run-attempt",
                "0",
            ],
            capture_output=True,
        )
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
