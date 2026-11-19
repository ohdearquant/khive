use super::{
    capture, git_command_with_overrides, operation_filter_snapshot, tree, ListedBlob,
    LocalGitError, Result, MAX_BLOB_WHOLE_BYTES,
};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const HEADER_LIMIT: u64 = 128;
const PLACEHOLDER_REF: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Serialize)]
struct PreviewEntry<'a> {
    path: &'a str,
    #[serde(rename = "ref")]
    content_ref: &'static str,
    mode: u32,
}

#[derive(Serialize)]
struct PreviewManifest<'a> {
    schema: &'static str,
    entries: Vec<PreviewEntry<'a>>,
}

struct ManifestSize(usize);

impl Write for ManifestSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > tree::MAX_MANIFEST_BYTES as usize {
            return Err(std::io::Error::other("manifest limit"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn admit_manifest(listed: &[ListedBlob]) -> Result<()> {
    // Every stored content reference is 64 unescaped ASCII hex bytes, so its
    // value cannot affect the serialized manifest size.
    let preview = PreviewManifest {
        schema: "khive-tree/v1",
        entries: listed
            .iter()
            .map(|entry| PreviewEntry {
                path: &entry.path,
                content_ref: PLACEHOLDER_REF,
                mode: entry.mode,
            })
            .collect(),
    };
    serde_json::to_writer(ManifestSize(0), &preview)
        .map_err(|_| LocalGitError::new("output_limit", "checkout manifest exceeds the tree limit"))
}

type Cancellation = Arc<(Mutex<bool>, Condvar)>;

struct EofReader<R> {
    reader: R,
    saw_eof: bool,
}

impl<R: Read> Read for EofReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = self.reader.read(bytes)?;
        self.saw_eof |= !bytes.is_empty() && count == 0;
        Ok(count)
    }
}

impl<R: BufRead> BufRead for EofReader<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let bytes = self.reader.fill_buf()?;
        self.saw_eof |= bytes.is_empty();
        Ok(bytes)
    }

    fn consume(&mut self, count: usize) {
        self.reader.consume(count);
    }
}

struct CancelOnDrop(Cancellation);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (cancelled, changed) = &*self.0;
        *cancelled.lock().unwrap_or_else(|error| error.into_inner()) = true;
        changed.notify_all();
    }
}

struct ManagedChild {
    child: Arc<Mutex<Child>>,
    cancellation: Cancellation,
    finished: Arc<AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl ManagedChild {
    fn spawn(mut command: Command, cancellation: &Cancellation) -> Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = khive_runtime::process_retry::spawn_retrying_executable_busy(
            &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
            || command.spawn(),
        )
        .map_err(|_| LocalGitError::new("git_spawn", "could not start git cat-file"))?;
        let child = Arc::new(Mutex::new(child));
        let finished = Arc::new(AtomicBool::new(false));
        let watcher_child = Arc::clone(&child);
        let watcher_finished = Arc::clone(&finished);
        let watcher_cancellation = Arc::clone(cancellation);
        let watcher = std::thread::Builder::new()
            .name("git-object-cancellation".into())
            .spawn(move || {
                let (cancelled, changed) = &*watcher_cancellation;
                let mut cancelled = cancelled.lock().unwrap_or_else(|error| error.into_inner());
                while !*cancelled && !watcher_finished.load(Ordering::Acquire) {
                    cancelled = changed
                        .wait(cancelled)
                        .unwrap_or_else(|error| error.into_inner());
                }
                if *cancelled {
                    drop(cancelled);
                    let mut child = watcher_child
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let _ = child.kill();
                    let _ = child.wait();
                }
            });
        match watcher {
            Ok(watcher) => Ok(Self {
                child,
                cancellation: Arc::clone(cancellation),
                finished,
                watcher: Some(watcher),
            }),
            Err(_) => {
                let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                let _ = child.kill();
                let _ = child.wait();
                Err(LocalGitError::after_start("cat-file", false))
            }
        }
    }

    fn wait(&self) -> Result<ExitStatus> {
        loop {
            let status = self
                .child
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .try_wait()
                .map_err(|_| LocalGitError::after_start("cat-file", false))?;
            if let Some(status) = status {
                return Ok(status);
            }
            // Release the child handle between probes so cancellation can kill
            // a process that closed its output without exiting.
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn terminate(&self) {
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.terminate();
        let (cancelled, changed) = &*self.cancellation;
        let _guard = cancelled.lock().unwrap_or_else(|error| error.into_inner());
        self.finished.store(true, Ordering::Release);
        changed.notify_all();
        drop(_guard);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

pub(super) struct BlobFrame {
    pub(super) bytes: Vec<u8>,
    pub(super) consumed: std::sync::mpsc::Sender<()>,
}

pub(super) struct BlobReader {
    frames: tokio::sync::mpsc::Receiver<BlobFrame>,
    completion: Option<tokio::task::JoinHandle<Result<()>>>,
    _cancel: CancelOnDrop,
}

impl BlobReader {
    pub(super) async fn start(
        program: &Path,
        repo: &Path,
        listed: &[ListedBlob],
        filters: &mut Option<Arc<[String]>>,
    ) -> Result<Self> {
        let overrides = operation_filter_snapshot(program, repo, filters).await?;
        let program = program.to_path_buf();
        let repo = repo.to_path_buf();
        let oids = listed
            .iter()
            .map(|entry| entry.oid.clone())
            .collect::<Vec<_>>();
        let cancellation = Arc::new((Mutex::new(false), Condvar::new()));
        let cancel = CancelOnDrop(Arc::clone(&cancellation));
        let (sender, frames) = tokio::sync::mpsc::channel(1);
        let completion = tokio::task::spawn_blocking(move || {
            run_batch(&program, &repo, &oids, &overrides, &cancellation, sender)
        });
        Ok(Self {
            frames,
            completion: Some(completion),
            _cancel: cancel,
        })
    }

    pub(super) async fn next(&mut self) -> Result<BlobFrame> {
        if let Some(frame) = self.frames.recv().await {
            return Ok(frame);
        }
        self.complete().await?;
        Err(protocol_error("missing batch object"))
    }

    async fn complete(&mut self) -> Result<()> {
        let Some(completion) = self.completion.take() else {
            return Ok(());
        };
        completion
            .await
            .map_err(|_| LocalGitError::after_start("cat-file", false))?
    }

    pub(super) async fn finish(mut self) -> Result<()> {
        self.complete().await
    }
}

enum Header {
    Missing,
    Object { blob: bool, size: u64 },
}

fn protocol_error(message: &'static str) -> LocalGitError {
    LocalGitError::new("git_output", message)
}

fn read_header(reader: &mut impl BufRead, requested: &str) -> Result<Header> {
    let mut header = Vec::new();
    reader
        .take(HEADER_LIMIT + 1)
        .read_until(b'\n', &mut header)
        .map_err(|_| protocol_error("could not read batch header"))?;
    if header.len() as u64 > HEADER_LIMIT || header.last() != Some(&b'\n') {
        return Err(protocol_error("invalid batch header length"));
    }
    let header = std::str::from_utf8(&header[..header.len() - 1])
        .map_err(|_| protocol_error("invalid batch header"))?;
    let fields = header.split(' ').collect::<Vec<_>>();
    if fields
        .first()
        .is_none_or(|oid| !oid.eq_ignore_ascii_case(requested))
    {
        return Err(protocol_error("batch object id does not match the request"));
    }
    match fields.as_slice() {
        [_, "missing"] => Ok(Header::Missing),
        [_, kind @ ("blob" | "tree" | "commit" | "tag"), size] => {
            if size.is_empty() || !size.as_bytes().iter().all(u8::is_ascii_digit) {
                return Err(protocol_error("invalid batch object size"));
            }
            let size = size
                .parse()
                .map_err(|_| protocol_error("invalid batch object size"))?;
            Ok(Header::Object {
                blob: *kind == "blob",
                size,
            })
        }
        _ => Err(protocol_error("invalid batch header")),
    }
}

fn read_separator(reader: &mut impl Read) -> Result<()> {
    let mut separator = [0];
    reader
        .read_exact(&mut separator)
        .map_err(|_| protocol_error("incomplete batch object"))?;
    if separator != [b'\n'] {
        return Err(protocol_error("invalid batch object separator"));
    }
    Ok(())
}

fn read_blob(reader: &mut impl Read, size: u64) -> Result<Vec<u8>> {
    if size > MAX_BLOB_WHOLE_BYTES {
        return Err(LocalGitError::new(
            "output_limit",
            "git output exceeds the whole-blob limit",
        ));
    }
    let size = usize::try_from(size).map_err(|_| protocol_error("invalid batch object size"))?;
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| protocol_error("incomplete batch object"))?;
    read_separator(reader)?;
    Ok(bytes)
}

fn check_exit(status: ExitStatus) -> Result<()> {
    let exit_code = status
        .code()
        .ok_or_else(|| LocalGitError::after_start("cat-file", false))?;
    if status.success() {
        Ok(())
    } else {
        Err(LocalGitError::new(
            "git_failed",
            format!("git cat-file refused the operation with exit status {exit_code}"),
        ))
    }
}

fn fallback_blob(
    program: &Path,
    repo: &Path,
    oid: &str,
    overrides: &[String],
    cancellation: &Cancellation,
) -> Result<Vec<u8>> {
    let mut command =
        git_command_with_overrides(program, repo, &["cat-file", "blob", oid], None, overrides);
    command.stdin(Stdio::null());
    let child = ManagedChild::spawn(command, cancellation)?;
    let (stdout, stderr) = {
        let mut process = child
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            process.stdout.take().expect("piped stdout"),
            process.stderr.take().expect("piped stderr"),
        )
    };
    std::thread::scope(|scope| {
        let out = scope.spawn(move || capture(stdout, MAX_BLOB_WHOLE_BYTES as usize));
        let err = scope.spawn(move || capture(stderr, 64 * 1024));
        let status = child.wait();
        if status.is_err() {
            child.terminate();
        }
        let out = out
            .join()
            .map_err(|_| LocalGitError::after_start("cat-file", false))?
            .map_err(|_| LocalGitError::after_start("cat-file", false))?;
        err.join()
            .map_err(|_| LocalGitError::after_start("cat-file", false))?
            .map_err(|_| LocalGitError::after_start("cat-file", false))?;
        check_exit(status?)?;
        if out.1 {
            return Err(LocalGitError::new(
                "output_limit",
                "git output exceeds the whole-blob limit",
            ));
        }
        Ok(out.0)
    })
}

fn run_batch(
    program: &Path,
    repo: &Path,
    oids: &[String],
    overrides: &[String],
    cancellation: &Cancellation,
    sender: tokio::sync::mpsc::Sender<BlobFrame>,
) -> Result<()> {
    let mut command =
        git_command_with_overrides(program, repo, &["cat-file", "--batch"], None, overrides);
    command.stdin(Stdio::piped());
    let child = ManagedChild::spawn(command, cancellation)?;
    let (mut stdin, stdout, stderr) = {
        let mut process = child
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            process.stdin.take().expect("piped stdin"),
            process.stdout.take().expect("piped stdout"),
            process.stderr.take().expect("piped stderr"),
        )
    };
    std::thread::scope(|scope| {
        let stderr = scope.spawn(move || capture(stderr, 64 * 1024));
        let mut stdout = EofReader {
            reader: BufReader::new(stdout),
            saw_eof: false,
        };
        let mut result = (|| {
            for oid in oids {
                writeln!(stdin, "{oid}")
                    .map_err(|_| LocalGitError::after_start("cat-file", false))?;
                let bytes = match read_header(&mut stdout, oid)? {
                    Header::Missing => fallback_blob(program, repo, oid, overrides, cancellation)?,
                    Header::Object { blob: true, size } => read_blob(&mut stdout, size)?,
                    Header::Object { blob: false, size } => {
                        // `cat-file blob` also dereferences a tag targeting a blob.
                        let bytes = fallback_blob(program, repo, oid, overrides, cancellation)?;
                        let drained =
                            std::io::copy(&mut (&mut stdout).take(size), &mut std::io::sink())
                                .map_err(|_| protocol_error("incomplete batch object"))?;
                        if drained != size {
                            return Err(protocol_error("incomplete batch object"));
                        }
                        read_separator(&mut stdout)?;
                        bytes
                    }
                };
                let (consumed, acknowledgement) = std::sync::mpsc::channel();
                sender
                    .blocking_send(BlobFrame { bytes, consumed })
                    .map_err(|_| LocalGitError::after_start("cat-file", false))?;
                // Do not read another body while the caller is storing this one.
                acknowledgement
                    .recv()
                    .map_err(|_| LocalGitError::after_start("cat-file", false))?;
            }
            drop(stdin);
            let mut extra = [0];
            let trailing = stdout
                .read(&mut extra)
                .map_err(|_| protocol_error("could not finish batch output"))?;
            if trailing != 0 {
                return Err(protocol_error("unexpected batch output"));
            }
            check_exit(child.wait()?)?;
            Ok(())
        })();
        if result.is_err() {
            if stdout.saw_eof {
                // A native failure can truncate an otherwise valid frame; its
                // observed exit takes precedence over the resulting short read.
                match child.wait() {
                    Ok(status) => {
                        if let Err(error) = check_exit(status) {
                            result = Err(error);
                        }
                    }
                    Err(error) => {
                        child.terminate();
                        result = Err(error);
                    }
                }
            } else {
                child.terminate();
            }
        }
        stderr
            .join()
            .map_err(|_| LocalGitError::after_start("cat-file", false))?
            .map_err(|_| LocalGitError::after_start("cat-file", false))?;
        result
    })
}

#[cfg(all(test, unix))]
#[path = "local_git_checkout_batch_tests.rs"]
mod tests;
