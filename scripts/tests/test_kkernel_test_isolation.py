"""Fence the environment-writing fixture census without executing Rust or models."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "crates/kkernel/src"
ISOLATION = "if crate::test_process::run_in_child() {\n            return;\n        }"
TRIGGERS = (
    "std::env::set_var(", "std::env::remove_var(", "std::env::set_current_dir(",
    "isolate_home_for_test(", "EnvAndCwdGuard::capture(",
    "acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl(",
    "WriteQueueEnvGuard::unset(", "PacksEnvGuard::pin(",
    "run_exec_ops_file(", "run_exec_inline_with_forward(", "build_local_fallback_server(",
    "execute_atomic_ops_file(", "run_reindex(",
    "run_reindex_without_embeddings(", "run_reindex_offline(",
)
MODULES = ("exec.rs", "cli.rs", "code_ingest.rs", "reindex.rs", "pack_introspect.rs",
           "atomic_apply.rs", "atomic_project_origin_tests.rs")


def functions(source):
    # These modules use one four-space test-module level. Matching the
    # closing line at that same level keeps nested blocks and raw fixture JSON.
    for match in re.finditer(r"(?m)^    (?:async )?fn (\w+)(?:<[^>]*>)?\(", source):
        end = source.index("\n    }", match.end())
        start = source.index("{", match.end(), end) + 1
        before = max(source.rfind("\n    }", 0, match.start()),
                     source.rfind("\n    //", 0, match.start()))
        yield match.group(1), source[before:match.start()], source[start:end]


def function(filename, name):
    return next(body for found, _, body in functions((SOURCE / filename).read_text())
                if found == name)


class KkernelTestIsolationTests(unittest.TestCase):
    def test_child_environment_removes_external_model_cache_override(self):
        source = (SOURCE / "test_process.rs").read_text()
        command = source[source.index("let output = command"):source.index('.expect("spawn isolated test")')]
        self.assertIn('.env_remove("LATTICE_MODEL_CACHE")', command,
                      "exact child must not inherit an external model cache override")

    def test_environment_and_fallback_cases_enter_exact_children(self):
        count = 0
        for filename in MODULES:
            for name, attributes, body in functions((SOURCE / filename).read_text()):
                if "#[test]" not in attributes and "#[tokio::test" not in attributes:
                    continue
                if not any(trigger in body for trigger in TRIGGERS):
                    continue
                count += 1
                self.assertTrue(body.strip().startswith(ISOLATION),
                                f"{filename}:{name} must enter exact child before fixture setup")
        self.assertGreaterEqual(count, 89, "fixture census must not become vacuous")

    def test_atomic_execution_callers_enter_exact_children(self):
        count = 0
        for filename in ("exec.rs", "atomic_apply.rs", "atomic_project_origin_tests.rs"):
            for name, attributes, body in functions((SOURCE / filename).read_text()):
                if "#[tokio::test" not in attributes or "execute_atomic_ops_file(" not in body:
                    continue
                count += 1
                self.assertTrue(body.strip().startswith(ISOLATION),
                                f"{filename}:{name} atomic caller must enter exact child")
        self.assertEqual(count, 22, "all direct atomic execution witnesses must be counted")

    def test_reindex_command_cases_choose_explicit_offline_setup(self):
        zero_model_cases = {
            "run_reindex_populates_fts_without_embedding_model",
            "run_reindex_populates_entity_fts_without_embedding_model",
            "reindex_fts_fixture_clears_primary_and_additional_models",
        }
        count = 0
        for name, attributes, body in functions((SOURCE / "reindex.rs").read_text()):
            if "#[tokio::test" not in attributes:
                continue
            if not re.search(r"\brun_reindex(?:_without_embeddings|_offline)?\(", body):
                continue
            count += 1
            self.assertNotRegex(body, r"\brun_reindex\(",
                                f"{name} must choose an explicit offline reindex fixture")
            expected = "run_reindex_without_embeddings(" if name in zero_model_cases else "run_reindex_offline("
            self.assertIn(expected, body, f"{name} must preserve its intended embedding mode")
            self.assertTrue(body.strip().startswith(ISOLATION),
                            f"{name} must enter exact child before reindex setup")
        self.assertEqual(count, 8, "all reindex command witnesses must be counted")

    def test_reindex_offline_setup_replaces_each_configured_provider(self):
        body = function("reindex.rs", "run_reindex_offline")
        self.assertIn("for name in runtime.registered_embedding_model_names()", body)
        self.assertIn("runtime.register_embedder(FixedReindexEmbedder { name, dimensions })", body,
                      "offline reindex fixture must replace every native provider")
        source = (SOURCE / "reindex.rs").read_text()
        opened = source.index("let rt = open_validated_reindex_backend(cfg, validated_target.as_ref())?;")
        setup = source.index("runtime_setup(&rt)?;", opened)
        authorize = source.index(".authorize(resolved_ns)", opened)
        self.assertLess(setup, authorize, "offline setup must precede reindex work")

    def test_default_ingest_installs_offline_providers_before_custom_setup(self):
        default = function("code_ingest.rs", "code_ingest_batch")
        setup = function("code_ingest.rs", "code_ingest_batch_with_runtime_setup")
        self.assertIn("code_ingest_batch_with_runtime_setup(args, |_| Ok(()))", default)
        install = "install_test_embedders(runtime)?;"
        self.assertIn(install, setup, "default ingest must install offline providers")
        self.assertLess(setup.index(install), setup.index("runtime_setup(runtime)"),
                        "custom setup must retain its final override")
        provider = function("code_ingest.rs", "install_test_embedders")
        self.assertIn("runtime.register_embedder(FixedEmbeddingProvider { name, dimensions })", provider,
                      "offline fixture must replace each configured native provider")

    def test_gate_allow_ingest_installs_offline_providers(self):
        body = function("code_ingest.rs", "code_ingest_batch_gate_allow_persists_success_audit_event")
        self.assertIn("install_test_embedders,", body,
                      "allowed gate fixture must install offline providers before writes")


if __name__ == "__main__":
    unittest.main()
