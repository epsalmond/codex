#!/usr/bin/env python3
"""Smoke test an extracted CLI package without credentials or dependencies."""

import argparse
import asyncio
import json
import os
import subprocess
import tempfile
from pathlib import Path


async def smoke_app_server(binary: Path, codex_home: str) -> None:
    process = await asyncio.create_subprocess_exec(
        str(binary),
        "app-server",
        "--listen",
        "stdio://",
        cwd=codex_home,
        env={**os.environ, "CODEX_HOME": codex_home},
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
    )
    assert process.stdin is not None and process.stdout is not None

    async def initialize() -> None:
        request = {
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "package_smoke", "version": "0.0.0"},
            },
        }
        process.stdin.write(json.dumps(request).encode("utf-8") + b"\n")
        await process.stdin.drain()
        for _ in range(8):
            message = json.loads(await process.stdout.readline())
            if message.get("id") == 1:
                if "error" in message or not message.get("result", {}).get("userAgent"):
                    raise RuntimeError(f"App-server initialize failed: {message}")
                break
        else:
            raise RuntimeError("App-server did not return an initialize response")
        process.stdin.write(b'{"method":"initialized"}\n')
        await process.stdin.drain()
        process.stdin.close()
        if await process.wait() != 0:
            raise RuntimeError("App-server exited unsuccessfully")

    try:
        await asyncio.wait_for(initialize(), timeout=30)
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()
    print("App-server initialize passed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-dir", type=Path, required=True)
    package_dir = parser.parse_args().package_dir.resolve()
    metadata = json.loads((package_dir / "codex-package.json").read_text())
    suffix = ".exe" if "windows" in metadata["target"] else ""
    for relative_path, arguments, expected in [
        (metadata["entrypoint"], ["--version"], "codex"),
        (metadata["entrypoint"], ["--help"], "Usage:"),
        (f"bin/codex-code-mode-host{suffix}", ["--help"], "Usage:"),
        (f"codex-path/rg{suffix}", ["--version"], "ripgrep"),
    ]:
        completed = subprocess.run(
            [str(package_dir / relative_path), *arguments],
            capture_output=True,
            text=True,
            check=True,
            timeout=30,
        )
        if expected not in completed.stdout:
            raise RuntimeError(
                f"Unexpected output from {relative_path}: {completed.stdout}"
            )
        print(f"{relative_path} {' '.join(arguments)} passed")
    with tempfile.TemporaryDirectory(prefix="codex-package-smoke-") as codex_home:
        asyncio.run(smoke_app_server(package_dir / metadata["entrypoint"], codex_home))


if __name__ == "__main__":
    main()
