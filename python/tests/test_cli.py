"""`khive-cloud` CLI, driven in-process against the fake khive-cloud servers."""

from __future__ import annotations

import json
import subprocess
import sys

import pytest

httpx = pytest.importorskip("httpx")

from khive import cli


def test_cli_health(rest_server, capsys):
    rc = cli.main(["--url", rest_server.url, "health"])
    assert rc == 0
    assert json.loads(capsys.readouterr().out) == {"status": "ok"}


def test_cli_whoami(rest_server, api_key, capsys):
    rc = cli.main(["--url", rest_server.url, "--api-key", api_key, "whoami"])
    assert rc == 0
    out = json.loads(capsys.readouterr().out)
    assert isinstance(out, dict), "whoami's exact shape is server-controlled; assert only shape"


def test_cli_exec_prints_envelope(rest_server, api_key, capsys):
    rc = cli.main(
        ["--url", rest_server.url, "--api-key", api_key, "exec", '[{"tool": "stats", "args": {}}]']
    )
    assert rc == 0
    out = json.loads(capsys.readouterr().out)
    assert out["summary"]["succeeded"] == 1


def test_cli_exec_nonzero_exit_on_op_failure(rest_server, api_key, capsys):
    rc = cli.main(
        ["--url", rest_server.url, "--api-key", api_key, "exec", '[{"tool": "nope", "args": {}}]']
    )
    assert rc == 1
    out = json.loads(capsys.readouterr().out)
    assert out["summary"]["failed"] == 1


def test_cli_exec_nonzero_exit_on_aborted(rest_server, api_key, capsys):
    rc = cli.main(
        [
            "--url",
            rest_server.url,
            "--api-key",
            api_key,
            "exec",
            '[{"tool": "stats", "args": {}}, {"tool": "later", "args": {}}]',
        ]
    )
    assert rc == 1
    out = json.loads(capsys.readouterr().out)
    assert out["summary"]["aborted"] == 1


def test_cli_missing_url_exits_2(monkeypatch, capsys):
    monkeypatch.delenv("KHIVE_CLOUD_URL", raising=False)
    with pytest.raises(SystemExit) as exc_info:
        cli.main(["health"])
    assert exc_info.value.code == 2
    assert "KHIVE_CLOUD_URL" in capsys.readouterr().err


def test_cli_missing_api_key_exits_2(rest_server, monkeypatch, capsys):
    monkeypatch.delenv("KHIVE_CLOUD_API_KEY", raising=False)
    with pytest.raises(SystemExit) as exc_info:
        cli.main(["--url", rest_server.url, "whoami"])
    assert exc_info.value.code == 2
    assert "KHIVE_CLOUD_API_KEY" in capsys.readouterr().err


def test_cli_env_vars(rest_server, api_key, monkeypatch, capsys):
    monkeypatch.setenv("KHIVE_CLOUD_URL", rest_server.url)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", api_key)
    rc = cli.main(["whoami"])
    assert rc == 0


def test_cli_http_error_message_never_echoes_key(rest_server, capsys):
    rc = cli.main(["--url", rest_server.url, "--api-key", "wrong-key", "whoami"])
    assert rc == 1
    err = capsys.readouterr().err
    assert "wrong-key" not in err
    assert "AuthError" in err
    assert "HTTP 401" in err


def test_cli_tools(mcp_server, api_key, capsys):
    pytest.importorskip("mcp")
    rc = cli.main(["--url", mcp_server.url, "--api-key", api_key, "tools"])
    assert rc == 0
    assert capsys.readouterr().out.strip().splitlines() == ["request"]


def test_installed_console_script_subprocess(rest_server, api_key):
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "khive.cli",
            "--url",
            rest_server.url,
            "--api-key",
            api_key,
            "exec",
            '[{"tool": "stats", "args": {}}]',
        ],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["summary"]["succeeded"] == 1
