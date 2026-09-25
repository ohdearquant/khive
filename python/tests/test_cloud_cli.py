"""Cloud configuration and command-line contract against the offline REST fixture."""

import importlib
import json
import tomllib
from pathlib import Path

import pytest

pytest.importorskip("httpx")

import khive
from khive import AuthError, HttpTransport


def cli(args):
    return importlib.import_module("khive.cloud_cli").main(args)


@pytest.fixture(autouse=True)
def clear_cloud_environment(monkeypatch):
    monkeypatch.delenv("KHIVE_CLOUD_URL", raising=False)
    monkeypatch.delenv("KHIVE_CLOUD_API_KEY", raising=False)


def test_cloud_explicit_arguments_override_environment(rest_server, api_key, monkeypatch):
    monkeypatch.setenv("KHIVE_CLOUD_URL", "http://127.0.0.1:1")
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "incorrect-environment-value")
    db = khive.cloud(rest_server.url, api_key)
    try:
        assert db.whoami() == {"namespace": "local"}
    finally:
        db.session.transport.close()


def test_cloud_uses_environment(rest_server, api_key, monkeypatch):
    monkeypatch.setenv("KHIVE_CLOUD_URL", rest_server.url)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", api_key)
    db = khive.cloud()
    try:
        assert db.whoami() == {"namespace": "local"}
    finally:
        db.session.transport.close()


@pytest.mark.parametrize(
    "argument,variable", [("base_url", "KHIVE_CLOUD_URL"), ("api_key", "KHIVE_CLOUD_API_KEY")]
)
def test_cloud_missing_configuration_names_argument_and_environment(argument, variable):
    kwargs = {"base_url": "https://example.invalid", "api_key": "example-credential"}
    del kwargs[argument]
    with pytest.raises(ValueError) as exc:
        khive.cloud(**kwargs)
    assert argument in str(exc.value) and variable in str(exc.value)


def test_cli_url_flag_overrides_environment_and_key_comes_from_environment(
    rest_server, api_key, monkeypatch, capsys
):
    monkeypatch.setenv("KHIVE_CLOUD_URL", "http://127.0.0.1:1")
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", api_key)
    assert cli(["whoami", "--url", rest_server.url]) == 0
    out = capsys.readouterr()
    assert json.loads(out.out)["results"][0]["result"] == {"namespace": "local"}
    assert out.err == ""


def test_cli_exec_verbatim_with_environment(rest_server, api_key, monkeypatch, capsys):
    import conftest

    monkeypatch.setenv("KHIVE_CLOUD_URL", rest_server.url)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", api_key)
    received = []
    dispatch = conftest._dispatch_ops

    def record(ops):
        received.append(ops)
        return dispatch(ops)

    monkeypatch.setattr(conftest, "_dispatch_ops", record)
    ops = "  whoami() | stats()  "
    assert cli(["exec", ops]) == 0
    out = capsys.readouterr()
    assert received == [ops]
    assert [r["tool"] for r in json.loads(out.out)["results"]] == ["whoami", "stats"]


def test_cli_missing_url_has_no_default(capsys, monkeypatch):
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "unused-credential")
    assert cli(["whoami"]) == 2
    out = capsys.readouterr()
    assert "--url" in out.err and "KHIVE_CLOUD_URL" in out.err
    assert not out.out


def test_cli_missing_key(capsys):
    assert cli(["whoami", "--url", "https://example.invalid"]) == 2
    out = capsys.readouterr()
    assert "KHIVE_CLOUD_API_KEY" in out.err and "--api-key" not in out.err


def test_cli_401_exit_and_server_message(rest_server, capsys, monkeypatch):
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "incorrect-credential")
    assert cli(["whoami", "--url", rest_server.url]) == 3
    out = capsys.readouterr()
    assert "AuthError" in out.err and "unauthorized" in out.err
    assert len(out.err.splitlines()) == 1 and not out.out


def test_cli_and_transport_never_print_credential(rest_server, monkeypatch, capsys):
    import conftest

    secret = "distinctive-client-credential-89173"
    monkeypatch.setattr(conftest, "API_KEY", secret)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", secret)
    assert cli(["whoami", "--url", rest_server.url]) == 0
    output = capsys.readouterr()
    assert secret not in output.out + output.err, "CLI must not print credential"
    monkeypatch.setattr(conftest, "API_KEY", "different-server-credential")
    assert cli(["whoami", "--url", rest_server.url]) == 3
    output = capsys.readouterr()
    assert secret not in output.out + output.err, "CLI must not print credential"
    with HttpTransport(rest_server.url, secret) as transport:
        assert secret not in str(transport) + repr(transport)
        with pytest.raises(AuthError) as exc:
            transport.send_dsl("whoami()", timeout=5)
        assert secret not in str(exc.value) + repr(exc.value)


@pytest.mark.parametrize(
    "ops,code,kind", [("rate_limited()", 4, "RateLimited"), ("boom()", 5, "ServerError")]
)
def test_cli_http_exit_codes(rest_server, api_key, capsys, ops, code, kind, monkeypatch):
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", api_key)
    assert cli(["--url", rest_server.url, "exec", ops]) == code
    assert kind in capsys.readouterr().err


def test_cli_insecure_forwarded_and_transport_closed(monkeypatch, capsys):
    module = importlib.import_module("khive.cloud_cli")
    seen = []

    class RecordingTransport:
        def __init__(self, url, key, *, allow_insecure):
            seen.append((url, key, allow_insecure))

        def __enter__(self):
            return self

        def __exit__(self, *args):
            seen.append("closed")

        def send_dsl(self, ops, *, timeout):
            return {"ok": True, "result": {"results": []}}

    monkeypatch.setattr(module, "HttpTransport", RecordingTransport)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "value")
    assert cli(["whoami", "--url", "http://example.invalid", "--allow-insecure"]) == 0
    assert seen == [("http://example.invalid", "value", True), "closed"]
    assert json.loads(capsys.readouterr().out) == {"results": []}


def test_cli_redaction_withholds_response_key_matching_credential(monkeypatch, capsys):
    module = importlib.import_module("khive.cloud_cli")
    monkeypatch.setenv("KHIVE_CLOUD_URL", "https://example.invalid")
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "results")

    class ResultsTransport:
        def __init__(self, *args, **kwargs):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

        def send_dsl(self, ops, *, timeout):
            return {
                "ok": True,
                "result": {"results": [{"tool": "whoami", "result": {"results": "results"}}]},
            }

    monkeypatch.setattr(module, "HttpTransport", ResultsTransport)
    assert cli(["whoami"]) == 6
    output = capsys.readouterr()
    assert output.out == ""
    assert output.err == ("Response carried a credential in a field name; result withheld.\n")
    assert "results" not in output.err


def test_cli_preserves_safe_keys_and_redacts_credential_values(monkeypatch, capsys):
    module = importlib.import_module("khive.cloud_cli")
    secret = "sk-test-credential-0123456789"
    monkeypatch.setenv("KHIVE_CLOUD_URL", "https://example.invalid")
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", secret)

    class ValuesTransport:
        def __init__(self, *args, **kwargs):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

        def send_dsl(self, ops, *, timeout):
            return {
                "ok": True,
                "result": {"results": [{"credential": f"received {secret}"}]},
            }

    monkeypatch.setattr(module, "HttpTransport", ValuesTransport)
    assert cli(["whoami"]) == 0
    output = capsys.readouterr()
    assert json.loads(output.out) == {"results": [{"credential": "received [REDACTED]"}]}
    assert secret not in output.out + output.err


def test_cli_entry_point():
    project = tomllib.loads((Path(__file__).parents[1] / "pyproject.toml").read_text())
    assert project["project"]["scripts"]["khive-cloud"] == "khive.cloud_cli:main"


def test_cloud_resolves_each_argument_independently(rest_server, api_key, monkeypatch):
    monkeypatch.setenv("KHIVE_CLOUD_URL", rest_server.url)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "unused-environment-credential")
    db = khive.cloud(api_key=api_key)
    try:
        assert db.whoami() == {"namespace": "local"}
    finally:
        db.session.transport.close()
    with pytest.raises(ValueError, match="base_url"):
        khive.cloud("", api_key)


@pytest.mark.parametrize("args", [[], ["exec"], ["whoami", "extra"], ["unknown"]])
def test_cli_usage_returns_two(args, capsys):
    assert cli(args) == 2
    output = capsys.readouterr()
    assert output.err.startswith("ValueError:") and len(output.err.splitlines()) == 1


def test_cli_redacts_reflected_error_and_usage_value(rest_server, monkeypatch, capsys):
    import conftest

    secret = "reflected-credential-31742"
    monkeypatch.setattr(conftest, "API_KEY", secret)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", secret)
    monkeypatch.setattr(
        conftest, "_dispatch_ops", lambda ops: (401, {"error": f"refused\n{secret}"})
    )
    assert cli(["whoami", "--url", rest_server.url]) == 3
    output = capsys.readouterr()
    assert secret not in output.out + output.err
    assert "refused" in output.err and "[REDACTED]" in output.err
    assert len(output.err.splitlines()) == 1
    assert cli(["whoami", "--api-key", secret, "--unknown", secret]) == 2
    assert secret not in capsys.readouterr().err


def test_cli_connection_failure_exit_five(monkeypatch, capsys):
    module = importlib.import_module("khive.cloud_cli")

    def unavailable(*args, **kwargs):
        raise khive.TransportError("connection unavailable")

    monkeypatch.setattr(module, "HttpTransport", unavailable)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", "example-value")
    assert cli(["whoami", "--url", "https://example.invalid"]) == 5
    assert "TransportError: connection unavailable" in capsys.readouterr().err


def test_cli_help_returns_zero(capsys):
    assert cli(["--help"]) == 0
    output = capsys.readouterr().out
    assert "khive-cloud" in output and "--api-key" not in output


@pytest.mark.parametrize("secret", ['quoted"value', "back\\slash", 'both"and\\slash'])
@pytest.mark.parametrize("error", [False, True])
def test_cli_redacts_nested_results_and_escaped_errors(secret, error, monkeypatch, capsys):
    module = importlib.import_module("khive.cloud_cli")
    monkeypatch.setenv("KHIVE_CLOUD_URL", "https://example.invalid")
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", secret)

    class EchoTransport:
        def __init__(self, *args, **kwargs):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

        def send_dsl(self, ops, *, timeout):
            if error:
                raise AuthError(
                    401,
                    f"refused {secret} and {json.dumps(secret)}",
                    None,
                    "https://example.invalid",
                )
            return {
                "result": {
                    "nested": {secret: [secret, {"echo": f"prefix {secret}"}]},
                    "untouched": [3, True, None],
                }
            }

    monkeypatch.setattr(module, "HttpTransport", EchoTransport)
    assert cli(["whoami"]) == (3 if error else 6)
    output = capsys.readouterr()
    text = output.out + output.err
    assert secret not in text
    assert json.dumps(secret)[1:-1] not in text
    if error:
        assert "[REDACTED]" in text
    else:
        assert output.out == ""
        assert output.err == ("Response carried a credential in a field name; result withheld.\n")


@pytest.mark.parametrize("args", [["--api-key", "abc"], ["--api-key=abc"]])
def test_cli_rejects_credential_argv_without_echo(args, rest_server, api_key, monkeypatch, capsys):
    monkeypatch.setenv("KHIVE_CLOUD_URL", rest_server.url)
    monkeypatch.setenv("KHIVE_CLOUD_API_KEY", api_key)
    assert cli(["whoami", *args]) == 2
    output = capsys.readouterr()
    assert "abc" not in output.err and not output.out
