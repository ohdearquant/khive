#!/usr/bin/env python3
"""Fixture tests for scripts/check-no-stubs.py. Run with:
    python3 scripts/test_check_no_stubs.py
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT_PATH = Path(__file__).resolve().parent / "check-no-stubs.py"
_spec = importlib.util.spec_from_file_location("check_no_stubs", SCRIPT_PATH)
check_no_stubs = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(check_no_stubs)


class NoStubScanTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.crates_dir = self.root / "crates"
        self.crates_dir.mkdir(parents=True)
        self._orig = (check_no_stubs.REPO_ROOT, check_no_stubs.CRATES_DIR)
        check_no_stubs.REPO_ROOT = self.root
        check_no_stubs.CRATES_DIR = self.crates_dir

    def tearDown(self):
        check_no_stubs.REPO_ROOT, check_no_stubs.CRATES_DIR = self._orig
        self._tmp.cleanup()

    def scan(self, content, relpath="khive-x/src/lib.rs"):
        path = self.crates_dir / relpath
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        if check_no_stubs.is_test_path(path):
            return []
        return check_no_stubs.scan_file(path, path.relative_to(self.root))

    # --- must fail ---

    def test_todo_bang_parens_fails(self):
        self.assertEqual(len(self.scan("fn f() {\n    todo!()\n}\n")), 1)

    def test_todo_bang_spaced_braces_fails(self):
        self.assertEqual(len(self.scan("fn f() {\n    todo ! {}\n}\n")), 1)

    def test_unimplemented_bang_brackets_fails(self):
        self.assertEqual(len(self.scan("fn f() {\n    unimplemented![]\n}\n")), 1)

    def test_dbg_fails(self):
        self.assertEqual(len(self.scan("fn f() {\n    dbg!(x);\n}\n")), 1)

    def test_panic_with_not_implemented_message_fails(self):
        findings = self.scan('fn f() {\n    panic!("not implemented");\n}\n')
        self.assertEqual(len(findings), 1)
        self.assertIn("stub marker", findings[0][1])

    def test_unreachable_with_stub_message_fails(self):
        findings = self.scan('fn f() {\n    unreachable!("stub");\n}\n')
        self.assertEqual(len(findings), 1)

    def test_multiline_conditional_panic_fails(self):
        content = (
            "fn f(x: i32) {\n"
            "    if x < 0 {\n"
            '        panic!(\n'
            '            "value {} is a placeholder that must be replaced",\n'
            "            x\n"
            "        );\n"
            "    }\n"
            "}\n"
        )
        findings = self.scan(content)
        self.assertEqual(len(findings), 1)

    # --- must pass ---

    def test_bare_unreachable_passes(self):
        self.assertEqual(self.scan("fn f() {\n    unreachable!()\n}\n"), [])

    def test_unreachable_ordinary_message_passes(self):
        self.assertEqual(self.scan('fn f() {\n    unreachable!("invalid state")\n}\n'), [])

    def test_ordinary_diagnostic_panic_passes(self):
        self.assertEqual(self.scan('fn f(e: &str) {\n    panic!("io error: {e}")\n}\n'), [])

    def test_macro_text_in_comment_passes(self):
        content = "// TODO: consider dbg!() here\nfn f() {}\n"
        self.assertEqual(self.scan(content), [])

    def test_macro_text_in_string_passes(self):
        content = 'fn f() -> &\'static str {\n    "call todo!() to stub this"\n}\n'
        self.assertEqual(self.scan(content), [])

    def test_test_paths_excluded_by_directory(self):
        path = self.crates_dir / "khive-x/tests/it.rs"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("fn f() { todo!() }\n", encoding="utf-8")
        self.assertTrue(check_no_stubs.is_test_path(path))

    def test_test_paths_excluded_by_filename(self):
        path = self.crates_dir / "khive-x/src/tests.rs"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("fn f() { todo!() }\n", encoding="utf-8")
        self.assertTrue(check_no_stubs.is_test_path(path))

    def test_inline_cfg_test_mod_excluded(self):
        content = (
            "fn real() {}\n\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    #[test]\n"
            "    fn t() {\n"
            '        panic!("stub service must not be called")\n'
            "    }\n"
            "}\n"
        )
        self.assertEqual(self.scan(content), [])

    def test_panic_with_comment_before_string_message_passes(self):
        content = 'fn f() {\n    panic!(/* not implemented */ "valid");\n}\n'
        self.assertEqual(self.scan(content), [])

    def test_panic_with_identifier_argument_passes(self):
        content = (
            'fn f(placeholder_reason: &str) {\n'
            '    panic!(placeholder_reason);\n'
            "}\n"
        )
        self.assertEqual(self.scan(content), [])

    def test_panic_stub_word_only_in_format_argument_passes(self):
        content = (
            'fn f(placeholder_reason: &str) {\n'
            '    panic!("valid: {}", placeholder_reason);\n'
            "}\n"
        )
        self.assertEqual(self.scan(content), [])

    def test_stub_word_outside_cfg_test_mod_still_fails(self):
        content = (
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    fn t() {}\n"
            "}\n\n"
            'fn real() { panic!("stub not allowed here") }\n'
        )
        self.assertEqual(len(self.scan(content)), 1)


if __name__ == "__main__":
    unittest.main()
