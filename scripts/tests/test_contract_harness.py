#!/usr/bin/env python3
"""Offline contract-harness tests. Only owned Python fake executables are started."""

from __future__ import annotations

import importlib.util
from contextlib import contextmanager
import io
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / "tests"), str(ROOT / "tests/khive-contract")]
from khive_contract.client import KhiveMcpSession
import contract_test as legacy
from contract_harness import OwnedContractStore

REAL_POPEN = subprocess.Popen


class OwnedChildren:
    """Serialize process admission with terminal watchdog expiration."""

    def __init__(self):
        self.lock = threading.Lock()
        self.expired = False
        self.children = []
        self.groups = set()
        self.result = None

    def spawn(self, *args, **kwargs):
        with self.lock:
            if self.expired:
                raise TimeoutError("contract fake watchdog expired; child admission is closed")
            child = REAL_POPEN(*args, **kwargs)
            self.children.append(child)
            if kwargs.get("start_new_session"):
                self.groups.add(child.pid)
            return child

    def bind_result(self, result):
        with self.lock:
            self.result = result
            if self.expired:
                result.stop()

    def expire(self):
        with self.lock:
            self.expired = True
            children = tuple(self.children)
            groups = set(self.groups)
            result = self.result
        if result is not None:
            result.stop()
        # Admission is already closed, so this snapshot cannot miss a later child.
        for child in children:
            if child.poll() is None:
                if child.pid in groups:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                else:
                    child.kill()
            child.wait(timeout=2)


WORKER_CHILDREN = OwnedChildren()


class BoundedTestRunner(unittest.TextTestRunner):
    def _makeResult(self):
        result = super()._makeResult()
        WORKER_CHILDREN.bind_result(result)
        return result


def run_worker(argv, *, timeout=30, grace=2):
    """Bound one dedicated worker session, including its owned descendants."""
    proc = subprocess.Popen(argv, start_new_session=True)
    try:
        try:
            return proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return 124
    finally:
        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                pass
        # Also remove descendants left by an unexpectedly exited worker.
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait(timeout=grace)


def load_fixtures():
    spec = importlib.util.spec_from_file_location("contract_conftest", ROOT / "tests/khive-contract/conftest.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HarnessTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="contract-fake-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.log = self.root / "calls.jsonl"
        self.fake = self.root / "fake-kkernel"
        self.fake.write_text(f"#!{sys.executable}\n" + FAKE)
        self.fake.chmod(0o700)
        self.children = []

        def capture(*args, **kwargs):
            child = WORKER_CHILDREN.spawn(*args, **kwargs)
            self.children.append(child)
            return child

        patch = mock.patch("subprocess.Popen", capture)
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(self.cleanup_children)

    def cleanup_children(self):
        for child in self.children:
            if child.returncode is None:
                child.kill()
                child.wait(timeout=2)
            for pipe in (child.stdin, child.stdout, child.stderr):
                if pipe is not None and not pipe.closed:
                    pipe.close()

    def assert_reaped(self):
        self.assertTrue(self.children)
        for child in self.children:
            self.assertIsNotNone(child.returncode, "owner must call wait before dropping its handle")
            with self.assertRaises(ChildProcessError):
                os.waitpid(child.pid, os.WNOHANG)

    def assert_completion_envelope(self, elapsed, *, phase, exchange=0.0, reap=0.0,
                                   worker=0.0, scheduling_floor=0.0):
        # Cleanup can wait once before killing and once to reap. One additional
        # largest-budget interval allows scheduling; a caller may set a fixed
        # floor when its budget is shorter than hosted-runner scheduling delays.
        # The watchdog bounds coverage runs while ownership checks remain active.
        envelope = exchange + worker + 2 * reap + max(exchange, reap, worker,
                                                       scheduling_floor)
        if "LLVM_PROFILE_FILE" not in os.environ:
            self.assertLess(elapsed, envelope,
                            f"{phase} exceeded completion envelope {envelope:g}s")

    @contextmanager
    def adapter(self, kind, store, mode="normal", timeout=0.3, reap_timeout=None):
        env = dict(os.environ, FAKE_LOG=str(self.log), FAKE_MODE=mode)
        if kind == "pytest":
            with KhiveMcpSession(binary=self.fake, store=store, env=env, timeout=timeout,
                                 reap_timeout=reap_timeout) as session:
                yield session.tools_list
        else:
            with mock.patch.object(legacy, "BINARY", str(self.fake)):
                proc = legacy._start_server(store, env=env, timeout=timeout,
                                            reap_timeout=reap_timeout)
                try:
                    def request():
                        legacy._send(proc, "tools/list", {})
                        return legacy._recv(proc)["result"]["tools"]
                    yield request
                finally:
                    legacy._stop_server(proc)

    def calls(self):
        return [row for line in self.log.read_text().splitlines()
                if "argv" in (row := json.loads(line))]

    def assert_protocol_stage(self, mode):
        events = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertTrue(any(row.get("stage") == mode and row.get("pid") == self.children[-1].pid
                            for row in events), "fake must actually reach the intended stalled phase")

    def test_entrypoints_use_enrolled_owned_environment(self):
        ambient = {"KHIVE_DB": "/unowned/store", "KHIVE_PACKS": "wrong", "KHIVE_ACTOR": "wrong",
                   "KHIVE_CONFIG": "/unowned/config", "KHIVE_NO_DAEMON": "0", "FAKE_LOG": str(self.log)}
        with mock.patch.dict(os.environ, ambient):
            before = dict(os.environ)
            with KhiveMcpSession(binary=self.fake, timeout=0.3) as session:
                session.tools_list()
            self.assertEqual(dict(os.environ), before)
        call = self.calls()[0]
        self.assertEqual(call["env"]["KHIVE_NO_DAEMON"], "1")
        self.assertEqual(call["env"]["KHIVE_ACTOR"], "lambda:contract-test")
        self.assertNotIn("KHIVE_DB", call["env"])
        self.assertNotIn("KHIVE_PACKS", call["env"])
        self.assertIn('granted_actors = ["lambda:contract-test"]', call["config"])
        self.assertIn('--db', call['argv'])
        self.assertNotEqual(call['argv'][call['argv'].index('--db') + 1], ':memory:')
        self.assertNotEqual(call['env']['HOME'], before.get('HOME'))
        with OwnedContractStore() as store, mock.patch.dict(os.environ, ambient):
            parent = dict(os.environ)
            with self.adapter("legacy", store) as request:
                self.assertEqual(request(), [])
            self.assertEqual(dict(os.environ), parent)
            call = self.calls()[-1]
            self.assertEqual(call["env"]["HOME"], str(store.home))
            self.assertEqual(call["env"]["KHIVE_ACTOR"], "lambda:contract-test")
            self.assertEqual(call["env"]["KHIVE_NO_DAEMON"], "1")
            self.assertEqual(call["env"]["KHIVE_CONFIG"], str(store.config))
            self.assertNotIn("KHIVE_DB", call["env"])
            self.assertNotIn("KHIVE_PACKS", call["env"])
            self.assertIn('grant_unattributed = false', call["config"])
            self.assertEqual(call["argv"][call["argv"].index("--db") + 1], str(store.db))
        self.assert_reaped()

    def test_function_fixtures_do_not_share_store(self):
        fixtures = load_fixtures()
        for name in ("khive_session", "khive_gtd_session", "khive_memory_session", "khive_formal_session"):
            fixture = getattr(fixtures, name)
            marker = getattr(fixture, "_fixture_function_marker", None) or fixture._pytestfixturefunction
            self.assertEqual(marker.scope, "function", name)
        paths = []
        with mock.patch.dict(os.environ, {"KKERNEL_BINARY": str(self.fake), "FAKE_LOG": str(self.log)}):
            for _ in range(2):
                owner = fixtures.contract_store.__wrapped__()
                store = next(owner)
                paths.append(store.db)
                session_fixture = fixtures.khive_session.__wrapped__(store)
                session = next(session_fixture)
                self.assertEqual(session.tools_list(), [])
                session_fixture.close()
                owner.close()
                self.assertFalse(store.root.exists())
        self.assertNotEqual(*paths)
        self.assert_reaped()

    def test_create_factory_preserves_namespace_routing(self):
        fixtures = load_fixtures()
        for name in ("sample_entity", "sample_note"):
            fn = getattr(fixtures, name).__wrapped__
            import inspect
            factory = fn("local") if inspect.signature(fn).parameters else fn()
            self.assertEqual(factory()["namespace"], "local", name)
            self.assertEqual(factory(namespace="ns-alpha")["namespace"], "ns-alpha", name)

    def test_payload_serialization_retains_explicit_namespace(self):
        fixtures = load_fixtures()
        entity = fixtures.sample_entity.__wrapped__("local")()
        note = fixtures.sample_note.__wrapped__("local")()
        explicit = {"kind": "concept", "name": "ExplicitNamespace", "namespace": "ns-alpha"}
        with OwnedContractStore() as store:
            with KhiveMcpSession(binary=self.fake, store=store, env={"FAKE_LOG": str(self.log)}, timeout=0.3) as session:
                for args in (entity, note, explicit):
                    self.assertEqual(session.verb("create", args), args)
                ops = [{"tool": "create", "args": explicit}]
                self.assertEqual(session.request_batch(ops)["results"][0]["result"], explicit)
            with mock.patch.object(legacy, "BINARY", str(self.fake)):
                proc = legacy._start_server(store, env=dict(os.environ, FAKE_LOG=str(self.log)), timeout=0.3)
                try:
                    self.assertEqual(legacy._tool(proc, "create", explicit), explicit)
                finally:
                    legacy._stop_server(proc)
        self.assert_reaped()

    def test_stalled_initialize_is_bounded_and_reaped(self):
        exchange_budget = 0.25
        reap_budget = exchange_budget
        for kind in ("legacy", "pytest"):
            for mode in ("initialize_silent", "initialize_partial", "initialize_noise"):
                with self.subTest(kind=kind, mode=mode), OwnedContractStore() as store:
                    start = time.monotonic()
                    with self.assertRaises(Exception) as error:
                        with self.adapter(kind, store, mode, timeout=exchange_budget,
                                          reap_timeout=reap_budget):
                            self.fail("stalled initialize unexpectedly completed")
                    elapsed = time.monotonic() - start
                    self.assertIn("exceeded", str(error.exception))
                    self.assert_completion_envelope(elapsed, phase="stalled initialize",
                                                    exchange=exchange_budget, reap=reap_budget)
                    self.assertTrue(store.root.exists())
                    self.assert_protocol_stage(mode)
                    self.assert_reaped()

    def test_stalled_request_is_bounded_and_reaped(self):
        exchange_budget = 0.25
        reap_budget = exchange_budget
        for kind in ("legacy", "pytest"):
            for mode in ("request_silent", "request_partial", "request_noise"):
                with self.subTest(kind=kind, mode=mode), OwnedContractStore() as store:
                    with self.adapter(kind, store, mode, timeout=exchange_budget,
                                      reap_timeout=reap_budget) as request:
                        start = time.monotonic()
                        with self.assertRaises(Exception) as error:
                            request()
                        elapsed = time.monotonic() - start
                        self.assertIn("exceeded", str(error.exception))
                        self.assert_completion_envelope(elapsed, phase="stalled request",
                                                        exchange=exchange_budget, reap=reap_budget)
                        self.assert_protocol_stage(mode)
                        self.assert_reaped()

    def test_initialize_error_is_reaped(self):
        for kind in ("legacy", "pytest"):
            with self.subTest(kind=kind), OwnedContractStore() as store:
                with self.assertRaises(Exception):
                    with self.adapter(kind, store, "initialize_error"):
                        self.fail("initialization error was ignored")
                self.assert_reaped()

    def test_success_and_test_exception_reap_children(self):
        for kind in ("legacy", "pytest"):
            with self.subTest(kind=kind), OwnedContractStore() as store:
                with self.adapter(kind, store) as request:
                    self.assertEqual(request(), [])
                self.assert_reaped()
                with self.assertRaisesRegex(RuntimeError, "test body"):
                    with self.adapter(kind, store) as request:
                        self.assertEqual(request(), [])
                        raise RuntimeError("test body")
                self.assert_reaped()

    def test_close_reaps_after_kill(self):
        reap_budget = 0.12
        for kind in ("legacy", "pytest"):
            with self.subTest(kind=kind), OwnedContractStore() as store:
                # The short budget belongs to the reap: this server ignores EOF,
                # so exclude spawn, handshake and the successful request from the
                # timer that checks close's two reap waits.
                with self.adapter(kind, store, "ignore_eof", reap_timeout=reap_budget) as request:
                    self.assertEqual(request(), [])
                    if kind == "pytest":
                        session = request.__self__
                    start = time.monotonic()
                elapsed = time.monotonic() - start
                self.assert_completion_envelope(elapsed, phase="close after EOF",
                                                reap=reap_budget)
                self.assert_reaped()
                self.assertLess(self.children[-1].returncode, 0)
                if kind == "legacy":
                    legacy._stop_server(self.children[-1])
                else:
                    session.close()
                    session.close()
                self.assert_reaped()

    def test_reap_budget_is_independent_of_the_exchange_budget(self):
        from contract_harness import attach_transport

        exchange_budget = 30.0
        reap_budget = 0.05
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, bufsize=0)
        transport = attach_transport(proc, exchange_budget, reap_budget)
        try:
            self.assertEqual((transport.timeout, transport.reap_timeout),
                             (exchange_budget, reap_budget))
            # A child that never exits on its own: close returns on the reap
            # budget, the small number, not on the 30-second exchange budget.
            start = time.monotonic()
            transport.close()
            elapsed = time.monotonic() - start
            self.assert_completion_envelope(elapsed, phase="independent reap",
                                            reap=reap_budget, scheduling_floor=0.5)
            self.assertIsNotNone(proc.returncode, "independent close must reap its child")
            self.assert_reaped()
        finally:
            # This suite runs under a 20-second watchdog, so an arm that leaves a
            # 30-second sleeper behind spends the budget the later arms need.
            if proc.returncode is None:
                proc.kill()
                proc.wait(timeout=5)

        # Omitted, the reap budget is the exchange budget, so existing callers
        # keep the behaviour they had.
        other = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, bufsize=0)
        try:
            self.assertEqual(attach_transport(other, 0.25).reap_timeout, 0.25)
        finally:
            other.kill()
            other.wait(timeout=5)
            for pipe in (other.stdin, other.stdout, other.stderr):
                pipe.close()

    def test_legacy_runner_cleans_setup_failure_and_preserves_parent_home(self):
        from contextlib import redirect_stdout
        with mock.patch.object(legacy, "BINARY", str(self.fake)), mock.patch.object(legacy, "_results", []):
            for mode, expected in [("normal", True), ("initialize_error", False)]:
                with mock.patch.dict(os.environ, {"FAKE_LOG": str(self.log), "FAKE_MODE": mode}):
                    before = dict(os.environ)
                    def check(proc):
                        legacy._send(proc, "tools/list", {})
                        self.assertEqual(legacy._recv(proc)["result"]["tools"], [])
                    with redirect_stdout(io.StringIO()):
                        legacy._run_test("fake_runner", check, ambient_home=True)
                    self.assertEqual(legacy._results[-1][1], expected)
                    self.assertEqual(dict(os.environ), before)
                    call = self.calls()[-1]
                    self.assertFalse(Path(call["env"]["HOME"]).exists())
                    self.assertIn('granted_actors = ["lambda:contract-test"]', call["config"])
                    self.assert_reaped()

    def test_stderr_output_cannot_stall_response(self):
        for kind in ("legacy", "pytest"):
            with OwnedContractStore() as store:
                with self.adapter(kind, store, "stderr_flood") as request:
                    self.assertEqual(request(), [])
                self.assert_reaped()

    def test_two_backend_fixture_keeps_config_authoritative(self):
        spec = importlib.util.spec_from_file_location("coordinator_tests", ROOT / "tests/khive-contract/tests/test_coordinator_fanout.py")
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        paths = []
        with mock.patch.dict(os.environ, {"KKERNEL_BINARY": str(self.fake), "FAKE_LOG": str(self.log), "KHIVE_DB": "/unowned/store"}):
            for _ in range(2):
                fixture = module.khive_two_backend_session.__wrapped__()
                harness = next(fixture)
                self.assertNotEqual(harness.main_db, harness.tasks_db)
                paths.append(harness.main_db)
                call = self.calls()[-1]
                self.assertNotIn("--db", call["argv"])
                self.assertNotIn("KHIVE_DB", call["env"])
                self.assertIn(str(harness.main_db), call["config"])
                self.assertIn(str(harness.tasks_db), call["config"])
                self.assertIn('[packs.gtd]\nbackend = "tasks"', call["config"])
                self.assertIn('granted_actors = ["lambda:contract-test"]', call["config"])
                fixture.close()
        self.assertNotEqual(*paths)
        self.assert_reaped()


    def test_relative_binary_and_client_paths_resolve_before_private_cwd(self):
        relative = os.path.relpath(self.fake)
        with OwnedContractStore() as store:
            env = dict(os.environ, FAKE_LOG=str(self.log))
            with KhiveMcpSession(binary=relative, store=store, db=os.path.relpath(store.db),
                                 config=os.path.relpath(store.config), env=env, timeout=0.3) as session:
                self.assertEqual(session.tools_list(), [])
                call = self.calls()[-1]
                self.assertEqual(Path(call["argv"][call["argv"].index("--db") + 1]), store.db.resolve())
                self.assertEqual(Path(call["argv"][call["argv"].index("--config") + 1]), store.config.resolve())
            with mock.patch.dict(os.environ, dict(env, KKERNEL_BINARY=relative)):
                with KhiveMcpSession(store=store, timeout=0.3) as session:
                    self.assertEqual(session.tools_list(), [])
            with mock.patch.object(legacy, "BINARY", relative):
                proc = legacy._start_server(store, env=env, timeout=0.3)
                legacy._stop_server(proc)
        self.assert_reaped()


    def test_watchdog_expiry_reaps_and_permanently_closes_admission(self):
        owned = OwnedChildren()
        result = mock.Mock()
        owned.bind_result(result)
        positive = owned.spawn([sys.executable, "-c", "pass"])
        self.children.append(positive)
        self.assertEqual(positive.wait(timeout=2), 0)
        parked = owned.spawn([sys.executable, "-c", "import time; time.sleep(60)"])
        self.children.append(parked)
        owned.expire()
        self.assertTrue(owned.expired)
        result.stop.assert_called_once_with()
        self.assertLess(parked.returncode, 0)
        self.assert_reaped()
        with mock.patch(__name__ + ".REAL_POPEN") as launch:
            with self.assertRaisesRegex(TimeoutError, "admission is closed"):
                owned.spawn([sys.executable, "-c", "import time; time.sleep(60)"])
            launch.assert_not_called()
        self.assertEqual(len(owned.children), 2)
        owned.expire()  # Cleanup remains idempotent.
        self.assert_reaped()

    def test_outer_timeout_stops_owned_worker_group(self):
        worker_budget = 0.5
        reap_budget = worker_budget
        record = self.root / "worker-group.json"
        worker = self.root / "parked-worker.py"
        worker.write_text(WORKER_GROUP_CONTROL)
        start = time.monotonic()
        code = run_worker([sys.executable, str(worker), str(record)],
                          timeout=worker_budget, grace=reap_budget)
        elapsed = time.monotonic() - start
        self.assertEqual(code, 124)
        self.assert_completion_envelope(elapsed, phase="worker timeout",
                                        worker=worker_budget, reap=reap_budget)
        outcome = json.loads(record.read_text())
        self.assertEqual(outcome["worker_group"], outcome["worker_pid"])
        self.assertEqual(outcome["child_group"], outcome["worker_group"])
        self.assertTrue(outcome.get("child_reaped"), "worker must reap its child before exit")
        self.assertLess(outcome["child_returncode"], 0)
        with self.assertRaises(ProcessLookupError):
            os.killpg(outcome["worker_group"], 0)
        with self.assertRaises(ChildProcessError):
            os.waitpid(outcome["worker_pid"], os.WNOHANG)
        # A completed worker is not mislabeled as a timeout.
        self.assertEqual(run_worker([sys.executable, "-c", "pass"], timeout=2), 0)

    def test_negative_identity_only_changes_owned_child(self):
        for actor in (None, "lambda:unenrolled-contract-test"):
            with OwnedContractStore(actor=actor) as store:
                parent = dict(os.environ, KHIVE_ACTOR="ambient", KHIVE_NAMESPACE="ambient")
                before = parent.copy()
                env = store.child_env(parent)
                self.assertEqual(parent, before)
                self.assertEqual(env.get("KHIVE_ACTOR"), actor)
                self.assertNotIn("KHIVE_NAMESPACE", env)
                self.assertIn('granted_actors = ["lambda:contract-test"]', store.config.read_text())


WORKER_GROUP_CONTROL = r'''
import json, os, pathlib, signal, subprocess, sys, time
record = pathlib.Path(sys.argv[1])
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
outcome = {'worker_pid': os.getpid(), 'worker_group': os.getpgrp(),
           'child_pid': child.pid, 'child_group': os.getpgid(child.pid)}
def stop(*_):
    child.wait(timeout=1)
    outcome.update(child_reaped=True, child_returncode=child.returncode)
    record.write_text(json.dumps(outcome))
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
record.write_text(json.dumps(outcome))
while True:
    time.sleep(1)
'''


FAKE = r'''
import json, os, pathlib, sys, time
args = sys.argv[1:]
config = args[args.index('--config') + 1] if '--config' in args else os.environ.get('KHIVE_CONFIG')
with open(os.environ['FAKE_LOG'], 'a') as log:
    log.write(json.dumps({'pid': os.getpid(), 'argv': args, 'env': dict(os.environ),
                         'config': pathlib.Path(config).read_text() if config and pathlib.Path(config).exists() else ''}) + '\n')
mode = os.environ.get('FAKE_MODE', 'normal')
def stage(name):
    with open(os.environ['FAKE_LOG'], 'a') as log:
        log.write(json.dumps({'pid': os.getpid(), 'stage': name}) + '\n')

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    phase = 'initialize' if request['method'] == 'initialize' else 'request'
    if mode == phase + '_silent':
        stage(mode)
        time.sleep(60)
    if mode == phase + '_partial':
        sys.stdout.write('{"jsonrpc":"2.0","id":')
        sys.stdout.flush()
        stage(mode)
        time.sleep(60)
    if mode == phase + '_noise':
        stage(mode)
        while True:
            print(json.dumps({'jsonrpc': '2.0', 'method': 'notification'}), flush=True)
            print(json.dumps({'jsonrpc': '2.0', 'id': -1, 'result': {}}), flush=True)
            time.sleep(0.005)
    if mode == 'initialize_error' and phase == 'initialize':
        print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'error': {'message': 'synthetic initialize refusal'}}), flush=True)
        continue
    if mode == 'stderr_flood':
        sys.stderr.write('diagnostic ' * 24000)
        sys.stderr.flush()
    result = {'serverInfo': {'name': 'khive-mcp'}} if phase == 'initialize' else {'tools': []}
    if request['method'] == 'tools/call':
        ops = json.loads(request['params']['arguments']['ops'])
        envelope = {'results': [{'tool': op['tool'], 'ok': True, 'result': op['args']} for op in ops]}
        result = {'content': [{'type': 'text', 'text': json.dumps(envelope)}]}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
if mode == 'ignore_eof':
    time.sleep(60)
'''


if __name__ == "__main__":
    if "--worker" not in sys.argv:
        sys.exit(run_worker([sys.executable, __file__, "--worker", *sys.argv[1:]]))
    sys.argv.remove("--worker")
    watchdog = threading.Timer(20, WORKER_CHILDREN.expire)
    watchdog.daemon = True
    watchdog.start()
    try:
        program = unittest.main(exit=False, testRunner=BoundedTestRunner)
        sys.exit(0 if program.result.wasSuccessful() and not WORKER_CHILDREN.expired else 1)
    finally:
        watchdog.cancel()
        WORKER_CHILDREN.expire()
