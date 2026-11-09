//! Measures how long a merge holds a partition's `flock`, on real files in a temp dir, and how
//! long a seal's `publish` waits on it.
//!
//! `#[ignore]`d: they write hundreds of MiB and print a table, they assert
//! nothing about speed. Run in release, one at a time:
//!
//! ```text
//! cargo test --release -p tars-melt --features parquet lock_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A merge writes its output outside the lock and takes the lock
//! only to plan, to land (rename + manifest replaced) and to tidy. So each
//! merge is timed three ways on identical copies of one partition: the real
//! `maintain` with its merge step timed from inside (the rest of `maintain`
//! is the locked part), the same pass replayed through the same functions
//! with each locked section timed on its own, and the real `maintain` on one
//! thread while another thread `publish`es into the partition mid-merge (how
//! long a seal is stalled). A control holds the lock by hand for a known
//! time, so a stall the race does not show is one it could have seen.
//!
//! Rows are synthetic, sized after the arc workload's real `llm_call` bodies
//! (tigershark `~/projects/arc/.arc/tars`, 2026-07-20 … 09-16, 12,361 calls).

use std::cell::{Cell, RefCell};
use std::thread;
use std::time::{Duration, Instant};

use tars_types::{
    CallId, Millis, OutputLimit, ProviderId, SessionId, StopReason, TenantId, TraceId, Usage,
    UtcDay, ValidationSummary,
};
use tempfile::TempDir;

use super::*;
use crate::event::call_log::{
    CALL_LOG_COMPACTION, CALL_LOG_SINK, CallBody, CallDispatch, CallResponse, LlmCallRow,
    LlmCallTable, SentEnd, StreamEnd,
};
use crate::event::log::LogTable;
use crate::event::log::seal::row_groups;

const RUNS: usize = 3;
/// How long the merging thread runs before the sealing thread publishes: far
/// more than taking the lock needs, far less than any merge measured here.
const HEAD_START: Duration = Duration::from_millis(20);
/// How long the control holds the lock by hand before releasing it.
const CONTROL_HOLD: Duration = Duration::from_millis(500);
/// 2026-07-21T00:00:00Z, the busiest day of the arc workload.
const DAY_START: Millis = 1_784_592_000_000;

/// `(quantile, bytes)` of the real request bodies (10,778 found) and
/// response bodies (9,534 found); a size is interpolated between them.
const REQUEST_BYTES: [(f64, f64); 6] = [
    (0.0, 858.0),
    (0.1, 9_682.0),
    (0.5, 37_808.0),
    (0.9, 148_158.0),
    (0.99, 198_051.0),
    (1.0, 271_821.0),
];
const RESPONSE_BYTES: [(f64, f64); 6] = [
    (0.0, 373.0),
    (0.1, 532.0),
    (0.5, 1_889.0),
    (0.9, 11_834.0),
    (0.99, 64_772.0),
    (1.0, 82_023.0),
];
/// `(calls in one second, seconds with that many)` over 2026-07-21: 2,751
/// calls in 1,324 distinct seconds. At one seal a second, one file each.
const BUSIEST_DAY: [(usize, usize); 27] = [
    (1, 975),
    (2, 105),
    (3, 87),
    (4, 57),
    (5, 35),
    (6, 13),
    (7, 1),
    (8, 4),
    (9, 3),
    (10, 5),
    (11, 4),
    (12, 2),
    (13, 6),
    (14, 3),
    (15, 4),
    (16, 3),
    (17, 3),
    (18, 2),
    (19, 1),
    (20, 2),
    (23, 1),
    (24, 1),
    (26, 1),
    (27, 2),
    (31, 2),
    (37, 1),
    (40, 1),
];

#[test]
#[ignore = "b/81 measurement: writes ~300 MiB to a temp dir and prints a table"]
fn b81_layer_merge_of_16_full_files() {
    let mut load = Workload::new(81);
    let mut source = Source::new();
    let seals: Vec<SealTiming> = (0..CALL_LOG_COMPACTION.fan_in())
        .map(|_| source.seal(load.full_batch()))
        .collect();
    report_seals("16 full L0 seals", &seals);

    let runs: Vec<MergeRun> = (0..RUNS)
        .map(|_| measure(&source, &upkeep(false)))
        .collect();
    report_merge("layer merge 16 → 1", &source, &runs);

    let controls: Vec<Duration> = (0..RUNS).map(|_| held_by_hand(&source)).collect();
    report_durations(
        &format!(
            "control: publish behind a lock held by hand for {:.0} ms",
            ms(CONTROL_HOLD)
        ),
        "publish stalled",
        &controls,
    );
}

#[test]
#[ignore = "b/81 measurement: builds a whole day of files and prints a table"]
fn b81_whole_past_day_merge() {
    let mut load = Workload::new(90);
    let batches = load.busiest_day();

    let mut tiered = Source::new();
    let during_day: Vec<(SealTiming, Maintained)> = batches
        .iter()
        .map(|rows| {
            let seal = tiered.seal(rows.clone());
            (seal, maintain(&tiered.dir, &upkeep(false)))
        })
        .collect();
    let seals: Vec<SealTiming> = during_day.iter().map(|(s, _)| *s).collect();
    report_seals("the day's 1-second seals", &seals);
    report_durations(
        "tiered upkeep after each seal",
        "maintain, whole pass",
        &during_day.iter().map(|(_, m)| m.total).collect::<Vec<_>>(),
    );
    row(
        "maintain, lock held",
        during_day.iter().map(|(_, m)| m.locked()),
    );

    let runs: Vec<MergeRun> = (0..RUNS).map(|_| measure(&tiered, &upkeep(true))).collect();
    report_merge("past day, tiered all day → 1", &tiered, &runs);

    let mut flat = Source::new();
    for rows in batches {
        flat.seal(rows);
    }
    let runs: Vec<MergeRun> = (0..RUNS).map(|_| measure(&flat, &upkeep(true))).collect();
    report_merge("past day, never merged → 1", &flat, &runs);
}

/// A fresh writer's round and the day's age: its own `.tmp`s are the only ones it may sweep
/// young, and the bench's sealer is another writer.
fn upkeep(day_is_past: bool) -> (Round, DayAge) {
    let round = Round {
        compact: Some(CALL_LOG_COMPACTION),
        uploads: false,
        grace: CALL_LOG_SINK.seal.grace,
        now: SystemTime::now(),
        writer: WriterId::new(),
        in_flight: InFlight::default(),
    };
    let age = if day_is_past {
        DayAge::Past { freeze: false }
    } else {
        DayAge::Current
    };
    (round, age)
}

/// The day every bench partition is on; the bench does not read the day.
fn bench_day() -> UtcDay {
    UtcDay::of(0).expect("the epoch has a day")
}

/// A partition built once and copied for every measurement, so each run
/// merges the same files and none sees another's output.
struct Source {
    _root: TempDir,
    dir: PartitionDir,
    sealer: WriterId,
    seq: u64,
    rows: usize,
    raw_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct SealTiming {
    /// Encode + write `.tmp` + fsync, outside the lock.
    write: Duration,
    /// `publish`: the lock, rename, manifest replaced.
    publish: Duration,
}

impl Source {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let dir = PartitionDir::new(&root.path().join("p"), bench_day());
        dir.create().expect("partition dir");
        Self {
            _root: root,
            dir,
            sealer: WriterId::new(),
            seq: 0,
            rows: 0,
            raw_bytes: 0,
        }
    }

    /// One seal exactly as `ParquetWriter::publish_by` does it.
    fn seal(&mut self, rows: Vec<LlmCallRow>) -> SealTiming {
        self.rows += rows.len();
        self.raw_bytes += rows.iter().map(LlmCallTable::approx_bytes).sum::<usize>() as u64;
        let (tmp, sealed) = (
            self.sealer.seal_tmp(self.seq),
            self.sealer.sealed_file(self.seq),
        );
        self.seq += 1;

        let groups = row_groups::<LlmCallTable>(rows);
        let write = timed(|| file::write::<LlmCallTable>(&self.dir.tmp_path(&tmp), &groups)).1;
        let publish = timed(|| self.dir.publish(&tmp, sealed)).1;
        SealTiming { write, publish }
    }

    fn copy(&self) -> Source {
        let copy = Source::new();
        for entry in fs::read_dir(self.dir.path()).expect("read partition") {
            let entry = entry.expect("dir entry");
            if entry.file_name() != LOCK {
                fs::copy(entry.path(), copy.dir.path().join(entry.file_name())).expect("copy");
            }
        }
        Source {
            rows: self.rows,
            raw_bytes: self.raw_bytes,
            ..copy
        }
    }

    fn footprint(&self) -> Footprint {
        let manifest = self.dir.manifest().expect("manifest");
        let files = manifest.live.len();
        let bytes = manifest
            .live
            .iter()
            .map(|f| {
                fs::metadata(self.dir.file_path(&f.file))
                    .expect("live file")
                    .len()
            })
            .sum();
        Footprint { files, bytes }
    }
}

struct Footprint {
    files: usize,
    bytes: u64,
}

/// One real `maintain`, its merge step timed from inside.
#[derive(Debug, Clone, Copy)]
struct Maintained {
    total: Duration,
    /// Inside `file::merge`: outside the lock.
    merging: Duration,
}

impl Maintained {
    /// The rest of the pass: planning, landing and tidying, each under the
    /// lock (plus the unlocked bookkeeping between them, which is µs).
    fn locked(&self) -> Duration {
        self.total.saturating_sub(self.merging)
    }
}

struct MergeRun {
    /// The real `maintain`, uncontended.
    plain: Maintained,
    steps: Steps,
    raced: Raced,
    output_bytes: u64,
}

/// The real `maintain` with a `publish` racing it.
struct Raced {
    pass: Maintained,
    /// How many merges the pass ran.
    merges: usize,
    /// The `publish`, started `HEAD_START` after the pass.
    stalled_publish: Duration,
}

/// One pass replayed through the same functions `maintain_by` calls; every
/// step but `merge` holds the lock.
struct Steps {
    plan: Duration,
    merge: Duration,
    land_lock: Duration,
    land_manifest_read: Duration,
    land_rename: Duration,
    land_manifest_replace: Duration,
    /// The `next_plan` that finds nothing more due.
    replan: Duration,
    tidy: Duration,
}

impl Steps {
    fn land(&self) -> Duration {
        self.land_lock + self.land_manifest_read + self.land_rename + self.land_manifest_replace
    }

    fn locked(&self) -> Duration {
        self.plan + self.land() + self.replan + self.tidy
    }

    fn sum(&self) -> Duration {
        self.locked() + self.merge
    }
}

fn measure(source: &Source, upkeep: &(Round, DayAge)) -> MergeRun {
    let plain = source.copy();
    let plain_run = maintain(&plain.dir, upkeep);
    let output_bytes = plain.footprint().bytes;

    let replay = source.copy();
    let steps = replay_pass(&replay.dir, upkeep);

    let raced = source.copy();
    let raced = race_publish(&raced, upkeep);

    MergeRun {
        plain: plain_run,
        steps,
        raced,
        output_bytes,
    }
}

/// The real `maintain`, with `file::merge` as its merge step, timed.
fn maintain(dir: &PartitionDir, (round, age): &(Round, DayAge)) -> Maintained {
    let merging = Cell::new(Duration::ZERO);
    let merge = |inputs: &[PathBuf], out: &Path| {
        let ((), took) = timed(|| file::merge::<LlmCallTable>(inputs, out));
        merging.set(merging.get() + took);
        Ok(())
    };
    let ((), total) = timed(|| dir.maintain_by(round, *age, merge));
    Maintained {
        total,
        merging: merging.get(),
    }
}

/// `maintain_by` step by step, one merge due: `next_plan`, the merge into
/// this writer's `.tmp`, `land`'s body, `next_plan` again, `tidy`.
fn replay_pass(dir: &PartitionDir, (round, age): &(Round, DayAge)) -> Steps {
    let rule = round.compact.expect("the bench always merges");
    let (due, plan) = timed(|| dir.next_plan(rule, *age != DayAge::Current));
    let due = due.expect("a merge is due");

    let output = due.output();
    let tmp_file = round.writer.merge_tmp(&output);
    let tmp = dir.tmp_path(&tmp_file);
    let claim = round.in_flight.claim(tmp_file);
    let inputs: Vec<PathBuf> = due.inputs.iter().map(|f| dir.file_path(f)).collect();
    let merge = timed(|| file::merge::<LlmCallTable>(&inputs, &tmp)).1;

    let (lock, land_lock) = timed(|| dir.lock());
    let (manifest, land_manifest_read) = timed(|| dir.manifest());
    let merged = manifest
        .retire(due, output.clone(), round.now)
        .expect("nothing else merges this partition");
    let land_rename = timed(|| dir.rename_into(&tmp, &output)).1;
    let land_manifest_replace = timed(|| dir.write_manifest(&merged)).1;
    drop(lock);
    drop(claim);

    let (more, replan) = timed(|| dir.next_plan(rule, *age != DayAge::Current));
    assert_eq!(more, None, "one merge is all maintain would have done");
    let tidy = timed(|| dir.tidy(round)).1;

    Steps {
        plan,
        merge,
        land_lock,
        land_manifest_read,
        land_rename,
        land_manifest_replace,
        replan,
        tidy,
    }
}

/// The real `maintain` on one thread; `HEAD_START` later a one-row seal
/// publishes into the same partition from this one — two `flock`s on two
/// open files, exactly as a sealer and a merge contend. Fails the bench if
/// the publish did not land while a merge was running: then it measured
/// nothing about contention. On a past day the probe is a late row, so the
/// pass merges the day once more after the merge it raced.
fn race_publish(partition: &Source, upkeep: &(Round, DayAge)) -> Raced {
    let probe = probe_tmp(partition);

    let merging = partition.dir.clone();
    let (round, age) = upkeep.clone();
    let merge = thread::spawn(move || {
        let windows = RefCell::new(Vec::new());
        let merge = |inputs: &[PathBuf], out: &Path| {
            let start = Instant::now();
            file::merge::<LlmCallTable>(inputs, out)?;
            windows.borrow_mut().push((start, Instant::now()));
            Ok(())
        };
        let ((), total) = timed(|| merging.maintain_by(&round, age, merge));
        (total, windows.into_inner())
    });
    thread::sleep(HEAD_START);
    let published_at = Instant::now();
    let stalled = timed(|| partition.dir.publish(&probe.0, probe.1.clone())).1;
    let published_by = published_at + stalled;
    let (total, windows) = merge.join().expect("merge thread");

    assert!(
        windows
            .iter()
            .any(|&(start, end)| start <= published_at && published_by <= end),
        "the publish must land while a merge runs to measure the contention"
    );
    let merging = windows.iter().map(|&(start, end)| end - start).sum();
    Raced {
        pass: Maintained { total, merging },
        merges: windows.len(),
        stalled_publish: stalled,
    }
}

/// Positive control: a lock held by hand for `CONTROL_HOLD` stalls a
/// `publish` into the same partition, as a merge holding it would.
fn held_by_hand(source: &Source) -> Duration {
    let partition = source.copy();
    let probe = probe_tmp(&partition);

    let holder = partition.dir.clone();
    let (taken, released) = std::sync::mpsc::channel();
    let hold = thread::spawn(move || {
        let lock = holder.lock().expect("lock");
        taken.send(()).expect("signal");
        thread::sleep(CONTROL_HOLD);
        drop(lock);
    });
    released.recv().expect("lock taken");
    thread::sleep(HEAD_START);
    let stalled = timed(|| partition.dir.publish(&probe.0, probe.1.clone())).1;
    hold.join().expect("holder thread");
    stalled
}

/// A one-row `.tmp` sealed by `partition`'s sealer, ready to publish as its data file.
fn probe_tmp(partition: &Source) -> (TmpFile, DataFile) {
    let mut load = Workload::new(7);
    let tmp = partition.sealer.seal_tmp(u64::MAX);
    let groups = row_groups::<LlmCallTable>(vec![load.row()]);
    file::write::<LlmCallTable>(&partition.dir.tmp_path(&tmp), &groups).expect("probe tmp");
    (tmp, partition.sealer.sealed_file(u64::MAX))
}

/// Run `f`, fail the bench on its error, and say how long it took.
fn timed<R>(f: impl FnOnce() -> Result<R, LogError>) -> (R, Duration) {
    let start = Instant::now();
    let out = f().expect("bench step");
    (out, start.elapsed())
}

/// Synthetic `llm_call` rows: request and response bodies drawn from the
/// real size quantiles; text is one of a few shared system prompts, then
/// words from a small vocabulary — shaped like the real JSON bodies, where
/// the prompt repeats across calls and the rest (source, answers) does not.
struct Workload {
    rng: XorShift,
    vocab: Vec<String>,
    prompts: Vec<String>,
    at_ms: Millis,
}

const VOCAB: usize = 1_024;
const PROMPTS: usize = 8;
const PROMPT_BYTES: usize = 6_000;

impl Workload {
    fn new(seed: u64) -> Self {
        let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let vocab: Vec<String> = (0..VOCAB).map(|_| rng.word()).collect();
        let prompts = (0..PROMPTS)
            .map(|_| words(&mut rng, &vocab, PROMPT_BYTES))
            .collect();
        Self {
            rng,
            vocab,
            prompts,
            at_ms: DAY_START,
        }
    }

    fn row(&mut self) -> LlmCallRow {
        let request_len = self.rng.size(&REQUEST_BYTES);
        let response_len = self.rng.size(&RESPONSE_BYTES);
        let prompt = &self.prompts[self.rng.below(PROMPTS)];
        let request = format!(
            r#"{{"system":"{prompt}","messages":[{{"role":"user","content":"{}"}}]}}"#,
            words(
                &mut self.rng,
                &self.vocab,
                request_len.saturating_sub(PROMPT_BYTES)
            )
        );
        let response = format!(
            r#"{{"content":[{{"text":"{}"}}]}}"#,
            words(&mut self.rng, &self.vocab, response_len)
        );
        self.at_ms += 1 + self.rng.below(30_000) as Millis;
        call_row(self.at_ms, request, response)
    }

    /// Rows until the next would overflow the call log's `max_bytes`, or
    /// `max_rows` is reached: a file at the seal cap.
    fn full_batch(&mut self) -> Vec<LlmCallRow> {
        let cap = CALL_LOG_SINK.seal;
        let mut rows = Vec::new();
        let mut bytes = 0;
        loop {
            let row = self.row();
            let len = LlmCallTable::approx_bytes(&row);
            if rows.len() == cap.max_rows || bytes + len > cap.max_bytes {
                return rows;
            }
            bytes += len;
            rows.push(row);
        }
    }

    /// One batch per second of 2026-07-21 that saw a call, in a shuffled order.
    fn busiest_day(&mut self) -> Vec<Vec<LlmCallRow>> {
        let mut sizes: Vec<usize> = BUSIEST_DAY
            .iter()
            .flat_map(|&(calls, seconds)| std::iter::repeat_n(calls, seconds))
            .collect();
        for i in (1..sizes.len()).rev() {
            sizes.swap(i, self.rng.below(i + 1));
        }
        sizes
            .into_iter()
            .map(|n| (0..n).map(|_| self.row()).collect())
            .collect()
    }
}

fn words(rng: &mut XorShift, vocab: &[String], len: usize) -> String {
    let mut text = String::with_capacity(len + 16);
    while text.len() < len {
        text.push_str(&vocab[rng.below(vocab.len())]);
        text.push(if rng.below(12) == 0 { '\n' } else { ' ' });
    }
    text.truncate(len);
    text
}

fn call_row(at_ms: Millis, request: String, response: String) -> LlmCallRow {
    LlmCallRow {
        call_id: CallId::new_v7(at_ms).expect("a 2026 timestamp"),
        tenant: TenantId::new("local"),
        session_id: Some(SessionId::new_v7(1_790_553_600_000).unwrap()),
        trace_id: TraceId::new("llm-agent"),
        model: "deepseek-v4-flash".into(),
        request_body: CallBody::Present(request),
        request_sha256: [7; 32],
        has_tools: false,
        has_thinking: false,
        has_structured_output: true,
        temperature: Some(0.0),
        max_output_tokens: Some(16_384),
        output_limit: OutputLimit::Configured { tokens: 16_384 },
        dispatch: CallDispatch::Sent {
            degraded: Vec::new(),
            provider_id: Some(ProviderId::new("deepseek")),
            end: SentEnd::Streamed {
                usage: Usage {
                    input_tokens: 27_855,
                    output_tokens: 412,
                    cached_input_tokens: 26_880,
                    cache_creation_tokens: 0,
                    thinking_tokens: 0,
                },
                end: StreamEnd::Completed(CallResponse {
                    body: CallBody::Present(response),
                    actual_model: "deepseek-v4-flash".into(),
                    stop_reason: Some(StopReason::EndTurn),
                }),
            },
        },
        retry_attempts: Vec::new(),
        provider_latency_ms: Some(1_628),
        pipeline_total_ms: Some(1_632),
        layers: ["event_emitter", "telemetry", "retry", "provider"]
            .map(String::from)
            .to_vec(),
        validation_summary: ValidationSummary::default(),
        tags: Vec::new(),
    }
}

/// Deterministic, dependency-free: the same seed builds the same workload.
struct XorShift(u64);

impl XorShift {
    fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.step() % n as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.step() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A size interpolated between the `(quantile, bytes)` points.
    fn size(&mut self, quantiles: &[(f64, f64)]) -> usize {
        let u = self.unit();
        let bytes = quantiles.windows(2).find(|w| u <= w[1].0).map_or(
            quantiles[quantiles.len() - 1].1,
            |w| {
                let ((q0, b0), (q1, b1)) = (w[0], w[1]);
                b0 + (b1 - b0) * (u - q0) / (q1 - q0)
            },
        );
        bytes as usize
    }

    fn word(&mut self) -> String {
        let len = 2 + self.below(9);
        (0..len)
            .map(|_| char::from(b'a' + self.below(26) as u8))
            .collect()
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 20)
}

/// `(median, max)` in ms.
fn spread(xs: impl IntoIterator<Item = Duration>) -> (f64, f64) {
    let mut xs: Vec<Duration> = xs.into_iter().collect();
    xs.sort();
    (ms(xs[xs.len() / 2]), ms(xs[xs.len() - 1]))
}

#[expect(
    clippy::print_stdout,
    reason = "the bench's output is this table, read with --nocapture; it asserts nothing"
)]
fn row(label: &str, xs: impl IntoIterator<Item = Duration>) {
    let (median, max) = spread(xs);
    println!("  {label:<34} median {median:>9.1} ms   max {max:>9.1} ms");
}

#[expect(
    clippy::print_stdout,
    reason = "the bench's output is this table, read with --nocapture; it asserts nothing"
)]
fn report_durations(title: &str, label: &str, xs: &[Duration]) {
    println!("\n== {title} (n = {})", xs.len());
    row(label, xs.iter().copied());
}

#[expect(
    clippy::print_stdout,
    reason = "the bench's output is this table, read with --nocapture; it asserts nothing"
)]
fn report_seals(title: &str, seals: &[SealTiming]) {
    println!("\n== {title} (n = {})", seals.len());
    row(
        "write .tmp + fsync (no lock)",
        seals.iter().map(|s| s.write),
    );
    row("publish (lock held)", seals.iter().map(|s| s.publish));
}

#[expect(
    clippy::print_stdout,
    reason = "the bench's output is this table, read with --nocapture; it asserts nothing"
)]
fn report_merge(title: &str, source: &Source, runs: &[MergeRun]) {
    let before = source.footprint();
    println!(
        "\n== {title} (runs = {}): {} live files, {:.1} MiB on disk → 1 file of {:.1} MiB; {} rows, {:.1} MiB of bodies",
        runs.len(),
        before.files,
        mib(before.bytes),
        mib(runs[0].output_bytes),
        source.rows,
        mib(source.raw_bytes),
    );
    row("maintain, whole pass", runs.iter().map(|r| r.plain.total));
    row(
        "  of it merging (no lock)",
        runs.iter().map(|r| r.plain.merging),
    );
    row("  of it lock held", runs.iter().map(|r| r.plain.locked()));
    row(
        "raced: maintain, whole pass",
        runs.iter().map(|r| r.raced.pass.total),
    );
    row(
        "raced: lock held",
        runs.iter().map(|r| r.raced.pass.locked()),
    );
    row(
        "raced: publish mid-merge",
        runs.iter().map(|r| r.raced.stalled_publish),
    );
    let merges: Vec<usize> = runs.iter().map(|r| r.raced.merges).collect();
    println!("  raced: merges in the pass          {merges:?}");
    println!("  replayed step by step:");
    let step = |label: &str, f: fn(&Steps) -> Duration| {
        row(label, runs.iter().map(|r| f(&r.steps)));
    };
    step("  plan (lock held)", |s| s.plan);
    step("  merge (no lock)", |s| s.merge);
    step("  land: take lock", |s| s.land_lock);
    step("  land: manifest read", |s| s.land_manifest_read);
    step("  land: rename", |s| s.land_rename);
    step("  land: manifest replace", |s| s.land_manifest_replace);
    step("  replan (lock held)", |s| s.replan);
    step("  tidy (lock held)", |s| s.tidy);
    step("  lock held, sum", Steps::locked);
    step("  sum", Steps::sum);
}
