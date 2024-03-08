// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use differential_dataflow::difference::Semigroup;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::trace::Description;
use futures_util::TryFutureExt;
use mz_dyncfg::Config;
use mz_ore::cast::CastFrom;
use mz_ore::error::ErrorExt;
use mz_persist::location::Blob;
use mz_persist_types::codec_impls::VecU8Schema;
use mz_persist_types::{Codec, Codec64};
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, oneshot, TryAcquireError};
use tracing::{debug, debug_span, trace, warn, Instrument, Span};

use crate::async_runtime::IsolatedRuntime;
use crate::batch::{BatchBuilderConfig, BatchBuilderInternal};
use crate::cfg::MiB;
use crate::fetch::FetchBatchFilter;
use crate::internal::encoding::Schemas;
use crate::internal::gc::GarbageCollector;
use crate::internal::machine::{retry_external, Machine};
use crate::internal::metrics::ShardMetrics;
use crate::internal::state::{HollowBatch, HollowBatchPart};
use crate::internal::trace::{ApplyMergeResult, FueledMergeRes};
use crate::iter::Consolidator;
use crate::{Metrics, PersistConfig, ShardId, WriterId};

/// A request for compaction.
///
/// This is similar to FueledMergeReq, but intentionally a different type. If we
/// move compaction to an rpc server, this one will become a protobuf; the type
/// parameters will become names of codecs to look up in some registry.
#[derive(Debug, Clone)]
pub struct CompactReq<T> {
    /// The shard the input and output batches belong to.
    pub shard_id: ShardId,
    /// A description for the output batch.
    pub desc: Description<T>,
    /// The updates to include in the output batch. Any data in these outside of
    /// the output descriptions bounds should be ignored.
    pub inputs: Vec<HollowBatch<T>>,
}

/// A response from compaction.
#[derive(Debug)]
pub struct CompactRes<T> {
    /// The compacted batch.
    pub output: HollowBatch<T>,
}

/// A snapshot of dynamic configs to make it easier to reason about an
/// individual run of compaction.
#[derive(Debug, Clone)]
pub struct CompactConfig {
    pub(crate) compaction_memory_bound_bytes: usize,
    pub(crate) compaction_yield_after_n_updates: usize,
    pub(crate) version: semver::Version,
    pub(crate) batch: BatchBuilderConfig,
}

impl CompactConfig {
    /// Initialize the compaction config from Persist configuration.
    pub fn new(value: &PersistConfig, writer_id: &WriterId) -> Self {
        CompactConfig {
            compaction_memory_bound_bytes: value.dynamic.compaction_memory_bound_bytes(),
            compaction_yield_after_n_updates: value.compaction_yield_after_n_updates,
            version: value.build_version.clone(),
            batch: BatchBuilderConfig::new(value, writer_id),
        }
    }
}

/// A service for performing physical and logical compaction.
///
/// This will possibly be called over RPC in the future. Physical compaction is
/// merging adjacent batches. Logical compaction is advancing timestamps to a
/// new since and consolidating the resulting updates.
#[derive(Debug)]
pub struct Compactor<K, V, T, D> {
    cfg: PersistConfig,
    metrics: Arc<Metrics>,
    sender: Sender<(
        Instant,
        CompactReq<T>,
        Machine<K, V, T, D>,
        oneshot::Sender<Result<ApplyMergeResult, anyhow::Error>>,
    )>,
    _phantom: PhantomData<fn() -> D>,
}

impl<K, V, T, D> Clone for Compactor<K, V, T, D> {
    fn clone(&self) -> Self {
        Compactor {
            cfg: self.cfg.clone(),
            metrics: Arc::clone(&self.metrics),
            sender: self.sender.clone(),
            _phantom: Default::default(),
        }
    }
}

/// In Compactor::compact_and_apply_background, the minimum amount of time to
/// allow a compaction request to run before timing it out. A request may be
/// given a timeout greater than this value depending on the inputs' size
pub(crate) const COMPACTION_MINIMUM_TIMEOUT: Config<Duration> = Config::new(
    "persist_compaction_minimum_timeout",
    Duration::from_secs(90),
    "\
    The minimum amount of time to allow a persist compaction request to run \
    before timing it out (Materialize).",
);

impl<K, V, T, D> Compactor<K, V, T, D>
where
    K: Debug + Codec,
    V: Debug + Codec,
    T: Timestamp + Lattice + Codec64,
    D: Semigroup + Codec64 + Send + Sync,
{
    pub fn new(
        cfg: PersistConfig,
        metrics: Arc<Metrics>,
        isolated_runtime: Arc<IsolatedRuntime>,
        writer_id: WriterId,
        schemas: Schemas<K, V>,
        gc: GarbageCollector<K, V, T, D>,
    ) -> Self {
        let (compact_req_sender, mut compact_req_receiver) = mpsc::channel::<(
            Instant,
            CompactReq<T>,
            Machine<K, V, T, D>,
            oneshot::Sender<Result<ApplyMergeResult, anyhow::Error>>,
        )>(cfg.compaction_queue_size);
        let concurrency_limit = Arc::new(tokio::sync::Semaphore::new(
            cfg.compaction_concurrency_limit,
        ));

        // spin off a single task responsible for executing compaction requests.
        // work is enqueued into the task through a channel
        let _worker_handle = mz_ore::task::spawn(|| "PersistCompactionScheduler", async move {
            while let Some((enqueued, req, mut machine, completer)) =
                compact_req_receiver.recv().await
            {
                assert_eq!(req.shard_id, machine.shard_id());
                let metrics = Arc::clone(&machine.applier.metrics);

                let permit = {
                    let inner = Arc::clone(&concurrency_limit);
                    // perform a non-blocking attempt to acquire a permit so we can
                    // record how often we're ever blocked on the concurrency limit
                    match inner.try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(TryAcquireError::NoPermits) => {
                            metrics.compaction.concurrency_waits.inc();
                            Arc::clone(&concurrency_limit)
                                .acquire_owned()
                                .await
                                .expect("semaphore is never closed")
                        }
                        Err(TryAcquireError::Closed) => {
                            // should never happen in practice. the semaphore is
                            // never explicitly closed, nor will it close on Drop
                            warn!("semaphore for shard {} is closed", machine.shard_id());
                            continue;
                        }
                    }
                };
                metrics
                    .compaction
                    .queued_seconds
                    .inc_by(enqueued.elapsed().as_secs_f64());

                let cfg = machine.applier.cfg.clone();
                let blob = Arc::clone(&machine.applier.state_versions.blob);
                let isolated_runtime = Arc::clone(&isolated_runtime);
                let writer_id = writer_id.clone();
                let schemas = schemas.clone();

                let compact_span =
                    debug_span!(parent: None, "compact::apply", shard_id=%machine.shard_id());
                compact_span.follows_from(&Span::current());
                let gc = gc.clone();
                mz_ore::task::spawn(|| "PersistCompactionWorker", async move {
                    let res = Self::compact_and_apply(
                        cfg,
                        blob,
                        metrics,
                        isolated_runtime,
                        req,
                        writer_id,
                        schemas,
                        &mut machine,
                        &gc,
                    )
                    .instrument(compact_span)
                    .await;

                    // we can safely ignore errors here, it's possible the caller
                    // wasn't interested in waiting and dropped their receiver
                    let _ = completer.send(res);

                    // moves `permit` into async scope so it can be dropped upon completion
                    drop(permit);
                });
            }
        });

        Compactor {
            cfg,
            metrics,
            sender: compact_req_sender,
            _phantom: PhantomData,
        }
    }

    /// Enqueues a [CompactReq] to be consumed by the compaction background task when available.
    ///
    /// Returns a receiver that indicates when compaction has completed. The receiver can be
    /// safely dropped at any time if the caller does not wish to wait on completion.
    pub fn compact_and_apply_background(
        &self,
        req: CompactReq<T>,
        machine: &Machine<K, V, T, D>,
    ) -> Option<oneshot::Receiver<Result<ApplyMergeResult, anyhow::Error>>> {
        // Run some initial heuristics to ignore some requests for compaction.
        // We don't gain much from e.g. compacting two very small batches that
        // were just written, but it does result in non-trivial blob traffic
        // (especially in aggregate). This heuristic is something we'll need to
        // tune over time.
        let should_compact = req.inputs.len() >= self.cfg.dynamic.compaction_heuristic_min_inputs()
            || req.inputs.iter().map(|x| x.parts.len()).sum::<usize>()
                >= self.cfg.dynamic.compaction_heuristic_min_parts()
            || req.inputs.iter().map(|x| x.len).sum::<usize>()
                >= self.cfg.dynamic.compaction_heuristic_min_updates();
        if !should_compact {
            self.metrics.compaction.skipped.inc();
            return None;
        }

        let (compaction_completed_sender, compaction_completed_receiver) = oneshot::channel();
        let new_compaction_sender = self.sender.clone();

        self.metrics.compaction.requested.inc();
        // NB: we intentionally pass along the input machine, as it ought to come from the
        // writer that generated the compaction request / maintenance. this machine has a
        // spine structure that generated the request, so it has a much better chance of
        // merging and committing the result than a machine kept up-to-date through state
        // diffs, which may have a different spine structure less amendable to merging.
        let send = new_compaction_sender.try_send((
            Instant::now(),
            req,
            machine.clone(),
            compaction_completed_sender,
        ));
        if let Err(_) = send {
            self.metrics.compaction.dropped.inc();
            return None;
        }

        Some(compaction_completed_receiver)
    }

    async fn compact_and_apply(
        cfg: PersistConfig,
        blob: Arc<dyn Blob + Send + Sync>,
        metrics: Arc<Metrics>,
        isolated_runtime: Arc<IsolatedRuntime>,
        req: CompactReq<T>,
        writer_id: WriterId,
        schemas: Schemas<K, V>,
        machine: &mut Machine<K, V, T, D>,
        gc: &GarbageCollector<K, V, T, D>,
    ) -> Result<ApplyMergeResult, anyhow::Error> {
        metrics.compaction.started.inc();
        let start = Instant::now();

        // pick a timeout for our compaction request proportional to the amount
        // of data that must be read (with a minimum set by PersistConfig)
        let total_input_bytes = req
            .inputs
            .iter()
            .flat_map(|batch| batch.parts.iter())
            .map(|parts| parts.encoded_size_bytes)
            .sum::<usize>();
        let timeout = Duration::max(
            // either our minimum timeout
            COMPACTION_MINIMUM_TIMEOUT.get(&cfg),
            // or 1s per MB of input data
            Duration::from_secs(u64::cast_from(total_input_bytes / MiB)),
        );

        trace!(
            "compaction request for {}MBs ({} bytes), with timeout of {}s.",
            total_input_bytes / MiB,
            total_input_bytes,
            timeout.as_secs_f64()
        );

        let compact_span = debug_span!("compact::consolidate");
        let res = tokio::time::timeout(
            timeout,
            // Compaction is cpu intensive, so be polite and spawn it on the isolated runtime.
            isolated_runtime
                .spawn_named(
                    || "persist::compact::consolidate",
                    Self::compact(
                        CompactConfig::new(&cfg, &writer_id),
                        Arc::clone(&blob),
                        Arc::clone(&metrics),
                        Arc::clone(&machine.applier.shard_metrics),
                        Arc::clone(&isolated_runtime),
                        req,
                        schemas.clone(),
                    )
                    .instrument(compact_span),
                )
                .map_err(|e| anyhow!(e)),
        )
        .await;

        let res = match res {
            Ok(res) => res,
            Err(err) => {
                metrics.compaction.timed_out.inc();
                Err(anyhow!(err))
            }
        };

        metrics
            .compaction
            .seconds
            .inc_by(start.elapsed().as_secs_f64());

        match res {
            Ok(Ok(res)) => {
                let res = FueledMergeRes { output: res.output };
                let (apply_merge_result, maintenance) = machine.merge_res(&res).await;
                maintenance.start_performing(machine, gc);
                match &apply_merge_result {
                    ApplyMergeResult::AppliedExact => {
                        metrics.compaction.applied.inc();
                        metrics.compaction.applied_exact_match.inc();
                        machine.applier.shard_metrics.compaction_applied.inc();
                        Ok(apply_merge_result)
                    }
                    ApplyMergeResult::AppliedSubset => {
                        metrics.compaction.applied.inc();
                        metrics.compaction.applied_subset_match.inc();
                        machine.applier.shard_metrics.compaction_applied.inc();
                        Ok(apply_merge_result)
                    }
                    ApplyMergeResult::NotAppliedNoMatch
                    | ApplyMergeResult::NotAppliedInvalidSince
                    | ApplyMergeResult::NotAppliedTooManyUpdates => {
                        if let ApplyMergeResult::NotAppliedTooManyUpdates = &apply_merge_result {
                            metrics.compaction.not_applied_too_many_updates.inc();
                        }
                        metrics.compaction.noop.inc();
                        for part in res.output.parts {
                            let key = part.key.complete(&machine.shard_id());
                            retry_external(
                                &metrics.retries.external.compaction_noop_delete,
                                || blob.delete(&key),
                            )
                            .await;
                        }
                        Ok(apply_merge_result)
                    }
                }
            }
            Ok(Err(err)) | Err(err) => {
                metrics.compaction.failed.inc();
                debug!(
                    "compaction for {} failed: {}",
                    machine.shard_id(),
                    err.display_with_causes()
                );
                Err(err)
            }
        }
    }

    /// Compacts input batches in bounded memory.
    ///
    /// The memory bound is broken into pieces:
    ///     1. in-progress work
    ///     2. fetching parts from runs
    ///     3. additional in-flight requests to Blob
    ///
    /// 1. In-progress work is bounded by 2 * [BatchBuilderConfig::blob_target_size]. This
    ///    usage is met at two mutually exclusive moments:
    ///   * When reading in a part, we hold the columnar format in memory while writing its
    ///     contents into a heap.
    ///   * When writing a part, we hold a temporary updates buffer while encoding/writing
    ///     it into a columnar format for Blob.
    ///
    /// 2. When compacting runs, only 1 part from each one is held in memory at a time.
    ///    Compaction will determine an appropriate number of runs to compact together
    ///    given the memory bound and accounting for the reservation in (1). A minimum
    ///    of 2 * [BatchBuilderConfig::blob_target_size] of memory is expected, to be
    ///    able to at least have the capacity to compact two runs together at a time,
    ///    and more runs will be compacted together if more memory is available.
    ///
    /// 3. If there is excess memory after accounting for (1) and (2), we increase the
    ///    number of outstanding parts we can keep in-flight to Blob.
    pub async fn compact(
        cfg: CompactConfig,
        blob: Arc<dyn Blob + Send + Sync>,
        metrics: Arc<Metrics>,
        shard_metrics: Arc<ShardMetrics>,
        isolated_runtime: Arc<IsolatedRuntime>,
        req: CompactReq<T>,
        schemas: Schemas<K, V>,
    ) -> Result<CompactRes<T>, anyhow::Error> {
        let () = Self::validate_req(&req)?;

        // We introduced a fast-path optimization in https://github.com/MaterializeInc/materialize/pull/15363
        // but had to revert it due to a very scary bug. Here we count how many of our compaction reqs
        // could be eligible for the optimization to better understand whether it's worth trying to
        // reintroduce it.
        let mut single_nonempty_batch = None;
        for batch in &req.inputs {
            if batch.len > 0 {
                match single_nonempty_batch {
                    None => single_nonempty_batch = Some(batch),
                    Some(_previous_nonempty_batch) => {
                        single_nonempty_batch = None;
                        break;
                    }
                }
            }
        }
        if let Some(single_nonempty_batch) = single_nonempty_batch {
            if single_nonempty_batch.runs.len() == 0
                && single_nonempty_batch.desc.since() != &Antichain::from_elem(T::minimum())
            {
                metrics.compaction.fast_path_eligible.inc();
            }
        }

        // compaction needs memory enough for at least 2 runs and 2 in-progress parts
        assert!(cfg.compaction_memory_bound_bytes >= 4 * cfg.batch.blob_target_size);
        // reserve space for the in-progress part to be held in-mem representation and columnar
        let in_progress_part_reserved_memory_bytes = 2 * cfg.batch.blob_target_size;
        // then remaining memory will go towards pulling down as many runs as we can
        let run_reserved_memory_bytes =
            cfg.compaction_memory_bound_bytes - in_progress_part_reserved_memory_bytes;

        let mut all_parts = vec![];
        let mut all_runs = vec![];
        let mut len = 0;

        for (runs, run_chunk_max_memory_usage) in
            Self::chunk_runs(&req, &cfg, metrics.as_ref(), run_reserved_memory_bytes)
        {
            metrics.compaction.chunks_compacted.inc();
            metrics
                .compaction
                .runs_compacted
                .inc_by(u64::cast_from(runs.len()));

            // given the runs we actually have in our batch, we might have extra memory
            // available. we reserved enough space to always have 1 in-progress part in
            // flight, but if we have excess, we can use it to increase our write parallelism
            let extra_outstanding_parts = (run_reserved_memory_bytes
                .saturating_sub(run_chunk_max_memory_usage))
                / cfg.batch.blob_target_size;
            let mut run_cfg = cfg.clone();
            run_cfg.batch.batch_builder_max_outstanding_parts = 1 + extra_outstanding_parts;
            let batch = Self::compact_runs(
                &run_cfg,
                &req.shard_id,
                &req.desc,
                runs,
                Arc::clone(&blob),
                Arc::clone(&metrics),
                Arc::clone(&shard_metrics),
                Arc::clone(&isolated_runtime),
                schemas.clone(),
            )
            .await?;
            let (parts, runs, updates) = (batch.parts, batch.runs, batch.len);
            assert!(
                (updates == 0 && parts.len() == 0) || (updates > 0 && parts.len() > 0),
                "updates={}, parts={}",
                updates,
                parts.len(),
            );

            if updates == 0 {
                continue;
            }
            // merge together parts and runs from each compaction round.
            // parts are appended onto our existing vec, and then we shift
            // the latest run offsets to account for prior parts.
            //
            // e.g. if we currently have 3 parts and 2 runs (including the implicit one from 0):
            //         parts: [k0, k1, k2]
            //         runs:  [    1     ]
            //
            // and we merge in another result with 2 parts and 2 runs:
            //         parts: [k3, k4]
            //         runs:  [    1]
            //
            // we our result will contain 5 parts and 4 runs:
            //         parts: [k0, k1, k2, k3, k4]
            //         runs:  [    1       3   4 ]
            let run_offset = all_parts.len();
            if all_parts.len() > 0 {
                all_runs.push(run_offset);
            }
            all_runs.extend(runs.iter().map(|run_start| run_start + run_offset));
            all_parts.extend(parts);
            len += updates;
        }

        Ok(CompactRes {
            output: HollowBatch {
                desc: req.desc.clone(),
                parts: all_parts,
                runs: all_runs,
                len,
            },
        })
    }

    /// Sorts and groups all runs from the inputs into chunks, each of which has been determined
    /// to consume no more than `run_reserved_memory_bytes` at a time, unless the input parts
    /// were written with a different target size than this build. Uses [Self::order_runs] to
    /// determine the order in which runs are selected.
    fn chunk_runs<'a>(
        req: &'a CompactReq<T>,
        cfg: &CompactConfig,
        metrics: &Metrics,
        run_reserved_memory_bytes: usize,
    ) -> Vec<(Vec<(&'a Description<T>, &'a [HollowBatchPart])>, usize)> {
        let ordered_runs = Self::order_runs(req);
        let mut ordered_runs = ordered_runs.iter().peekable();

        let mut chunks = vec![];
        let mut current_chunk = vec![];
        let mut current_chunk_max_memory_usage = 0;
        while let Some(run) = ordered_runs.next() {
            let run_greatest_part_size = run
                .1
                .iter()
                .map(|x| x.encoded_size_bytes)
                .max()
                .unwrap_or(cfg.batch.blob_target_size);
            current_chunk.push(*run);
            current_chunk_max_memory_usage += run_greatest_part_size;

            if let Some(next_run) = ordered_runs.peek() {
                let next_run_greatest_part_size = next_run
                    .1
                    .iter()
                    .map(|x| x.encoded_size_bytes)
                    .max()
                    .unwrap_or(cfg.batch.blob_target_size);

                // if we can fit the next run in our chunk without going over our reserved memory, we should do so
                if current_chunk_max_memory_usage + next_run_greatest_part_size
                    <= run_reserved_memory_bytes
                {
                    continue;
                }

                // NB: There's an edge case where we cannot fit at least 2 runs into a chunk
                // with our reserved memory. This could happen if blobs were written with a
                // larger target size than the current build. When this happens, we violate
                // our memory requirement and force chunks to be at least length 2, so that we
                // can be assured runs are merged and converge over time.
                if current_chunk.len() == 1 {
                    // in the steady state we expect this counter to be 0, and would only
                    // anticipate it being temporarily nonzero if we changed target blob size
                    // or our memory requirement calculations
                    metrics.compaction.memory_violations.inc();
                    continue;
                }
            }

            chunks.push((
                std::mem::take(&mut current_chunk),
                current_chunk_max_memory_usage,
            ));
            current_chunk_max_memory_usage = 0;
        }

        chunks
    }

    /// With bounded memory where we cannot compact all runs/parts together, the groupings
    /// in which we select runs to compact together will affect how much we're able to
    /// consolidate updates.
    ///
    /// This approach orders the input runs by cycling through each batch, selecting the
    /// head element until all are consumed. It assumes that it is generally more effective
    /// to prioritize compacting runs from different batches, rather than runs from within
    /// a single batch.
    ///
    /// ex.
    /// ```text
    ///        inputs                                        output
    ///     b0 runs=[A, B]
    ///     b1 runs=[C]                           output=[A, C, D, B, E, F]
    ///     b2 runs=[D, E, F]
    /// ```
    fn order_runs(req: &CompactReq<T>) -> Vec<(&Description<T>, &[HollowBatchPart])> {
        let total_number_of_runs = req.inputs.iter().map(|x| x.runs.len() + 1).sum::<usize>();

        let mut batch_runs: VecDeque<_> = req
            .inputs
            .iter()
            .map(|batch| (&batch.desc, batch.runs()))
            .collect();

        let mut ordered_runs = Vec::with_capacity(total_number_of_runs);

        while let Some((desc, mut runs)) = batch_runs.pop_front() {
            if let Some(run) = runs.next() {
                ordered_runs.push((desc, run));
                batch_runs.push_back((desc, runs));
            }
        }

        ordered_runs
    }

    /// Compacts runs together. If the input runs are sorted, a single run will be created as output.
    ///
    /// Maximum possible memory usage is `(# runs + 2) * [crate::PersistConfig::blob_target_size]`
    async fn compact_runs<'a>(
        // note: 'a cannot be elided due to https://github.com/rust-lang/rust/issues/63033
        cfg: &'a CompactConfig,
        shard_id: &'a ShardId,
        desc: &'a Description<T>,
        runs: Vec<(&'a Description<T>, &'a [HollowBatchPart])>,
        blob: Arc<dyn Blob + Send + Sync>,
        metrics: Arc<Metrics>,
        shard_metrics: Arc<ShardMetrics>,
        isolated_runtime: Arc<IsolatedRuntime>,
        real_schemas: Schemas<K, V>,
    ) -> Result<HollowBatch<T>, anyhow::Error> {
        // TODO: Figure out a more principled way to allocate our memory budget.
        // Currently, we give any excess budget to write parallelism. If we had
        // to pick between 100% towards writes vs 100% towards reads, then reads
        // is almost certainly better, but the ideal is probably somewhere in
        // between the two.
        //
        // For now, invent some some extra budget out of thin air for prefetch.
        let prefetch_budget_bytes = 2 * cfg.batch.blob_target_size;

        let mut timings = Timings::default();

        // Old style compaction operates on the encoded bytes and doesn't need
        // the real schema, so we synthesize one. We use the real schema for
        // stats though (see below).
        let fake_compaction_schema = Schemas {
            key: Arc::new(VecU8Schema),
            val: Arc::new(VecU8Schema),
        };

        let mut batch = BatchBuilderInternal::<Vec<u8>, Vec<u8>, T, D>::new(
            cfg.batch.clone(),
            Arc::clone(&metrics),
            Arc::clone(&shard_metrics),
            fake_compaction_schema,
            metrics.compaction.batch.clone(),
            desc.lower().clone(),
            Arc::clone(&blob),
            isolated_runtime,
            shard_id.clone(),
            cfg.version.clone(),
            desc.since().clone(),
            Some(desc.upper().clone()),
            true,
        );

        let mut consolidator = Consolidator::new(
            Arc::clone(&metrics),
            FetchBatchFilter::Compaction {
                since: desc.since().clone(),
            },
            prefetch_budget_bytes,
        );

        for (desc, parts) in runs {
            consolidator.enqueue_run(
                *shard_id,
                &blob,
                &metrics,
                |m| &m.compaction,
                &shard_metrics,
                desc,
                parts,
            );
        }

        let remaining_budget = consolidator.start_prefetches();
        if remaining_budget.is_none() {
            metrics.compaction.not_all_prefetched.inc();
        }

        // Reuse the allocations for individual keys and values
        let mut key_vec = vec![];
        let mut val_vec = vec![];
        loop {
            let fetch_start = Instant::now();
            let Some(updates) = consolidator.next().await? else {
                break;
            };
            timings.part_fetching += fetch_start.elapsed();
            for (k, v, t, d) in updates.take(cfg.compaction_yield_after_n_updates) {
                key_vec.clear();
                key_vec.extend_from_slice(k);
                val_vec.clear();
                val_vec.extend_from_slice(v);
                batch.add(&real_schemas, &key_vec, &val_vec, &t, &d).await?;
            }
            tokio::task::yield_now().await;
        }
        let batch = batch.finish(&real_schemas, desc.upper().clone()).await?;
        let hollow_batch = batch.into_hollow_batch();

        timings.record(&metrics);

        Ok(hollow_batch)
    }

    fn validate_req(req: &CompactReq<T>) -> Result<(), anyhow::Error> {
        let mut frontier = req.desc.lower();
        for input in req.inputs.iter() {
            if PartialOrder::less_than(req.desc.since(), input.desc.since()) {
                return Err(anyhow!(
                    "output since {:?} must be at or in advance of input since {:?}",
                    req.desc.since(),
                    input.desc.since()
                ));
            }
            if frontier != input.desc.lower() {
                return Err(anyhow!(
                    "invalid merge of non-consecutive batches {:?} vs {:?}",
                    frontier,
                    input.desc.lower()
                ));
            }
            frontier = input.desc.upper();
        }
        if frontier != req.desc.upper() {
            return Err(anyhow!(
                "invalid merge of non-consecutive batches {:?} vs {:?}",
                frontier,
                req.desc.upper()
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Timings {
    part_fetching: Duration,
    heap_population: Duration,
}

impl Timings {
    fn record(self, metrics: &Metrics) {
        // intentionally deconstruct so we don't forget to consider each field
        let Timings {
            part_fetching,
            heap_population,
        } = self;

        metrics
            .compaction
            .steps
            .part_fetch_seconds
            .inc_by(part_fetching.as_secs_f64());
        metrics
            .compaction
            .steps
            .heap_population_seconds
            .inc_by(heap_population.as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use mz_persist_types::codec_impls::{StringSchema, UnitSchema};
    use timely::progress::Antichain;

    use crate::batch::BLOB_TARGET_SIZE;

    use crate::tests::{all_ok, expect_fetch_part, new_test_client_cache, CodecProduct};
    use crate::PersistLocation;

    use super::*;

    // A regression test for a bug caught during development of #13160 (never
    // made it to main) where batches written by compaction would always have a
    // since of the minimum timestamp.
    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // unsupported operation: returning ready events from epoll_wait is not yet implemented
    async fn regression_minimum_since() {
        let data = vec![
            (("0".to_owned(), "zero".to_owned()), 0, 1),
            (("0".to_owned(), "zero".to_owned()), 1, -1),
            (("1".to_owned(), "one".to_owned()), 1, 1),
        ];

        let cache = new_test_client_cache();
        cache.cfg.set_config(&BLOB_TARGET_SIZE, 100);
        let (mut write, _) = cache
            .open(PersistLocation::new_in_mem())
            .await
            .expect("client construction failed")
            .expect_open::<String, String, u64, i64>(ShardId::new())
            .await;
        let b0 = write
            .expect_batch(&data[..1], 0, 1)
            .await
            .into_hollow_batch();
        let b1 = write
            .expect_batch(&data[1..], 1, 2)
            .await
            .into_hollow_batch();

        let req = CompactReq {
            shard_id: write.machine.shard_id(),
            desc: Description::new(
                b0.desc.lower().clone(),
                b1.desc.upper().clone(),
                Antichain::from_elem(10u64),
            ),
            inputs: vec![b0, b1],
        };
        let schemas = Schemas {
            key: Arc::new(StringSchema),
            val: Arc::new(UnitSchema),
        };
        let res = Compactor::<String, (), u64, i64>::compact(
            CompactConfig::new(&write.cfg, &write.writer_id),
            Arc::clone(&write.blob),
            Arc::clone(&write.metrics),
            write.metrics.shards.shard(&write.machine.shard_id(), ""),
            Arc::new(IsolatedRuntime::default()),
            req.clone(),
            schemas,
        )
        .await
        .expect("compaction failed");

        assert_eq!(res.output.desc, req.desc);
        assert_eq!(res.output.len, 1);
        assert_eq!(res.output.parts.len(), 1);
        let part = &res.output.parts[0];
        let (part, updates) = expect_fetch_part(
            write.blob.as_ref(),
            &part.key.complete(&write.machine.shard_id()),
            &write.metrics,
        )
        .await;
        assert_eq!(part.desc, res.output.desc);
        assert_eq!(updates, all_ok(&data, 10));
    }

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // unsupported operation: returning ready events from epoll_wait is not yet implemented
    async fn compaction_partial_order() {
        let data = vec![
            (
                ("0".to_owned(), "zero".to_owned()),
                CodecProduct::new(0, 10),
                1,
            ),
            (
                ("1".to_owned(), "one".to_owned()),
                CodecProduct::new(10, 0),
                1,
            ),
        ];

        let cache = new_test_client_cache();
        cache.cfg.set_config(&BLOB_TARGET_SIZE, 100);
        let (mut write, _) = cache
            .open(PersistLocation::new_in_mem())
            .await
            .expect("client construction failed")
            .expect_open::<String, String, CodecProduct, i64>(ShardId::new())
            .await;
        let b0 = write
            .batch(
                &data[..1],
                Antichain::from_elem(CodecProduct::new(0, 0)),
                Antichain::from_iter([CodecProduct::new(0, 11), CodecProduct::new(10, 0)]),
            )
            .await
            .expect("invalid usage")
            .into_hollow_batch();

        let b1 = write
            .batch(
                &data[1..],
                Antichain::from_iter([CodecProduct::new(0, 11), CodecProduct::new(10, 0)]),
                Antichain::from_elem(CodecProduct::new(10, 1)),
            )
            .await
            .expect("invalid usage")
            .into_hollow_batch();

        let req = CompactReq {
            shard_id: write.machine.shard_id(),
            desc: Description::new(
                b0.desc.lower().clone(),
                b1.desc.upper().clone(),
                Antichain::from_elem(CodecProduct::new(10, 0)),
            ),
            inputs: vec![b0, b1],
        };
        let schemas = Schemas {
            key: Arc::new(StringSchema),
            val: Arc::new(UnitSchema),
        };
        let res = Compactor::<String, (), CodecProduct, i64>::compact(
            CompactConfig::new(&write.cfg, &write.writer_id),
            Arc::clone(&write.blob),
            Arc::clone(&write.metrics),
            write.metrics.shards.shard(&write.machine.shard_id(), ""),
            Arc::new(IsolatedRuntime::default()),
            req.clone(),
            schemas,
        )
        .await
        .expect("compaction failed");

        assert_eq!(res.output.desc, req.desc);
        assert_eq!(res.output.len, 2);
        assert_eq!(res.output.parts.len(), 1);
        let part = &res.output.parts[0];
        let (part, updates) = expect_fetch_part(
            write.blob.as_ref(),
            &part.key.complete(&write.machine.shard_id()),
            &write.metrics,
        )
        .await;
        assert_eq!(part.desc, res.output.desc);
        assert_eq!(updates, all_ok(&data, CodecProduct::new(10, 0)));
    }
}
