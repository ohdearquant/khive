use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use khive_fs::atomic_publish::{
    stage_atomic_at_detailed, AtomicPublishOptions, AtomicPublishPhase, StaleTmp,
};
use khive_fs::directory_walk::{walk_to_directory, LinkContext, LinkPolicy};
use khive_fs::fd_relative::{
    c_name, open_at, open_file_at, rename_at, stat_fd, Create, FileIdentity, OpenFileOptions,
};

use super::process;
use super::record::{
    decode, encode, invalid, CoordinatorIdentity, Declaration, EpochDraft, Phase, PhaseAdvance,
    ReadExpectation, ReadObservation, RecordRefusal, MAX_RECORD_BYTES,
};

/// A validated final name and its stable, separate claim name.
#[derive(Debug, Clone)]
pub struct RecordSlot {
    final_name: OsString,
    claim_name: OsString,
}

impl RecordSlot {
    pub fn new(final_name: &OsStr) -> Result<Self, RecordRefusal> {
        check_name(final_name)?;
        let mut claim_name = final_name.to_os_string();
        claim_name.push(".claim");
        check_name(&claim_name)?;
        Ok(Self {
            final_name: final_name.to_os_string(),
            claim_name,
        })
    }
}

struct NoLinks;
impl LinkPolicy for NoLinks {
    fn before_follow(&mut self, _: &LinkContext<'_>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "maintenance authority contains a symlink",
        ))
    }
}

/// Pinned actual-main authority; no path rewrite or public descriptor escape.
pub struct RecordLocation {
    pub(super) parent: PathBuf,
    pub(super) directory: File,
    pub(super) uid: u32,
    pub(super) slot: RecordSlot,
    #[cfg(test)]
    pub(super) faults: Faults,
}

impl RecordLocation {
    pub fn open_discovered(
        actual_main_parent: &Path,
        slot: RecordSlot,
    ) -> Result<Self, RecordRefusal> {
        let mut normalized = PathBuf::new();
        for part in actual_main_parent.components() {
            match part {
                Component::RootDir | Component::Normal(_) => normalized.push(part.as_os_str()),
                _ => return Err(invalid("main parent is not normalized")),
            }
        }
        if !actual_main_parent.is_absolute()
            || normalized.as_os_str().as_bytes() != actual_main_parent.as_os_str().as_bytes()
            || actual_main_parent.as_os_str().as_bytes().contains(&0)
        {
            return Err(invalid("invalid actual-main parent"));
        }
        let directory = walk_to_directory(actual_main_parent, &mut NoLinks, 0)?
            .pop()
            .ok_or_else(|| invalid("missing authority descriptor"))?;
        // SAFETY: geteuid has no arguments or memory preconditions.
        let uid = unsafe { libc::geteuid() };
        let result = Self {
            parent: actual_main_parent.to_owned(),
            directory,
            uid,
            slot,
            #[cfg(test)]
            faults: Faults::default(),
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> Result<(), RecordRefusal> {
        let stat = stat_fd(&self.directory)?;
        validate_metadata(stat.st_mode, stat.st_uid, self.uid, true)
    }

    fn sync_publication_directory(&self) -> io::Result<()> {
        #[cfg(test)]
        if self.faults.publish_sync_error {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        self.directory.sync_all()
    }

    fn sync_reconciliation_directory(&self) -> io::Result<()> {
        #[cfg(test)]
        if self.faults.reconcile_sync_error {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        self.directory.sync_all()
    }
}

pub(super) fn validate_metadata(
    mode: libc::mode_t,
    uid: u32,
    expected: u32,
    directory: bool,
) -> Result<(), RecordRefusal> {
    let kind = if directory {
        libc::S_IFDIR
    } else {
        libc::S_IFREG
    };
    let forbidden = if directory { 0o022 } else { 0o077 };
    if mode & libc::S_IFMT != kind || uid != expected || mode & forbidden != 0 {
        return Err(invalid("untrusted authority type, owner or permissions"));
    }
    Ok(())
}

fn check_name(name: &OsStr) -> Result<(), RecordRefusal> {
    c_name(name)?;
    if name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(invalid("dot record name"));
    }
    Ok(())
}

struct Blob {
    bytes: Vec<u8>,
    identity: FileIdentity,
}

#[allow(clippy::unnecessary_cast)] // Unix field widths and signedness vary.
fn identity_from_stat(stat: &libc::stat) -> FileIdentity {
    FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    }
}

fn read_bytes(location: &RecordLocation) -> Result<Option<Blob>, RecordRefusal> {
    location.validate()?;
    #[cfg(test)]
    location.faults.before_read()?;
    let file = match open_at(&location.directory, &location.slot.final_name, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stat = stat_fd(&file)?;
    validate_metadata(stat.st_mode, stat.st_uid, location.uid, false)?;
    let identity = identity_from_stat(&stat);
    if identity != FileIdentity::at(&location.directory, &location.slot.final_name)? {
        return Err(invalid("record entry changed during open"));
    }
    let mut bytes = Vec::new();
    (&file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid("record too large"));
    }
    if identity != FileIdentity::at(&location.directory, &location.slot.final_name)? {
        return Err(invalid("record entry changed during read"));
    }
    let stat = stat_fd(&file)?;
    validate_metadata(stat.st_mode, stat.st_uid, location.uid, false)?;
    Ok(Some(Blob { bytes, identity }))
}

fn read_bound(
    location: &RecordLocation,
    expectation: &ReadExpectation,
) -> Result<Option<Declaration>, RecordRefusal> {
    read_bytes(location)?
        .map(|blob| {
            let record = decode(&blob.bytes, location.uid)?;
            expectation.check(&record, &location.parent)?;
            Ok(record)
        })
        .transpose()
}

pub fn read_record(location: &RecordLocation, expectation: &ReadExpectation) -> ReadObservation {
    match read_bound(location, expectation) {
        Ok(None) => ReadObservation::Missing,
        Ok(Some(record)) => ReadObservation::Verified(Box::new(record)),
        Err(error) => ReadObservation::Rejected(error),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("maintenance declaration writer is already claimed")]
    Busy,
    #[error(transparent)]
    Refused(#[from] RecordRefusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Unchanged,
    DestinationMayBeVisible,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishPhase {
    Validate,
    Capture,
    Atomic(AtomicPublishPhase),
    ReadBack,
    ReconcileSync,
}
#[derive(Debug, thiserror::Error)]
pub enum PublishCause {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Refused(#[from] RecordRefusal),
}
#[derive(Debug, thiserror::Error)]
#[error("maintenance publication {phase:?} ({visibility:?}): {cause}")]
pub struct PublishFailure {
    pub phase: PublishPhase,
    pub visibility: Visibility,
    #[source]
    pub cause: PublishCause,
}

fn refused(phase: PublishPhase, error: RecordRefusal) -> PublishFailure {
    PublishFailure {
        phase,
        visibility: Visibility::Unchanged,
        cause: error.into(),
    }
}
fn atomic_error(phase: AtomicPublishPhase, error: io::Error) -> PublishFailure {
    let visibility = match phase {
        AtomicPublishPhase::ValidateNames
        | AtomicPublishPhase::InspectTmp
        | AtomicPublishPhase::RefuseTmp
        | AtomicPublishPhase::RemoveTmp
        | AtomicPublishPhase::CreateTmp
        | AtomicPublishPhase::WriteTmp
        | AtomicPublishPhase::SyncTmp
        | AtomicPublishPhase::Rename => Visibility::Unchanged,
        AtomicPublishPhase::SyncDirectory => Visibility::DestinationMayBeVisible,
    };
    PublishFailure {
        phase: PublishPhase::Atomic(phase),
        visibility,
        cause: error.into(),
    }
}

#[derive(Debug)]
pub struct PublicationReceipt {
    declaration: Declaration,
}
impl PublicationReceipt {
    pub fn declaration(&self) -> &Declaration {
        &self.declaration
    }
}
struct Pending {
    declaration: Declaration,
    bytes: Vec<u8>,
    identity: FileIdentity,
}

/// The claim is held across record replacement and ambiguous publication.
/// Topic 4 must additionally retain its leases until durable completion.
pub struct DeclarationWriter {
    pub(super) location: RecordLocation,
    claim: File,
    incarnation: CoordinatorIdentity,
    pub(super) current: Option<Declaration>,
    pending: Option<Pending>,
    attempt: u64,
}

impl DeclarationWriter {
    pub fn claim(location: RecordLocation) -> Result<Self, ClaimError> {
        let incarnation = process::capture()?;
        location.validate()?;
        let claim = open_file_at(
            location.directory.as_fd(),
            &location.slot.claim_name,
            OpenFileOptions {
                read_write: true,
                create: Create::IfMissing,
                nonblock: true,
                mode: 0o600,
            },
        )
        .map_err(RecordRefusal::from)?;
        validate_claim(&location, &claim)?;
        match fs4::FileExt::try_lock(&claim) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => return Err(ClaimError::Busy),
            Err(fs4::TryLockError::Error(error)) => return Err(RecordRefusal::Io(error).into()),
        }
        validate_claim(&location, &claim)?;
        Ok(Self {
            location,
            claim,
            incarnation,
            current: None,
            pending: None,
            attempt: 0,
        })
    }

    pub fn begin(&mut self, draft: EpochDraft) -> Result<PublicationReceipt, PublishFailure> {
        self.available()?;
        if let Some(previous) = read_bound(&self.location, &ReadExpectation::writer())
            .map_err(|e| refused(PublishPhase::ReadBack, e))?
        {
            if previous.phase != Phase::Ready
                || previous.stores != draft.stores
                || previous.epoch == draft.epoch
            {
                return Err(refused(
                    PublishPhase::Validate,
                    invalid("cannot begin over closed, retargeted or same-epoch record"),
                ));
            }
        }
        let record = Declaration {
            epoch: draft.epoch,
            sequence: 1,
            phase: Phase::Closing,
            stores: draft.stores,
            target: draft.target,
            coordinator: self.incarnation.clone(),
            capability_digest: draft.capability_digest,
            phase_started_unix_ms: draft.phase_started_unix_ms,
            phase_deadline_unix_ms: draft.phase_deadline_unix_ms,
            failure_code: None,
        };
        self.publish(record)
    }

    pub fn advance(&mut self, next: PhaseAdvance) -> Result<PublicationReceipt, PublishFailure> {
        self.available()?;
        let mut record = self
            .current
            .clone()
            .ok_or_else(|| refused(PublishPhase::Validate, invalid("no owned active epoch")))?;
        if !matches!(
            (record.phase, next.phase),
            (Phase::Closing, Phase::Quiescent)
                | (Phase::Quiescent, Phase::Migrating)
                | (Phase::Migrating, Phase::Ready)
                | (
                    Phase::Closing | Phase::Quiescent | Phase::Migrating,
                    Phase::FailedClosed
                )
        ) {
            return Err(refused(
                PublishPhase::Validate,
                invalid("invalid phase transition"),
            ));
        }
        let observed = read_bound(&self.location, &ReadExpectation::writer())
            .map_err(|e| refused(PublishPhase::ReadBack, e))?;
        if observed.as_ref() != Some(&record) {
            return Err(refused(
                PublishPhase::ReadBack,
                invalid("active record changed"),
            ));
        }
        record.sequence = record
            .sequence
            .checked_add(1)
            .ok_or_else(|| refused(PublishPhase::Validate, invalid("sequence overflow")))?;
        record.phase = next.phase;
        record.phase_started_unix_ms = next.phase_started_unix_ms;
        record.phase_deadline_unix_ms = next.phase_deadline_unix_ms;
        record.failure_code = next.failure_code;
        self.publish(record)
    }

    pub fn reconcile_pending(&mut self) -> Result<PublicationReceipt, PublishFailure> {
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| refused(PublishPhase::ReadBack, invalid("no pending publication")))?;
        let outcome = (|| {
            self.location.validate()?;
            validate_claim(&self.location, &self.claim)?;
            process::same_incarnation(&self.incarnation, process::capture())?;
            let blob =
                read_bytes(&self.location)?.ok_or_else(|| invalid("pending record missing"))?;
            let record = decode(&blob.bytes, self.location.uid)?;
            ReadExpectation::writer().check(&record, &self.location.parent)?;
            if blob.bytes != pending.bytes
                || blob.identity != pending.identity
                || record != pending.declaration
            {
                return Err(invalid("pending publication readback mismatch"));
            }
            Ok(())
        })();
        outcome.map_err(|e| PublishFailure {
            phase: PublishPhase::ReadBack,
            visibility: Visibility::DestinationMayBeVisible,
            cause: e.into(),
        })?;
        self.location
            .sync_reconciliation_directory()
            .map_err(|e| PublishFailure {
                phase: PublishPhase::ReconcileSync,
                visibility: Visibility::DestinationMayBeVisible,
                cause: e.into(),
            })?;
        let pending = self
            .pending
            .take()
            .expect("pending retained until successful sync");
        self.current = Some(pending.declaration.clone());
        Ok(PublicationReceipt {
            declaration: pending.declaration,
        })
    }

    fn available(&self) -> Result<(), PublishFailure> {
        if self.pending.is_some() {
            return Err(PublishFailure {
                phase: PublishPhase::Validate,
                visibility: Visibility::DestinationMayBeVisible,
                cause: invalid("publication requires reconciliation").into(),
            });
        }
        Ok(())
    }

    fn publish(&mut self, record: Declaration) -> Result<PublicationReceipt, PublishFailure> {
        self.available()?;
        ReadExpectation::writer()
            .check(&record, &self.location.parent)
            .map_err(|e| refused(PublishPhase::Validate, e))?;
        let bytes =
            encode(&record, self.location.uid).map_err(|e| refused(PublishPhase::Validate, e))?;
        self.location
            .validate()
            .and_then(|()| validate_claim(&self.location, &self.claim))
            .map_err(|e| refused(PublishPhase::Validate, e))?;
        process::same_incarnation(&self.incarnation, process::capture())
            .map_err(|e| refused(PublishPhase::Capture, e))?;
        self.attempt = self
            .attempt
            .checked_add(1)
            .ok_or_else(|| refused(PublishPhase::Validate, invalid("attempt overflow")))?;
        let mut stage = self.location.slot.final_name.clone();
        stage.push(format!(
            ".{}.{}.{}.stage",
            record.epoch, record.sequence, self.attempt
        ));
        check_name(&stage).map_err(|e| refused(PublishPhase::Validate, e))?;
        let mut identity = None;
        stage_atomic_at_detailed(
            &self.location.directory,
            &stage,
            AtomicPublishOptions {
                stale: StaleTmp::RefuseExisting,
                mode: 0o600,
            },
            |file| {
                #[cfg(test)]
                self.location.faults.before_write()?;
                identity = Some(FileIdentity::of(file)?);
                file.write_all(&bytes)
            },
        )
        .map_err(|e| atomic_error(e.phase(), e.into_source()))?;
        rename_at(
            &self.location.directory,
            &stage,
            &self.location.directory,
            &self.location.slot.final_name,
        )
        .map_err(|e| atomic_error(AtomicPublishPhase::Rename, e))?;
        self.pending = Some(Pending {
            declaration: record.clone(),
            bytes,
            identity: identity.expect("staged file identity"),
        });
        self.location
            .sync_publication_directory()
            .map_err(|e| atomic_error(AtomicPublishPhase::SyncDirectory, e))?;
        self.pending = None;
        self.current = Some(record.clone());
        Ok(PublicationReceipt {
            declaration: record,
        })
    }
}

fn validate_claim(location: &RecordLocation, claim: &File) -> Result<(), RecordRefusal> {
    let stat = stat_fd(claim)?;
    validate_metadata(stat.st_mode, stat.st_uid, location.uid, false)?;
    if identity_from_stat(&stat)
        != FileIdentity::at(&location.directory, &location.slot.claim_name)?
    {
        return Err(invalid("stable claim entry changed"));
    }
    Ok(())
}

// Per-location seams are private and absent from release-build state.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Faults {
    pub read_error: Option<io::ErrorKind>,
    pub write_error: bool,
    pub publish_sync_error: bool,
    pub reconcile_sync_error: bool,
}
#[cfg(test)]
impl Faults {
    fn before_read(&self) -> io::Result<()> {
        if let Some(kind) = self.read_error {
            return Err(io::Error::from(kind));
        }
        Ok(())
    }
    fn before_write(&self) -> io::Result<()> {
        if self.write_error {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}
