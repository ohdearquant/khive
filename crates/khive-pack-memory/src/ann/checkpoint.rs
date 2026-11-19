use super::*;

#[derive(Clone, Copy)]
pub(super) struct CheckpointPublication {
    pub(super) generation: u64,
    pub(super) epoch: u64,
    pub(super) authority: WatermarkAuthority,
}

pub(super) enum CheckpointResult {
    Full {
        reopened: Option<Box<AnnBridge>>,
        base_digest: [u8; 32],
    },
    Delta(delta::DeltaPublication),
}

pub(super) enum WrittenCheckpoint {
    Full([u8; 32]),
    Delta(delta::DeltaPublication),
}

/// Persist `bridge`, raise the wildcard registry row, compact the log, then
/// reopen the just-written segment via mmap and swap it in for the Owned
/// build product (ADR-079 Amendment 1 §B; see `docs/ann.md`). A failed
/// persistence or fenced publication never installs the candidate.
/// In-memory backends install the Owned candidate before raising/compacting
/// instead, since they have no segment to reopen.
pub(super) async fn checkpoint_raise_compact_readopt(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    mut bridge: AnnBridge,
    publication: CheckpointPublication,
) -> bool {
    let CheckpointPublication {
        generation,
        epoch,
        authority,
    } = publication;
    let applied = bridge.index.last_applied_seq().unwrap_or(0);
    let namespace_set = bridge.namespace_set.clone();
    let stamp =
        |b: AnnBridge| -> AnnBridge { b.with_generation(generation).with_epoch_baseline(epoch) };

    let Some(dir) = ann_segment_dir(rt, model) else {
        // No filesystem commit record exists to re-resolve against, so
        // publish the bridge first; fresh-tail's registry check rejects and
        // evicts it until the conditional raise below succeeds.
        bridge.mark_checkpointed();
        if !install_replacing(ann, key, stamp(bridge)).await {
            // A post-scan generation already installed something newer. Do
            // not advance the registry past log rows that rejected candidate
            // may not contain; retry from the ordinary freshness path.
            return false;
        }
        if let Err(e) = raise_watermark_with_authority(rt, model, applied, authority).await {
            tracing::warn!(error = %e, "memory ann watermark publication rejected; dropping candidate bridge");
            evict_unprotected_index(ann, key).await;
            return false;
        }
        advance_note_search_watermark(rt, ann, model, applied, authority).await;
        if let Err(e) = compact_log(rt, model).await {
            tracing::warn!(error = %e, "memory ann log compaction failed (retries next checkpoint)");
        }
        return true;
    };

    match persist_file_checkpoint(rt, ann, model, &dir, &bridge, authority).await {
        Ok(CheckpointResult::Full {
            reopened,
            base_digest,
        }) => {
            let mut replacement = match reopened {
                Some(reopened) => *reopened,
                None => {
                    bridge.mark_full_checkpoint_base(base_digest);
                    bridge
                }
            };
            replacement.mark_checkpointed();
            replacement.set_namespace_set(namespace_set);
            install_replacing(ann, key, stamp(replacement)).await
        }
        Ok(CheckpointResult::Delta(publication)) => {
            bridge.mark_delta_checkpoint(&publication);
            bridge.mark_checkpointed();
            bridge.set_namespace_set(namespace_set);
            install_replacing(ann, key, stamp(bridge)).await
        }
        Err(unprotected) => {
            if unprotected {
                evict_unprotected_index(ann, key).await;
            }
            false
        }
    }
}

/// Publish through the existing registry fence while borrowing the candidate.
/// A read guard can keep an incremental incumbent available throughout file I/O.
/// `Err(true)` means registry protection was lost; other failures retain it.
pub(super) async fn persist_file_checkpoint(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
    dir: &std::path::Path,
    bridge: &AnnBridge,
    authority: WatermarkAuthority,
) -> Result<CheckpointResult, bool> {
    let applied = bridge.index.last_applied_seq().unwrap_or(0);
    // Every process writing this model's segment takes the same filesystem
    // lock. Revalidate the durable row only after acquiring it: otherwise a
    // stale publisher could overwrite a newer segment, lose its conditional
    // raise, and leave the registry ahead of the files that restart adopts.
    let _publication_lock =
        match khive_retrieval::ann::acquire_checkpoint_lock_async(dir.to_path_buf(), "memory ANN")
            .await
        {
            Ok(lock) => lock,
            Err(e) => {
                tracing::warn!(error = %e, "failed to acquire memory ANN checkpoint lock");
                return Err(false);
            }
        };
    let current_watermark = match read_own_watermark(rt, model).await {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(error = %e, "memory ANN checkpoint registry read failed");
            return Err(false);
        }
    };
    let authorized = match authority {
        WatermarkAuthority::PendingOrActive => {
            current_watermark == Some(PENDING_WATERMARK)
                || current_watermark.is_some_and(|watermark| {
                    watermark >= 0 && u64::try_from(watermark).is_ok_and(|value| value <= applied)
                })
        }
        WatermarkAuthority::Active => current_watermark.is_some_and(|watermark| {
            watermark >= 0 && u64::try_from(watermark).is_ok_and(|value| value <= applied)
        }),
        WatermarkAuthority::Recovering => {
            current_watermark == Some(ann_registry::RECOVERING_WATERMARK)
        }
    };
    if !authorized {
        tracing::info!(
            model,
            candidate_watermark = applied,
            observed_watermark = ?current_watermark,
            ?authority,
            "memory ANN checkpoint lost publication race before persistence"
        );
        return Err(false);
    }

    // A peer may have published a different overlay at the same SQL watermark.
    // The registry alone cannot distinguish that case: preserve the exact
    // segment + delta identity this candidate was built from.
    if let Some(expected) = bridge.commit_digest {
        match delta::publication_digest(dir) {
            Ok(Some(observed)) if observed == expected => {}
            Ok(observed) => {
                tracing::info!(
                    model,
                    ?observed,
                    "memory ANN checkpoint base changed before publication"
                );
                return Err(false);
            }
            Err(error) => {
                tracing::warn!(%error, model, "memory ANN checkpoint identity read failed");
                return Err(false);
            }
        }
    }

    let persisted = write_file_checkpoint(dir, bridge, delta::MAX_RETIRED_CHAIN);
    let written = match persisted {
        Ok(publication) => publication,
        Err(e) => {
            tracing::error!(error = %e, "failed to persist memory ANN checkpoint");
            // An ordinary active rebuild still has a registry-protected incumbent
            // and a retained tail. Preserve that stale fallback until a complete
            // replacement commits; pending/closed paths already evicted before
            // entering the scan and therefore have nothing unsafe to retain.
            return Err(false);
        }
    };
    #[cfg(test)]
    ann.publication_count.fetch_add(1, Ordering::SeqCst);
    if let Err(e) = raise_watermark_with_authority(rt, model, applied, authority).await {
        // Retirement or a newer checkpoint won the writer race.  This build's
        // candidates are not protected by its own registry state, so never
        // install them; the next ensure re-resolves durable state.
        tracing::warn!(error = %e, "memory ann watermark publication rejected; dropping candidate bridge");
        return Err(true);
    }
    advance_note_search_watermark(rt, ann, model, applied, authority).await;
    if let Err(e) = compact_log(rt, model).await {
        tracing::warn!(error = %e, "memory ann log compaction failed (retries next checkpoint)");
    }
    match written {
        WrittenCheckpoint::Delta(publication) => Ok(CheckpointResult::Delta(publication)),
        WrittenCheckpoint::Full(base_digest) => match load_segment(ann, dir) {
            Ok(mmap_bridge) => Ok(CheckpointResult::Full {
                reopened: Some(Box::new(mmap_bridge)),
                base_digest,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "memory ann mmap re-adoption failed; serving Owned build");
                Ok(CheckpointResult::Full {
                    reopened: None,
                    base_digest,
                })
            }
        },
    }
}

pub(super) fn write_file_checkpoint(
    dir: &std::path::Path,
    bridge: &AnnBridge,
    max_chunks: usize,
) -> Result<WrittenCheckpoint, String> {
    if bridge.needs_full_compaction_with_chain_limit(max_chunks) {
        bridge.save_atomic(dir).map(WrittenCheckpoint::Full)
    } else {
        delta::write(dir, bridge).map(WrittenCheckpoint::Delta)
    }
}

/// Outcome of the v2-segment decision table for this consumer's global scope.
pub(super) enum SegmentOutcome {
    /// An index was installed; carries the status the ensure path reports.
    Installed(AnnEnsureStatus),
    /// Live corpus is zero: no ANN candidate may be served or replayed
    /// (decision rule 5).
    Empty,
    /// No trustworthy segment: fall through to the rebuild path.
    Cold,
}

pub(super) fn effective_persisted_state(
    dir: &std::path::Path,
    base_seq: u64,
) -> Result<(u64, u64), String> {
    let base_digest = segment_commit_digest(dir)?
        .ok_or_else(|| "memory ANN segment commit vanished".to_string())?;
    Ok(delta::read_info(dir, &base_digest, base_seq)?.unwrap_or((base_seq, 0)))
}

/// ADR-079 Amendment 1 restart classifier: the 8-rule first-match decision
/// table for the memory pack's global-scope note index, followed by the
/// matching adoption action. Full table and rationale: `docs/ann.md`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn classify_and_adopt_segment(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    seg_dir: &std::path::Path,
    target_generation: u64,
    target_epoch: u64,
    details: &mut AnnWarmDetails,
) -> SegmentOutcome {
    // Rule 1: commit record absent, corrupt, or invalid length → Cold.
    let info = match read_commit_info(seg_dir) {
        Ok(Some(info)) => info,
        Ok(None) => return SegmentOutcome::Cold,
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "error reading memory v2 commit record; Cold");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 2: readable but pre-amendment (no watermark) → Cold.
    let Some(base_seq) = info.last_applied_seq else {
        tracing::info!(model = %model,
            "pre-amendment memory v2 segment (no watermark); Cold rebuild");
        return SegmentOutcome::Cold;
    };
    let (s, _persisted_delta_ops) = match effective_persisted_state(seg_dir, base_seq) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, model, "memory delta commit is invalid; Cold rebuild");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 3: configured embedder dimensions ≠ segment dimensions → Cold.
    // Read from embedder configuration, not the corpus — no storage I/O.
    match rt.embedder_dimensions(model) {
        Some(dims) if dims as u64 == info.dimensions => {}
        Some(dims) => {
            tracing::info!(model = %model,
                segment_dims = info.dimensions, live_dims = dims,
                "memory v2 segment dimension mismatch; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        None => return SegmentOutcome::Cold,
    }

    // Rule 4: own wildcard registry row absent for an extended-format state →
    // Cold after re-registering as pending.
    match read_own_watermark(rt, model).await {
        Ok(Some(watermark)) if u64::try_from(watermark).is_ok_and(|watermark| watermark > s) => {
            tracing::warn!(
                model,
                watermark,
                persisted_seq = s,
                "memory ANN publication is behind its active registry watermark; Cold rebuild"
            );
            return SegmentOutcome::Cold;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::info!(model = %model,
                "memory ann consumer registry row absent; re-registering pending, Cold rebuild");
            if let Err(e) = register_consumer(rt, model).await {
                tracing::warn!(error = %e, "memory ann consumer re-registration failed");
            }
            return SegmentOutcome::Cold;
        }
        Err(e) => {
            tracing::warn!(error = %e, "memory ann registry read failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // Rule 6, tested before rule 5 (docs/ann.md: "Evaluation order of rules
    // 5 and 6"): no tail above S → Hot, mmap load with zero corpus I/O. The
    // namespace set stays empty (conservative default) rather than paying an
    // O(N) DISTINCT corpus scan.
    match tail_exists(rt, model, s).await {
        Ok(false) => {
            return match load_segment(ann, seg_dir) {
                Ok(bridge) => {
                    let bridge = bridge
                        .with_generation(target_generation)
                        .with_epoch_baseline(target_epoch);
                    install_replacing(ann, key, bridge).await;
                    details.path = "segment_load";
                    tracing::debug!(model = %model, "memory ANN loaded Hot from v2 segment");
                    SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot)
                }
                Err(e) => {
                    tracing::warn!(error = %e, dir = %seg_dir.display(),
                        "memory Hot segment load failed; Cold rebuild");
                    SegmentOutcome::Cold
                }
            };
        }
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, "memory ann tail probe failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // A tail exists: rules 5, 7, and 8 need (live, tail) from one snapshot.
    let (live, tail) = match scope_counts(rt, model, s).await {
        Ok(counts) => counts,
        Err(e) => {
            tracing::warn!(error = %e, "memory ann scope-count read failed; Cold");
            return SegmentOutcome::Cold;
        }
    };
    #[cfg(test)]
    ann.pause_stale_tail_scope_for_test().await;

    // Rule 5: zero live corpus → Empty, regardless of tail contents.
    if live == 0 {
        return SegmentOutcome::Empty;
    }

    // Rule 7: apply ADR-079's raw-tail work limit (see its default rationale),
    // independently of delta-chain headroom. A replay that reaches the chain limit
    // publishes a full checkpoint after applying the tail.
    let threshold = replay_limit(live, ann_rebuild_threshold());
    if tail <= threshold {
        let mut bridge = match load_segment(ann, seg_dir) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, dir = %seg_dir.display(),
                    "memory Stale-tail segment load failed; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        // The earlier scope count selected this branch, but writes can land
        // before replay. Read the actual operations, raw count, and terminal
        // watermark together so the published chunk describes one snapshot.
        let IncrementalTail {
            ops,
            applied: new_s,
            raw_count,
        } = match protected_tail(rt, ann, model, s, threshold).await {
            Ok(Some(tail)) => tail,
            Ok(None) => {
                tracing::info!(model, "memory tail grew past replay cap; Cold rebuild");
                return SegmentOutcome::Cold;
            }
            Err(e) => {
                tracing::warn!(error = %e, "memory tail replay read failed; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        details.ops_applied = ops.len() as u64;
        let recorded_ops = ops.clone();
        if let Err(e) = bridge.apply_final_ops(ops, new_s) {
            tracing::warn!(error = %e, "memory tail replay failed; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        bridge.record_delta_batch(recorded_ops, new_s, raw_count);
        // Replay is cheap and in memory; the checkpoint publishes a delta
        // below the compaction bound. A non-owner serves the replayed bridge
        // without publication, so another client does not rewrite the segment.
        if !ann.builds_corpus_indexes {
            details.path = "stale_tail_replay";
            install_replacing(
                ann,
                key,
                bridge
                    .with_generation(target_generation)
                    .with_epoch_baseline(target_epoch),
            )
            .await;
            tracing::debug!(model = %model, tail,
                "memory ANN served from Stale-tail replay without checkpoint; not the warm index host");
            return SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot);
        }
        details.path = "stale_tail_publication";
        if bridge.needs_full_compaction() {
            if let Err(error) = bridge.consolidate_if_needed(checkpoint_policy(ann).consolidate_tau)
            {
                tracing::warn!(%error, "memory ANN consolidation failed before replay publication");
                return SegmentOutcome::Cold;
            }
        }
        let installed = checkpoint_raise_compact_readopt(
            rt,
            ann,
            key,
            model,
            bridge,
            CheckpointPublication {
                generation: target_generation,
                epoch: target_epoch,
                authority: WatermarkAuthority::Active,
            },
        )
        .await;
        if !installed {
            return SegmentOutcome::Cold;
        }
        tracing::debug!(model = %model, tail, "memory ANN adopted via Stale-tail replay");
        return SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot);
    }

    // Rule 8: tail above threshold → Stale-rebuild: serve the checksum-valid
    // segment while the caller's rebuild path replaces it. Cost decision,
    // never a demotion to FTS-only.
    match load_segment(ann, seg_dir) {
        Ok(bridge) => {
            tracing::info!(model = %model, tail, live,
                "memory tail above rebuild threshold; serving stale segment during rebuild");
            let bridge = bridge
                .with_generation(target_generation)
                .with_epoch_baseline(target_epoch);
            install_replacing(ann, key, bridge).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "memory Stale-rebuild segment load failed; rebuilding without serve-stale");
        }
    }
    SegmentOutcome::Cold
}
