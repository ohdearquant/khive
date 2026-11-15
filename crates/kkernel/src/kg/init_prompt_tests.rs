//! Deterministic prompt I/O with the real initializer and hook installer.

use std::io::{self, BufRead, Cursor, Read, Write};

use tempfile::TempDir;

use super::*;

const PROMPT: &[u8] = b"  Install pre-commit hook? [y/N]: ";

fn init_args(repo: &Path) -> InitArgs {
    InitArgs {
        repo: repo.to_path_buf(),
        ci: false,
        add_hooks: false,
    }
}

fn init_with_answer(repo: &Path, answer: &[u8]) -> Vec<u8> {
    let mut input = Cursor::new(answer);
    let mut output = Vec::new();
    cmd_init_with_hook_prompt(init_args(repo), |repo| {
        prompt_hook_install_with_io(repo, true, &mut input, &mut output)
    })
    .unwrap();
    assert_eq!(input.position(), answer.len() as u64);
    output
}

fn assert_hook_absent(repo: &Path) {
    let error = std::fs::symlink_metadata(repo.join(".git/hooks/pre-commit")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

fn assert_installed_hook(repo: &Path, expected_script: &[u8]) {
    let script = repo.join(".khive/kg/hooks/pre-commit");
    let installed = repo.join(".git/hooks/pre-commit");
    assert_eq!(std::fs::read(&script).unwrap(), expected_script);
    let metadata = std::fs::symlink_metadata(&installed).unwrap();
    #[cfg(unix)]
    {
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&installed).unwrap(),
            script.canonicalize().unwrap()
        );
    }
    #[cfg(not(unix))]
    assert!(metadata.is_file());
    assert_eq!(std::fs::read(installed).unwrap(), expected_script);
}

fn write_hook_script(repo: &Path) {
    let hooks = repo.join(".khive/kg/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("pre-commit"), PRE_COMMIT_HOOK).unwrap();
}

struct NoPromptIo;

impl Read for NoPromptIo {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("noninteractive init must not read prompt input");
    }
}

impl BufRead for NoPromptIo {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        panic!("noninteractive init must not inspect prompt input");
    }

    fn consume(&mut self, _: usize) {
        panic!("noninteractive init must not consume prompt input");
    }
}

impl Write for NoPromptIo {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        panic!("noninteractive init must not write a prompt");
    }

    fn flush(&mut self) -> io::Result<()> {
        panic!("noninteractive init must not flush a prompt");
    }
}

#[test]
fn terminal_yes_installs_real_hook_for_accepted_answers() {
    for answer in [b"y\n".as_slice(), b"Y\n", b"yes\n", b" YES \n"] {
        let tmp = TempDir::new().unwrap();
        assert_eq!(init_with_answer(tmp.path(), answer), PROMPT);
        assert_installed_hook(tmp.path(), PRE_COMMIT_HOOK.as_bytes());
    }
}

#[test]
fn terminal_declines_and_eof_leave_hook_uninstalled() {
    for answer in [b"n\n".as_slice(), b"N\n", b"\n", b"", b"other\n"] {
        let tmp = TempDir::new().unwrap();
        assert_eq!(init_with_answer(tmp.path(), answer), PROMPT);
        assert_eq!(
            std::fs::read(tmp.path().join(".khive/kg/hooks/pre-commit")).unwrap(),
            PRE_COMMIT_HOOK.as_bytes()
        );
        assert_hook_absent(tmp.path());
    }
}

#[test]
fn nonterminal_init_never_reads_or_writes_prompt_io() {
    let tmp = TempDir::new().unwrap();
    cmd_init_with_hook_prompt(init_args(tmp.path()), |repo| {
        prompt_hook_install_with_io(repo, false, &mut NoPromptIo, &mut NoPromptIo)
    })
    .unwrap();
    assert_eq!(
        std::fs::read(tmp.path().join(".khive/kg/hooks/pre-commit")).unwrap(),
        PRE_COMMIT_HOOK.as_bytes()
    );
    assert_hook_absent(tmp.path());
}

#[test]
fn declining_preserves_an_existing_git_hook() {
    let tmp = TempDir::new().unwrap();
    let hooks = tmp.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let installed = hooks.join("pre-commit");
    let original = b"#!/bin/sh\necho existing hook\n";
    std::fs::write(&installed, original).unwrap();
    assert_eq!(init_with_answer(tmp.path(), b"n\n"), PROMPT);
    assert!(std::fs::symlink_metadata(&installed).unwrap().is_file());
    assert_eq!(std::fs::read(installed).unwrap(), original);
}

#[test]
fn prompt_runs_after_scaffold_and_requested_ci_output() {
    let tmp = TempDir::new().unwrap();
    let mut args = init_args(tmp.path());
    args.ci = true;
    let mut calls = 0;
    let mut input = Cursor::new(b"n\n");
    let mut output = Vec::new();
    cmd_init_with_hook_prompt(args, |repo| {
        calls += 1;
        assert_eq!(
            std::fs::read(repo.join(".khive/kg/hooks/pre-commit")).unwrap(),
            PRE_COMMIT_HOOK.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.join(".github/workflows/kg-validate.yml")).unwrap(),
            CI_WORKFLOW.as_bytes()
        );
        prompt_hook_install_with_io(repo, true, &mut input, &mut output)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(output, PROMPT);
    assert_hook_absent(tmp.path());
}

#[test]
fn add_hooks_bypasses_prompt_and_uses_real_installer() {
    let tmp = TempDir::new().unwrap();
    write_hook_script(tmp.path());
    let mut args = init_args(tmp.path());
    args.add_hooks = true;
    cmd_init_with_hook_prompt(args, |_| panic!("--add-hooks must bypass the prompt")).unwrap();
    assert_installed_hook(tmp.path(), PRE_COMMIT_HOOK.as_bytes());
    assert!(!tmp.path().join(".khive/config.toml").exists());
    assert!(!tmp.path().join(".khive/kg/schema.yaml").exists());
}

#[test]
fn explicit_hook_install_still_uses_real_installer() {
    let tmp = TempDir::new().unwrap();
    write_hook_script(tmp.path());
    cmd_hook(HookCommand::Install {
        repo: tmp.path().to_path_buf(),
    })
    .unwrap();
    assert_installed_hook(tmp.path(), PRE_COMMIT_HOOK.as_bytes());
}

struct FailingOutput {
    fail_on_flush: bool,
}

impl Write for FailingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_on_flush {
            Ok(bytes.len())
        } else {
            Err(io::Error::other("forced prompt write failure"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("forced prompt flush failure"))
    }
}

struct FailingInput;

impl Read for FailingInput {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("forced prompt read failure"))
    }
}

impl BufRead for FailingInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::Error::other("forced prompt read failure"))
    }

    fn consume(&mut self, _: usize) {
        panic!("a failed prompt read must not consume input");
    }
}

#[test]
fn prompt_io_failures_refuse_before_installation() {
    for fail_on_flush in [false, true] {
        let tmp = TempDir::new().unwrap();
        let error = cmd_init_with_hook_prompt(init_args(tmp.path()), |repo| {
            prompt_hook_install_with_io(
                repo,
                true,
                &mut NoPromptIo,
                &mut FailingOutput { fail_on_flush },
            )
        })
        .unwrap_err();
        let expected = if fail_on_flush {
            "flush hook installation prompt"
        } else {
            "write hook installation prompt"
        };
        assert_eq!(error.to_string(), expected);
        assert_hook_absent(tmp.path());
    }
    let tmp = TempDir::new().unwrap();
    let mut output = Vec::new();
    let error = cmd_init_with_hook_prompt(init_args(tmp.path()), |repo| {
        prompt_hook_install_with_io(repo, true, &mut FailingInput, &mut output)
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "read hook installation answer");
    assert_eq!(output, PROMPT);
    assert_hook_absent(tmp.path());
}

#[test]
fn accepted_prompt_propagates_real_installer_failure() {
    let tmp = TempDir::new().unwrap();
    let git_path = tmp.path().join(".git");
    std::fs::write(&git_path, b"not a directory").unwrap();
    let mut input = Cursor::new(b"y\n");
    let mut output = Vec::new();
    let error = cmd_init_with_hook_prompt(init_args(tmp.path()), |repo| {
        prompt_hook_install_with_io(repo, true, &mut input, &mut output)
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("create {}", git_path.join("hooks").display())
    );
    assert_eq!(output, PROMPT);
    assert_eq!(std::fs::read(git_path).unwrap(), b"not a directory");
}

#[test]
fn rerun_offers_prompt_and_installs_the_preserved_script() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(init_with_answer(tmp.path(), b"n\n"), PROMPT);
    let script = tmp.path().join(".khive/kg/hooks/pre-commit");
    let custom = b"#!/bin/sh\necho preserved tracked hook\n";
    std::fs::write(&script, custom).unwrap();
    assert_eq!(init_with_answer(tmp.path(), b"y\n"), PROMPT);
    assert_installed_hook(tmp.path(), custom);
}
