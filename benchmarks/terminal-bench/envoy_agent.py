"""Minimal Harbor / Terminal-Bench 2.0 external agent adapter for Envoy.

Usage (once `harbor` is installed and `envoy` is built for Linux):
    PYTHONPATH=benchmarks/terminal-bench harbor run \\
      --dataset terminal-bench@2.0 \\
      --agent-import-path envoy_agent:EnvoyHarborAgent \\
      -m openai/gpt-5.4

Or run Envoy directly inside any benchmark container:
    echo "$INSTRUCTION" | envoy \\
      --data-dir /tmp/envoy-data \\
      --cwd /app \\
      --model openai/gpt-5.4 \\
      --dangerously-skip-permissions \\
      --sandbox off \\
      --max-steps 100 \\
      --output-format json \\
      -p -
"""

from __future__ import annotations

import json
import os
import shlex
from pathlib import Path
from typing import Any


try:
    from harbor.agents.base import BaseAgent  # type: ignore[import-not-found]
except ImportError:  # Allow importing outside a Harbor virtualenv (e.g. unit checks).
    BaseAgent = object  # type: ignore[assignment,misc]


class EnvoyHarborAgent(BaseAgent):  # type: ignore[misc]
    """Adapter following Harbor's `BaseAgent` lifecycle (`setup` + `run`)."""

    SUPPORTS_ATIF = False

    def __init__(
        self,
        logs_dir: Path,
        model_name: str | None = None,
        binary_path: str | None = None,
        max_steps: int = 100,
        sandbox: str = "off",
        cwd: str | None = None,
        **kwargs: Any,
    ) -> None:
        if BaseAgent is not object:
            super().__init__(logs_dir=logs_dir, model_name=model_name, **kwargs)
        self.logs_dir = Path(logs_dir)
        self.model_name = model_name or os.environ.get("ENVOY_MODEL")
        repo_root = Path(__file__).resolve().parents[2]
        musl_bin = repo_root / "target" / "x86_64-unknown-linux-musl" / "release" / "envoy"
        host_bin = repo_root / "target" / "release" / "envoy"
        default_bin = musl_bin if musl_bin.is_file() else host_bin
        self.binary_path = binary_path or os.environ.get("ENVOY_BINARY", str(default_bin))
        self.max_steps = int(max_steps)
        self.sandbox = sandbox
        self.cwd = cwd
        self.extra_env: dict[str, str] = dict(kwargs.get("extra_env") or {})
        self.extra_kwargs = kwargs

    @staticmethod
    def name() -> str:
        return "envoy"

    def version(self) -> str:
        return "0.3.0"

    async def setup(self, environment: Any) -> None:
        bin_path = Path(self.binary_path)
        if not bin_path.is_file():
            raise FileNotFoundError(
                f"Envoy binary not found at {bin_path}. Build a portable static musl binary first with "
                "`podman run --rm -v \"$PWD:/src:Z\" -w /src docker.io/library/rust:alpine "
                "sh -c 'apk add --no-cache musl-dev && cargo build --release --target x86_64-unknown-linux-musl -p envoy-cli'` "
                "(or set ENVOY_BINARY)."
            )
        await environment.upload_file(
            source_path=bin_path,
            target_path="/usr/local/bin/envoy",
        )
        await environment.exec(command="chmod +x /usr/local/bin/envoy")

    def _forwarded_env(self) -> dict[str, str]:
        keys = (
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_BASE_URL",
            "GEMINI_API_KEY",
            "GEMINI_BASE_URL",
            "OPENROUTER_API_KEY",
            "OPENROUTER_BASE_URL",
            "ENVOY_API_KEY",
            "ENVOY_BASE_URL",
            "ENVOY_PROTOCOL",
            "ENVOY_MODEL",
        )
        env = {k: os.environ[k] for k in keys if os.environ.get(k)}
        env.update(self.extra_env)
        if self.model_name:
            env["ENVOY_MODEL"] = self.model_name
        return env

    def build_command(self, instruction: str, cwd: str | None = None) -> str:
        args = [
            "/usr/local/bin/envoy",
            "--data-dir",
            "/tmp/envoy-data",
        ]
        effective_cwd = cwd if cwd is not None else self.cwd
        if effective_cwd:
            args.extend(["--cwd", effective_cwd])
        args.extend(
            [
                "--dangerously-skip-permissions",
                "--sandbox",
                self.sandbox,
                "--max-steps",
                str(self.max_steps),
                "--output-format",
                "json",
            ]
        )
        if self.model_name:
            args.extend(["--model", self.model_name])
        args.extend(["-p", "-"])
        return " ".join(shlex.quote(a) for a in args)

    @staticmethod
    def _parse_report(stdout: str) -> dict[str, Any]:
        raw = stdout.strip()
        if not raw:
            return {}
        decoder = json.JSONDecoder()
        best: dict[str, Any] = {}
        for index, char in enumerate(raw):
            if char != "{":
                continue
            try:
                parsed, _ = decoder.raw_decode(raw[index:])
            except json.JSONDecodeError:
                continue
            if isinstance(parsed, dict):
                best = parsed
        return best

    async def run(
        self,
        instruction: str,
        environment: Any,
        context: Any | None = None,
    ) -> dict[str, Any]:
        prompt_path = "/tmp/envoy-prompt.txt"
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        local_prompt = self.logs_dir / "envoy-prompt.txt"
        local_prompt.write_text(instruction, encoding="utf-8")
        await environment.upload_file(source_path=local_prompt, target_path=prompt_path)
        cmd = self.build_command(instruction) + " < " + shlex.quote(prompt_path)
        res = await environment.exec(
            command=cmd,
            env=self._forwarded_env(),
        )
        stdout = getattr(res, "stdout", "") or ""
        stderr = getattr(res, "stderr", "") or ""
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / "envoy.stdout.log").write_text(stdout, encoding="utf-8")
        (self.logs_dir / "envoy.stderr.log").write_text(stderr, encoding="utf-8")

        report = self._parse_report(stdout)

        if context is not None and report:
            context.n_input_tokens = int(report.get("input_tokens", 0))
            context.n_output_tokens = int(report.get("output_tokens", 0))
            context.metadata = {
                "session_id": report.get("session_id"),
                "tool_calls": report.get("tool_calls", 0),
                "duration_ms": report.get("duration_ms", 0),
            }
        return report
