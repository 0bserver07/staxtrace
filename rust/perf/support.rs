#![allow(
    dead_code,
    reason = "each integration-test crate exercises a different subset of the shared harness"
)]

//! Shared support for release-only performance integration tests.
//!
//! The checked-in workload policy is consumed before setup starts and again
//! before a receipt is published. The outer runner independently pins the
//! policy digest, bounds the process group, and validates the complete JSON
//! shape.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const RECEIPT_SCHEMA: &str = "staxtrace.performance-receipt";
pub const SCENARIO_TIMEOUT_SECONDS: u64 = 180;
pub const PEAK_RSS_LIMIT_BYTES: u64 = 1_073_741_824;
pub const OPEN_FILE_LIMIT: u64 = 256;
const OBSERVATION_INTERVAL_MILLISECONDS: u64 = 250;
const OBSERVER_WALL_SECONDS_LIMIT: f64 = 30.0;

#[derive(Debug, Clone, Copy)]
pub enum Guard {
    MaxP99Ms(f64),
    MinThroughput(f64),
    Concurrent { max_p99_ms: f64, errors: u64 },
}

impl Guard {
    fn policy_value(self) -> Value {
        match self {
            Self::MaxP99Ms(limit) => {
                json!({"metric": "p99_ms", "comparison": "at_most", "limit": limit})
            }
            Self::MinThroughput(limit) => json!({
                "metric": "throughput_per_second",
                "comparison": "at_least",
                "limit": limit,
            }),
            Self::Concurrent {
                max_p99_ms,
                errors: _,
            } => json!({
                "metric": "reader_p99_ms",
                "comparison": "at_most",
                "limit": max_p99_ms,
                "errors": 0,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub setup_wall: Duration,
    pub copy_wall: Duration,
    pub timed_wall: Duration,
    pub scenario_wall: Duration,
}

impl Timing {
    #[must_use]
    pub fn from_started(started: Instant, timed_wall: Duration, copy_wall: Duration) -> Self {
        let scenario_wall = started.elapsed();
        let accounted = timed_wall.saturating_add(copy_wall);
        assert!(
            scenario_wall >= accounted,
            "timed and copy durations exceed total scenario duration"
        );
        Self {
            setup_wall: scenario_wall - accounted,
            copy_wall,
            timed_wall,
            scenario_wall,
        }
    }

    #[must_use]
    pub fn explicit(setup_wall: Duration, copy_wall: Duration, timed_wall: Duration) -> Self {
        Self {
            setup_wall,
            copy_wall,
            timed_wall,
            scenario_wall: setup_wall
                .saturating_add(copy_wall)
                .saturating_add(timed_wall),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Resources {
    pub peak_rss_bytes: u64,
    pub peak_open_files: u64,
    pub child_peak_rss_bytes: u64,
    pub child_peak_open_files: u64,
    pub rss_sample_count: u64,
    pub open_file_sample_count: u64,
    pub child_process_sample_count: u64,
    pub observer_wall: Duration,
    continuous: bool,
}

impl Resources {
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.peak_rss_bytes = self.peak_rss_bytes.max(other.peak_rss_bytes);
        self.peak_open_files = self.peak_open_files.max(other.peak_open_files);
        self.child_peak_rss_bytes = self.child_peak_rss_bytes.max(other.child_peak_rss_bytes);
        self.child_peak_open_files = self.child_peak_open_files.max(other.child_peak_open_files);
        self.rss_sample_count = self.rss_sample_count.saturating_add(other.rss_sample_count);
        self.open_file_sample_count = self
            .open_file_sample_count
            .saturating_add(other.open_file_sample_count);
        self.child_process_sample_count = self
            .child_process_sample_count
            .saturating_add(other.child_process_sample_count);
        self.observer_wall = self.observer_wall.saturating_add(other.observer_wall);
        self.continuous |= other.continuous;
        self
    }

    #[must_use]
    pub fn measurement_mode(self) -> &'static str {
        if self.continuous {
            "continuous"
        } else {
            "edge"
        }
    }
}

/// Samples this test process at scenario edges or continuously. Continuous
/// mode records both RSS and descriptors throughout the workload. Command-
/// backed observer work is accumulated separately in the receipt.
pub struct ResourceMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Resources>>,
}

/// Samples a spawned process and its descendants only after stdin has been
/// delivered. Sampling runs concurrently with the child workload through its
/// exit, so observer command wall time is recorded without being subtracted
/// from the child latency distribution.
pub struct ChildProcessMonitor {
    begin: Arc<AtomicBool>,
    sampling_started: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Resources>>,
}

impl ChildProcessMonitor {
    #[must_use]
    pub fn start(root_pid: u32) -> Self {
        let begin = Arc::new(AtomicBool::new(false));
        let sampling_started = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_begin = Arc::clone(&begin);
        let worker_started = Arc::clone(&sampling_started);
        let worker_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !worker_begin.load(Ordering::Acquire) && !worker_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            let mut resources = Resources {
                continuous: true,
                ..Resources::default()
            };
            let mut observed_child = false;
            while !worker_stop.load(Ordering::Acquire) {
                worker_started.store(true, Ordering::Release);
                let sample = sample_process_tree(root_pid);
                observed_child |= sample.child_process_sample_count > 0;
                resources = resources.merge(sample);
                thread::yield_now();
            }
            // If stop races the first scheduled observation, take one final
            // post-input sample. A process that is already gone correctly
            // contributes no successful evidence.
            if !worker_started.load(Ordering::Acquire) {
                worker_started.store(true, Ordering::Release);
                let sample = sample_process_tree(root_pid);
                observed_child |= sample.child_process_sample_count > 0;
                resources = resources.merge(sample);
            }
            resources.child_process_sample_count = u64::from(observed_child);
            resources
        });
        Self {
            begin,
            sampling_started,
            stop,
            handle: Some(handle),
        }
    }

    /// Arm observation after the real payload has been written and stdin has
    /// been closed. Return only once the observer has begun its first sample.
    pub fn begin_processing(&self) {
        self.begin.store(true, Ordering::Release);
        while !self.sampling_started.load(Ordering::Acquire) {
            thread::yield_now();
        }
    }

    #[must_use]
    pub fn stop(mut self) -> Resources {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .expect("child resource monitor handle")
            .join()
            .expect("child resource monitor thread")
    }
}

impl Drop for ChildProcessMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl ResourceMonitor {
    #[must_use]
    pub fn start() -> Self {
        Self::start_inner(false)
    }

    #[must_use]
    pub fn start_continuous() -> Self {
        Self::start_inner(true)
    }

    fn start_inner(continuous: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let initial = sample_process(std::process::id());
        let handle = thread::spawn(move || {
            let mut resources = initial;
            resources.continuous = continuous;
            if continuous {
                while !worker_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(OBSERVATION_INTERVAL_MILLISECONDS));
                    if !worker_stop.load(Ordering::Relaxed) {
                        resources = resources.merge(sample_process(std::process::id()));
                    }
                }
            } else {
                while !worker_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            resources.merge(sample_process(std::process::id()))
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    #[must_use]
    pub fn stop(mut self) -> Resources {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .expect("resource monitor handle")
            .join()
            .expect("resource monitor thread")
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn process_tree_pids(root_pid: u32) -> Vec<u32> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output();
    let Some(text) = output
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
    else {
        return vec![root_pid];
    };
    let relationships: Vec<(u32, u32)> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    let mut tree = BTreeSet::from([root_pid]);
    loop {
        let before = tree.len();
        for (pid, parent) in &relationships {
            if tree.contains(parent) {
                tree.insert(*pid);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    tree.into_iter().collect()
}

fn sample_process_tree(root_pid: u32) -> Resources {
    let started = Instant::now();
    // Probe the known child immediately, before the slower process-tree walk,
    // so short-lived installed hooks still yield post-input evidence.
    let root = sample_process(root_pid);
    let mut peak_rss_bytes = root.peak_rss_bytes;
    let mut peak_open_files = root.peak_open_files;
    let mut rss_sample_count = root.rss_sample_count;
    let mut open_file_sample_count = root.open_file_sample_count;
    let mut complete_observation = root.rss_sample_count > 0 && root.open_file_sample_count > 0;
    for pid in process_tree_pids(root_pid)
        .into_iter()
        .filter(|pid| *pid != root_pid)
    {
        let sample = sample_process(pid);
        complete_observation |= sample.rss_sample_count > 0 && sample.open_file_sample_count > 0;
        peak_rss_bytes = peak_rss_bytes.saturating_add(sample.peak_rss_bytes);
        peak_open_files = peak_open_files.saturating_add(sample.peak_open_files);
        rss_sample_count = rss_sample_count.saturating_add(sample.rss_sample_count);
        open_file_sample_count =
            open_file_sample_count.saturating_add(sample.open_file_sample_count);
    }
    // Preserve the spawned workload tree independently. Generic peaks below
    // include the harness parent so the scenario ceiling covers all processes,
    // but these fields prove a fast child itself was actually observed.
    let child_peak_rss_bytes = peak_rss_bytes;
    let child_peak_open_files = peak_open_files;
    let parent = sample_process(std::process::id());
    peak_rss_bytes = peak_rss_bytes.saturating_add(parent.peak_rss_bytes);
    peak_open_files = peak_open_files.saturating_add(parent.peak_open_files);
    rss_sample_count = rss_sample_count.saturating_add(parent.rss_sample_count);
    open_file_sample_count = open_file_sample_count.saturating_add(parent.open_file_sample_count);
    Resources {
        peak_rss_bytes,
        peak_open_files,
        child_peak_rss_bytes,
        child_peak_open_files,
        rss_sample_count,
        open_file_sample_count,
        child_process_sample_count: u64::from(complete_observation),
        observer_wall: started.elapsed(),
        continuous: true,
    }
}

fn sample_process(pid: u32) -> Resources {
    let started = Instant::now();
    // RSS and descriptor probes are independent. Launch them together so a
    // fast child cannot exit in the gap between two sequential commands.
    let (peak_rss_bytes, peak_open_files) = thread::scope(|scope| {
        let rss = scope.spawn(|| sample_rss_bytes(pid));
        let open_files = sample_open_files(pid);
        (rss.join().expect("RSS observer thread"), open_files)
    });
    Resources {
        peak_rss_bytes: peak_rss_bytes.unwrap_or(0),
        peak_open_files: peak_open_files.unwrap_or(0),
        child_peak_rss_bytes: 0,
        child_peak_open_files: 0,
        rss_sample_count: u64::from(peak_rss_bytes.is_some()),
        open_file_sample_count: u64::from(peak_open_files.is_some()),
        child_process_sample_count: 0,
        observer_wall: started.elapsed(),
        continuous: false,
    }
}

fn sample_rss_bytes(pid: u32) -> Option<u64> {
    let pid = pid.to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output();
    output
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.split_whitespace().next()?.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.saturating_mul(1024))
}

fn sample_open_files(pid: u32) -> Option<u64> {
    let proc_path = PathBuf::from("/proc").join(pid.to_string()).join("fd");
    if let Ok(entries) = std::fs::read_dir(proc_path) {
        let count = entries.flatten().count() as u64;
        return (count > 0).then_some(count);
    }

    let pid = pid.to_string();
    std::process::Command::new("lsof")
        .args(["-n", "-P", "-p", &pid])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let count = String::from_utf8_lossy(&out.stdout)
                .lines()
                .count()
                .saturating_sub(1) as u64;
            (count > 0).then_some(count)
        })
}

#[must_use]
pub fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

#[must_use]
pub fn elapsed_ms_without(start: Instant, excluded: Duration) -> f64 {
    start.elapsed().saturating_sub(excluded).as_secs_f64() * 1_000.0
}

#[must_use]
pub fn sum_sample_time(samples_ms: &[f64]) -> Duration {
    Duration::from_secs_f64(samples_ms.iter().sum::<f64>() / 1_000.0)
}

fn policy_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../perf/workloads.json")
}

fn load_policy_from(path: &Path, scenario_dir: &Path) -> Value {
    let bytes = std::fs::read(path).expect("read checked-in performance policy");
    let mut policy: Value =
        serde_json::from_slice(&bytes).expect("parse checked-in performance policy");
    let mut fragments: Vec<PathBuf> = std::fs::read_dir(scenario_dir)
        .expect("read additive performance scenario directory")
        .map(|entry| entry.expect("read additive scenario entry").path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect();
    fragments.sort();
    for path in fragments {
        assert!(
            path.is_file() && !path.is_symlink(),
            "additive performance scenario must be a regular file"
        );
        let bytes = std::fs::read(&path).expect("read additive performance scenario");
        let mut fragment: Value =
            serde_json::from_slice(&bytes).expect("parse additive performance scenario");
        let object = fragment
            .as_object_mut()
            .expect("additive performance scenario object");
        let schema = object
            .remove("schema")
            .and_then(|value| value.as_str().map(str::to_owned));
        assert_eq!(
            schema.as_deref(),
            Some("staxtrace.performance-scenario"),
            "additive performance scenario schema"
        );
        let scenario = object
            .remove("scenario")
            .and_then(|value| value.as_str().map(str::to_owned))
            .expect("additive performance scenario identifier");
        let scenarios = policy["scenarios"]
            .as_object_mut()
            .expect("performance scenario policy map");
        assert!(
            !scenarios.contains_key(&scenario),
            "additive performance scenario collides with existing identifier"
        );
        scenarios.insert(scenario, Value::Object(std::mem::take(object)));
    }
    policy
}

fn policy() -> Value {
    let path = policy_path();
    let scenario_dir = path
        .parent()
        .expect("performance policy directory")
        .join("scenarios");
    load_policy_from(&path, &scenario_dir)
}

fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("serialize canonical JSON string"))
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut fields: Vec<_> = values.iter().collect();
            fields.sort_by_key(|(key, _)| (*key).clone());
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("serialize canonical JSON key"));
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
    }
}

fn canonical_policy_digest(policy: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_json(policy, &mut canonical);
    stax_etl::stats::sha256::hexdigest(canonical.as_bytes())
}

fn policy_digest_matches(supplied: &str, policy: &Value) -> bool {
    supplied == canonical_policy_digest(policy)
}

fn assert_policy_digest_matches(supplied: &str, policy: &Value) {
    assert_eq!(
        supplied,
        canonical_policy_digest(policy),
        "merged performance policy changed after runner verification"
    );
}

fn assert_runner_policy_digest(policy: &Value) {
    let supplied =
        std::env::var("STAX_PERF_POLICY_DIGEST").expect("verified policy digest is required");
    assert_policy_digest_matches(&supplied, policy);
}

pub fn require_release_harness() {
    if cfg!(debug_assertions) {
        panic!("performance evidence is release-only; run rust/perf-gate.sh");
    }
    assert_eq!(
        std::env::var("STAX_PERF_HARNESS").as_deref(),
        Ok("1"),
        "performance evidence must be run by rust/perf-gate.sh so timeouts and receipt validation are active"
    );
    assert!(
        std::env::var_os("STAX_PERF_RECEIPT_DIR").is_some(),
        "STAX_PERF_RECEIPT_DIR is required for durable evidence"
    );
    let run_id = std::env::var("STAX_PERF_RUN_ID").expect("STAX_PERF_RUN_ID is required");
    assert!(
        run_id.len() == 32 && run_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "STAX_PERF_RUN_ID must be a sanitized run identity"
    );
    let digest =
        std::env::var("STAX_PERF_POLICY_DIGEST").expect("verified policy digest is required");
    assert!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "STAX_PERF_POLICY_DIGEST must be a verified digest"
    );
}

/// Assert that the Rust scenario consumes the exact checked-in policy before
/// fixture setup or timing begins.
pub fn prepare_scenario(
    scenario: &str,
    fixture: &[(&str, u64)],
    policy_samples: Option<usize>,
    guard: Guard,
) {
    require_release_harness();
    let policy = policy();
    assert_runner_policy_digest(&policy);
    assert_eq!(policy["suite_timeout_seconds"], 900);
    assert_eq!(policy["scenario_timeout_seconds"], SCENARIO_TIMEOUT_SECONDS);
    assert_eq!(
        policy["resource_limits"]["peak_rss_bytes"],
        PEAK_RSS_LIMIT_BYTES
    );
    assert_eq!(policy["resource_limits"]["open_files"], OPEN_FILE_LIMIT);
    assert_eq!(
        policy["resource_observation"]["interval_milliseconds"],
        OBSERVATION_INTERVAL_MILLISECONDS
    );
    assert_eq!(
        policy["resource_observation"]["observer_wall_seconds_limit"],
        OBSERVER_WALL_SECONDS_LIMIT
    );
    assert_eq!(policy["baseline_policy"]["clean_run_count"], 3);
    assert_eq!(
        policy["baseline_policy"]["latency_regression_fraction"],
        0.20
    );
    assert_eq!(policy["baseline_policy"]["rss_regression_fraction"], 0.20);
    assert_eq!(
        policy["baseline_policy"]["open_file_regression_fraction"],
        0.20
    );
    assert_eq!(
        policy["baseline_policy"]["throughput_regression_fraction"],
        0.15
    );
    let expected = &policy["scenarios"][scenario];
    assert!(
        expected.is_object(),
        "scenario missing from checked-in policy"
    );
    let fixture_map: BTreeMap<&str, u64> = fixture.iter().copied().collect();
    assert_eq!(
        expected["fixture"],
        json!(fixture_map),
        "fixed performance fixture changed"
    );
    assert_eq!(
        expected["samples"],
        policy_samples.map_or(Value::Null, |count| json!(count)),
        "fixed performance sample count changed"
    );
    assert_eq!(
        expected["guard"],
        guard.policy_value(),
        "fixed performance guard changed"
    );
}

pub fn assert_exact(label: &str, actual: u64, expected: u64) {
    assert_eq!(
        actual, expected,
        "fixed performance fixture cardinality changed: {label}"
    );
}

fn runner() -> Value {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let os = required("STAX_PERF_RUNNER_OS");
    let architecture = required("STAX_PERF_RUNNER_ARCH");
    let runner_class = required("STAX_PERF_RUNNER_CLASS");
    let toolchain = required("STAX_PERF_RUNNER_TOOLCHAIN");
    let runner_key = required("STAX_PERF_RUNNER_KEY");
    assert!(
        matches!(os.as_str(), "freebsd" | "linux" | "macos" | "windows"),
        "cached runner OS must be canonical"
    );
    assert!(
        matches!(
            architecture.as_str(),
            "aarch64" | "armv7" | "riscv64" | "x86" | "x86_64"
        ),
        "cached runner architecture must be canonical"
    );
    assert!(
        matches!(runner_class.as_str(), "local" | "dedicated" | "hosted"),
        "cached runner class must be checked in"
    );
    assert!(
        toolchain.starts_with("rustc ")
            && toolchain.is_ascii()
            && !toolchain.contains(['\n', '\r', '/', '\\']),
        "cached toolchain identity must be sanitized"
    );
    assert_eq!(
        runner_key,
        format!("{os}-{architecture}-{runner_class}-{toolchain}"),
        "cached runner key must derive from its canonical fields"
    );
    json!({
        "os": os,
        "architecture": architecture,
        "runner_class": runner_class,
        "toolchain": toolchain,
        "key": runner_key,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn write_receipt(
    scenario: &str,
    fixture: &[(&str, u64)],
    policy_samples: Option<usize>,
    samples_ms: &[f64],
    timing: Timing,
    work_units: Option<u64>,
    resources: Resources,
    guard: Guard,
) -> PathBuf {
    prepare_scenario(scenario, fixture, policy_samples, guard);
    assert!(!samples_ms.is_empty(), "a receipt needs timed samples");
    assert!(
        samples_ms
            .iter()
            .all(|sample| sample.is_finite() && *sample >= 0.0),
        "latency samples must be finite and non-negative"
    );

    let expected = policy()["scenarios"][scenario].clone();
    let expected_resources = &expected["resources"];
    let expected_mode = expected_resources["mode"]
        .as_str()
        .expect("resource mode in policy");
    let minimum_rss_samples = expected_resources["minimum_rss_samples"]
        .as_u64()
        .expect("minimum RSS samples in policy");
    let minimum_open_file_samples = expected_resources["minimum_open_file_samples"]
        .as_u64()
        .expect("minimum descriptor samples in policy");
    let expected_child_samples = expected_resources["child_process_samples"]
        .as_u64()
        .expect("child sample count in policy");

    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p50 = percentile(&sorted, 50);
    let p95 = percentile(&sorted, 95);
    let p99 = percentile(&sorted, 99);
    let throughput = throughput_for_guard(guard, work_units, timing.timed_wall);

    let (metric, comparison, limit, actual, errors, operation_errors_pass) = match guard {
        Guard::MaxP99Ms(limit) => ("p99_ms", "at_most", limit, p99, None, true),
        Guard::MinThroughput(limit) => (
            "throughput_per_second",
            "at_least",
            limit,
            throughput.expect("throughput guard needs work units"),
            None,
            true,
        ),
        Guard::Concurrent { max_p99_ms, errors } => (
            "reader_p99_ms",
            "at_most",
            max_p99_ms,
            p99,
            Some(errors),
            errors == 0,
        ),
    };
    let comparison_pass = match comparison {
        "at_most" => actual <= limit,
        "at_least" => actual >= limit,
        _ => unreachable!(),
    };
    let guard_passed = comparison_pass && operation_errors_pass;
    let resource_passed = resources.peak_rss_bytes > 0
        && resources.peak_open_files > 0
        && resources.peak_rss_bytes <= PEAK_RSS_LIMIT_BYTES
        && resources.peak_open_files <= OPEN_FILE_LIMIT
        && resources.rss_sample_count >= minimum_rss_samples
        && resources.open_file_sample_count >= minimum_open_file_samples
        && resources.child_process_sample_count == expected_child_samples
        && (expected_child_samples == 0
            || (resources.child_peak_rss_bytes > 0
                && resources.child_peak_open_files > 0
                && resources.child_peak_rss_bytes <= PEAK_RSS_LIMIT_BYTES
                && resources.child_peak_open_files <= OPEN_FILE_LIMIT))
        && resources.measurement_mode() == expected_mode
        && resources.observer_wall.as_secs_f64() <= OBSERVER_WALL_SECONDS_LIMIT;
    let timing_passed = timing.timed_wall > Duration::ZERO
        && timing.scenario_wall <= Duration::from_secs(SCENARIO_TIMEOUT_SECONDS)
        && timing
            .setup_wall
            .saturating_add(timing.copy_wall)
            .saturating_add(timing.timed_wall)
            <= timing.scenario_wall;
    let passed = guard_passed && resource_passed && timing_passed;

    let fixture: BTreeMap<&str, u64> = fixture.iter().copied().collect();
    let observer_fraction = resources.observer_wall.as_secs_f64()
        / timing.scenario_wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let receipt = json!({
        "schema": RECEIPT_SCHEMA,
        "run_id": std::env::var("STAX_PERF_RUN_ID").expect("run id required"),
        "policy_digest": std::env::var("STAX_PERF_POLICY_DIGEST").expect("policy digest required"),
        "scenario": scenario,
        "profile": "release",
        "runner": runner(),
        "fixture": fixture,
        "samples": {
            "count": samples_ms.len(),
            "p50_ms": p50,
            "p95_ms": p95,
            "p99_ms": p99,
        },
        "throughput_per_second": throughput,
        "setup_wall_seconds": timing.setup_wall.as_secs_f64(),
        "copy_wall_seconds": timing.copy_wall.as_secs_f64(),
        "timed_wall_seconds": timing.timed_wall.as_secs_f64(),
        "scenario_wall_seconds": timing.scenario_wall.as_secs_f64(),
        "timeout_seconds": SCENARIO_TIMEOUT_SECONDS,
        "resources": {
            "measurement_mode": resources.measurement_mode(),
            "peak_rss_bytes": resources.peak_rss_bytes,
            "peak_open_files": resources.peak_open_files,
            "child_peak_rss_bytes": resources.child_peak_rss_bytes,
            "child_peak_open_files": resources.child_peak_open_files,
            "rss_sample_count": resources.rss_sample_count,
            "open_file_sample_count": resources.open_file_sample_count,
            "child_process_sample_count": resources.child_process_sample_count,
            "observer_wall_seconds": resources.observer_wall.as_secs_f64(),
            "observer_wall_fraction": observer_fraction,
            "observer_wall_seconds_limit": OBSERVER_WALL_SECONDS_LIMIT,
            "peak_rss_limit_bytes": PEAK_RSS_LIMIT_BYTES,
            "open_file_limit": OPEN_FILE_LIMIT,
        },
        "guard": {
            "metric": metric,
            "comparison": comparison,
            "limit": limit,
            "actual": actual,
            "errors": errors,
            "passed": guard_passed,
        },
        "status": if passed { "pass" } else { "fail" },
    });

    let dir =
        PathBuf::from(std::env::var_os("STAX_PERF_RECEIPT_DIR").expect("receipt dir required"));
    std::fs::create_dir_all(&dir).expect("create receipt directory");
    let path = dir.join(format!("{scenario}.json"));
    let temporary = dir.join(format!(".{scenario}-{}.tmp", std::process::id()));
    let mut encoded = serde_json::to_vec_pretty(&receipt).expect("encode receipt");
    encoded.push(b'\n');
    std::fs::write(&temporary, encoded).expect("write receipt");
    std::fs::rename(&temporary, &path).expect("publish receipt atomically");
    println!("performance receipt: {scenario}.json");
    assert!(
        passed,
        "{scenario} persisted a sanitized failing performance receipt"
    );
    path
}

fn throughput_for_guard(
    guard: Guard,
    work_units: Option<u64>,
    timed_wall: Duration,
) -> Option<f64> {
    match guard {
        Guard::MinThroughput(_) => {
            let units = work_units.expect("throughput guard needs work units");
            let timed_seconds = timed_wall.as_secs_f64();
            assert!(timed_seconds > 0.0, "timed wall must be positive");
            let throughput = units as f64 / timed_seconds;
            assert!(
                throughput.is_finite() && throughput >= 0.0,
                "throughput must be finite and non-negative"
            );
            Some(throughput)
        }
        Guard::MaxP99Ms(_) | Guard::Concurrent { .. } => {
            assert!(
                work_units.is_none(),
                "latency and concurrency guards must not publish throughput"
            );
            None
        }
    }
}

fn percentile(sorted: &[f64], percent: usize) -> f64 {
    debug_assert!(!sorted.is_empty());
    let rank = (percent * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles_are_distributional() {
        let values: Vec<f64> = (1..=100).map(|value| value as f64).collect();
        assert_eq!(percentile(&values, 50), 50.0);
        assert_eq!(percentile(&values, 95), 95.0);
        assert_eq!(percentile(&values, 99), 99.0);
    }

    #[test]
    fn receipt_metric_shape_matches_the_primary_guard() {
        assert_eq!(
            throughput_for_guard(Guard::MinThroughput(1.0), Some(100), Duration::from_secs(2),),
            Some(50.0)
        );
        assert_eq!(
            throughput_for_guard(Guard::MaxP99Ms(1.0), None, Duration::from_secs(1)),
            None
        );
        assert_eq!(
            throughput_for_guard(
                Guard::Concurrent {
                    max_p99_ms: 1.0,
                    errors: 0,
                },
                None,
                Duration::from_secs(1),
            ),
            None
        );
    }

    #[test]
    fn receipt_metric_shape_rejects_missing_and_surplus_work_units() {
        assert!(
            std::panic::catch_unwind(|| throughput_for_guard(
                Guard::MinThroughput(1.0),
                None,
                Duration::from_secs(1),
            ))
            .is_err(),
            "throughput guards must reject missing work units"
        );
        assert!(
            std::panic::catch_unwind(|| throughput_for_guard(
                Guard::MaxP99Ms(1.0),
                Some(1),
                Duration::from_secs(1),
            ))
            .is_err(),
            "latency guards must reject surplus work units"
        );
        assert!(
            std::panic::catch_unwind(|| throughput_for_guard(
                Guard::Concurrent {
                    max_p99_ms: 1.0,
                    errors: 0,
                },
                Some(1),
                Duration::from_secs(1),
            ))
            .is_err(),
            "concurrency guards must reject surplus work units"
        );
    }

    #[test]
    fn failed_process_observations_do_not_increment_evidence_counts() {
        let resources = sample_process(u32::MAX);
        assert_eq!(resources.peak_rss_bytes, 0);
        assert_eq!(resources.peak_open_files, 0);
        assert_eq!(resources.rss_sample_count, 0);
        assert_eq!(resources.open_file_sample_count, 0);
        assert_eq!(resources.child_process_sample_count, 0);
    }

    #[test]
    fn rust_and_runner_use_the_same_canonical_merged_policy_digest() {
        let script = policy_path()
            .parent()
            .expect("performance directory")
            .join("run.py");
        let output = std::process::Command::new("python3")
            .args([
                script.as_os_str(),
                "policy-value".as_ref(),
                "policy_digest".as_ref(),
            ])
            .output()
            .expect("run policy self-check");
        assert!(output.status.success());
        assert_eq!(
            canonical_policy_digest(&policy()),
            String::from_utf8(output.stdout)
                .expect("policy digest UTF-8")
                .trim()
        );
    }

    #[test]
    fn a_mid_run_fragment_change_changes_the_rust_policy_digest() {
        let root = std::env::temp_dir().join(format!(
            "stax-performance-policy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let fragments = root.join("scenarios");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&fragments).expect("create policy fixture");
        let core_path = root.join("workloads.json");
        std::fs::write(
            &core_path,
            serde_json::to_vec(&json!({"scenarios": {}})).expect("encode core policy fixture"),
        )
        .expect("write core policy fixture");
        let before = canonical_policy_digest(&load_policy_from(&core_path, &fragments));
        std::fs::write(
            fragments.join("10-added.json"),
            serde_json::to_vec(&json!({
                "schema": "staxtrace.performance-scenario",
                "scenario": "added_fixture",
                "fixture": {"rows": 1},
            }))
            .expect("encode additive policy fixture"),
        )
        .expect("write additive policy fixture");
        let changed = load_policy_from(&core_path, &fragments);
        let after = canonical_policy_digest(&changed);
        assert_ne!(
            before, after,
            "fragment changes must invalidate the runner digest"
        );
        assert!(
            !policy_digest_matches(&before, &changed),
            "a mid-run fragment change must reject the cached runner digest"
        );
        std::fs::remove_dir_all(&root).expect("remove policy fixture");
    }

    #[test]
    fn a_runner_policy_digest_mismatch_fails() {
        let mismatched = "0".repeat(64);
        assert!(!policy_digest_matches(&mismatched, &policy()));
    }
}
