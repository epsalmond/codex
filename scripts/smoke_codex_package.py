#!/usr/bin/env python3
"""Smoke test an extracted CLI package without credentials or dependencies."""

import argparse
import asyncio
import json
import struct
import subprocess
from pathlib import Path


async def smoke_code_mode_host(binary: Path) -> None:
    process = await asyncio.create_subprocess_exec(
        str(binary), stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE
    )
    assert process.stdin is not None and process.stdout is not None

    async def send(message: dict) -> None:
        payload = json.dumps(message).encode("utf-8")
        process.stdin.write(struct.pack("<I", len(payload)) + payload)
        await process.stdin.drain()

    async def receive() -> dict:
        length = struct.unpack("<I", await process.stdout.readexactly(4))[0]
        if length > 1024 * 1024:
            raise RuntimeError(f"Unexpected smoke response size: {length}")
        return json.loads(await process.stdout.readexactly(length))

    async def response(message_type: str, request_id: int) -> dict:
        for _ in range(8):
            message = await receive()
            if message["type"] == message_type and message["id"] == request_id:
                result = message["result"]
                if result["status"] != "ok":
                    raise RuntimeError(f"Code-mode request failed: {message}")
                return result["value"]
        raise RuntimeError("Code-mode host did not return the expected response")

    async def request(request_id: int, method: str, **fields) -> None:
        await send(
            {
                "type": "operation/request",
                "id": request_id,
                "request": {"method": method, "sessionId": "smoke", **fields},
            }
        )

    async def execute() -> None:
        await send(
            {
                "type": "connection/hello",
                "supportedVersions": [1],
                "requiredCapabilities": [],
                "optionalCapabilities": [],
            }
        )
        hello = await receive()
        if hello.get("type") != "connection/ready" or hello.get("selectedVersion") != 1:
            raise RuntimeError(f"Code-mode handshake failed: {hello}")
        await request(1, "session/open")
        ready = await response("operation/response", 1)
        if ready != {"type": "session/ready", "sessionId": "smoke"}:
            raise RuntimeError(f"Code-mode session did not open: {ready}")
        await request(
            2,
            "session/execute",
            request={
                "tool_call_id": "package-smoke",
                "enabled_tools": [],
                "source": 'text("v8-smoke:" + (6 * 7));',
                "yield_time_ms": 10_000,
                "max_output_tokens": 100,
            },
        )
        await response("operation/response", 2)
        outcome = await response("execute/initialResponse", 2)
        result = outcome.get("Result", {})
        if result.get("error_text") is not None or result.get("content_items") != [
            {"type": "input_text", "text": "v8-smoke:42"}
        ]:
            raise RuntimeError(
                f"V8 execution did not produce the expected output: {outcome}"
            )
        await request(3, "session/shutdown")
        await response("operation/response", 3)
        process.stdin.close()
        if await process.wait() != 0:
            raise RuntimeError("Code-mode host exited unsuccessfully")

    try:
        await asyncio.wait_for(execute(), timeout=30)
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()
    print("V8 JavaScript execution passed")


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
    asyncio.run(smoke_code_mode_host(package_dir / f"bin/codex-code-mode-host{suffix}"))


if __name__ == "__main__":
    main()
