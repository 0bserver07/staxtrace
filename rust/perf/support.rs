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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const RECEIPT_SCHEMA: &str = "staxtrace.performance-receipt";
pub const SCENARIO_TIMEOUT_SECONDS: u64 = 180;
pub const PEAK_RSS_LIMIT_BYTES: u64 = 1_073_741_824;
pub const OPEN_FILE_LIMIT: u64 = 256;
const OBSERVATION_INTERVAL_MILLISECONDS: u64 = 250;
const OBSERVER_WALL_SECONDS_LIMIT: f64 = 30.0;
const MAX_PROCESS_TREE_PIDS: usize = 4_096;
const MAX_PROCESS_SAMPLE_ENTRIES: usize = 1_024;
const MAX_PID_DECIMAL_BYTES: usize = 10;

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
/// mode records both RSS and descriptors throughout the workload. Observer
/// work is accumulated separately in the receipt.
pub struct ResourceMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Resources>>,
}

/// Samples a spawned process and its descendants only after stdin has been
/// delivered. Sampling runs concurrently with the child workload through its
/// exit, so observer wall time is recorded without being subtracted from the
/// child latency distribution.
pub struct ChildProcessMonitor {
    control: Arc<(Mutex<ChildMonitorControl>, Condvar)>,
    ready_wall: Duration,
    handle: Option<JoinHandle<Resources>>,
}

#[derive(Debug, Default)]
struct ChildMonitorControl {
    worker_ready: bool,
    armed: bool,
    first_sample_complete: bool,
    stop: bool,
    worker_done: bool,
    #[cfg(test)]
    first_sample_aggregate: Option<Resources>,
}

impl ChildProcessMonitor {
    #[must_use]
    pub fn start(root_pid: u32) -> Self {
        Self::start_with_sampler(root_pid, sample_process_tree)
    }

    fn start_with_sampler(
        root_pid: u32,
        sampler: impl Fn(u32) -> Resources + Send + 'static,
    ) -> Self {
        let ready_started = Instant::now();
        let control = Arc::new((Mutex::new(ChildMonitorControl::default()), Condvar::new()));
        let worker_control = Arc::clone(&control);
        let handle = thread::spawn(move || {
            // A failed sampler must wake begin_processing and fail closed. In
            // particular, evidence from an earlier iteration is discarded if
            // a later observer iteration panics.
            let sampled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (lock, wake) = &*worker_control;
                let mut state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.worker_ready = true;
                wake.notify_all();
                while !state.armed && !state.stop {
                    state = wake
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if state.stop {
                    return Resources {
                        continuous: true,
                        ..Resources::default()
                    };
                }
                drop(state);

                let mut resources = Resources {
                    continuous: true,
                    ..Resources::default()
                };
                let mut observed_child = false;
                loop {
                    let sample = sampler(root_pid);
                    observed_child |= sample.child_process_sample_count > 0;
                    resources = resources.merge(sample);

                    let mut state = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Publish the barrier only after the complete attempt has
                    // been merged, so the parent cannot reap a fast process
                    // while its descriptor snapshot is still in flight.
                    #[cfg(test)]
                    if state.first_sample_aggregate.is_none() {
                        state.first_sample_aggregate = Some(resources);
                    }
                    state.first_sample_complete = true;
                    wake.notify_all();
                    if state.stop {
                        break;
                    }
                    drop(state);
                    thread::yield_now();
                }
                resources.child_process_sample_count = u64::from(observed_child);
                resources
            }));

            let resources = sampled.unwrap_or_else(|_| Resources {
                continuous: true,
                ..Resources::default()
            });
            let (lock, wake) = &*worker_control;
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.worker_done = true;
            wake.notify_all();
            drop(state);
            resources
        });
        let mut monitor = Self {
            control,
            ready_wall: Duration::ZERO,
            handle: Some(handle),
        };
        {
            let (lock, wake) = &*monitor.control;
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.worker_ready && !state.worker_done {
                state = wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        monitor.ready_wall = ready_started.elapsed();
        monitor
    }

    /// Arm observation after the real payload has been written and stdin has
    /// been closed. Return only once the observer has merged its first complete
    /// sample attempt, or has failed closed.
    pub fn begin_processing(&self) {
        let (lock, wake) = &*self.control;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.armed = true;
        wake.notify_all();
        while !state.first_sample_complete && !state.worker_done {
            state = wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn request_stop(&self) {
        let (lock, wake) = &*self.control;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stop = true;
        wake.notify_all();
    }

    #[cfg(test)]
    fn first_sample_aggregate(&self) -> Option<Resources> {
        self.control
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_sample_aggregate
    }

    #[must_use]
    pub fn stop(mut self) -> Resources {
        self.request_stop();
        let mut resources = self
            .handle
            .take()
            .expect("child resource monitor handle")
            .join()
            .expect("child resource monitor thread");
        resources.observer_wall = resources.observer_wall.saturating_add(self.ready_wall);
        resources
    }
}

impl Drop for ChildProcessMonitor {
    fn drop(&mut self) {
        self.request_stop();
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

#[derive(Debug, PartialEq, Eq)]
struct ProcessTreeDiscovery {
    pids: Vec<u32>,
    complete: bool,
}

fn process_tree_pids(root_pid: u32) -> ProcessTreeDiscovery {
    discover_process_tree(root_pid, direct_child_pids)
}

fn discover_process_tree(
    root_pid: u32,
    mut child_lookup: impl FnMut(u32) -> Option<Vec<u32>>,
) -> ProcessTreeDiscovery {
    let mut discovered = vec![root_pid];
    let mut seen = BTreeSet::from([root_pid]);
    let mut pending = VecDeque::from([root_pid]);
    while let Some(parent_pid) = pending.pop_front() {
        let Some(children) = child_lookup(parent_pid) else {
            // A disappearing or inaccessible process contributes no invented
            // descendants. Already discovered PIDs remain independently
            // sampleable and must still produce complete per-process fields.
            return ProcessTreeDiscovery {
                pids: discovered,
                complete: false,
            };
        };
        for child_pid in children {
            if child_pid == 0 || !seen.insert(child_pid) {
                continue;
            }
            if discovered.len() == MAX_PROCESS_TREE_PIDS {
                return ProcessTreeDiscovery {
                    pids: discovered,
                    complete: false,
                };
            }
            discovered.push(child_pid);
            pending.push_back(child_pid);
        }
    }
    ProcessTreeDiscovery {
        pids: discovered,
        complete: true,
    }
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
    let tree = process_tree_pids(root_pid);
    let mut complete_observation =
        tree.complete && root.rss_sample_count > 0 && root.open_file_sample_count > 0;
    for pid in tree.pids.into_iter().filter(|pid| *pid != root_pid) {
        let sample = sample_process(pid);
        complete_observation &= sample.rss_sample_count > 0 && sample.open_file_sample_count > 0;
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
    let (peak_rss_bytes, peak_open_files) = sample_process_values(pid);
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

#[cfg(target_os = "macos")]
fn sample_process_values(pid: u32) -> (Option<u64>, Option<u64>) {
    // Descriptor state disappears at exit, while proc_pid_rusage supports a
    // zombie. Preserve this call order for short-lived workloads.
    let open_files = macos_process::open_file_count(pid);
    let rss_bytes = macos_process::resident_size(pid);
    (rss_bytes, open_files)
}

#[cfg(target_os = "macos")]
fn direct_child_pids(pid: u32) -> Option<Vec<u32>> {
    macos_process::child_pids(pid)
}

#[cfg(target_os = "linux")]
fn sample_process_values(pid: u32) -> (Option<u64>, Option<u64>) {
    let root = PathBuf::from("/proc").join(pid.to_string());
    // Match the macOS descriptor-first ordering for the state most likely to
    // disappear at process exit.
    let open_files = linux_open_file_count(&root.join("fd"));
    let rss_bytes = std::fs::read_to_string(root.join("status"))
        .ok()
        .and_then(|status| parse_linux_rss_bytes(&status));
    (rss_bytes, open_files)
}

#[cfg(target_os = "linux")]
fn direct_child_pids(pid: u32) -> Option<Vec<u32>> {
    let path = PathBuf::from("/proc")
        .join(pid.to_string())
        .join("task")
        .join(pid.to_string())
        .join("children");
    let byte_cap = linux_child_list_byte_cap()?;
    let contents = read_bounded_utf8(std::fs::File::open(path).ok()?, byte_cap)?;
    parse_linux_child_pids(&contents)
}

#[cfg(any(test, target_os = "linux"))]
fn linux_child_list_byte_cap() -> Option<usize> {
    MAX_PROCESS_TREE_PIDS.checked_mul(MAX_PID_DECIMAL_BYTES.checked_add(1)?)
}

#[cfg(any(test, target_os = "linux"))]
fn read_bounded_utf8(reader: impl std::io::Read, byte_cap: usize) -> Option<String> {
    use std::io::Read as _;

    let read_cap = byte_cap.checked_add(1)?;
    let mut bytes = Vec::with_capacity(read_cap);
    reader
        .take(u64::try_from(read_cap).ok()?)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > byte_cap {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_rss_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    (fields.next()? == "VmRSS:").then_some(())?;
    let kibibytes = parse_ascii_decimal_u64(fields.next()?)?;
    (fields.next()? == "kB" && fields.next().is_none()).then_some(())?;
    kibibytes.checked_mul(1_024).filter(|bytes| *bytes > 0)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_child_pids(contents: &str) -> Option<Vec<u32>> {
    let mut pids = Vec::new();
    for field in contents.split_whitespace() {
        if pids.len() == MAX_PROCESS_TREE_PIDS {
            return None;
        }
        let pid = parse_ascii_decimal_u32(field)?;
        if pid == 0 {
            return None;
        }
        pids.push(pid);
    }
    Some(pids)
}

#[cfg(target_os = "linux")]
fn linux_open_file_count(path: &Path) -> Option<u64> {
    let mut count = 0_u64;
    for entry in std::fs::read_dir(path).ok()? {
        entry.ok()?;
        count = count.checked_add(1)?;
        if usize::try_from(count).ok()? >= MAX_PROCESS_SAMPLE_ENTRIES {
            return None;
        }
    }
    (count > 0).then_some(count)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample_process_values(pid: u32) -> (Option<u64>, Option<u64>) {
    let pid_text = pid.to_string();
    let rss = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid_text])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_fallback_rss_bytes(&out.stdout));
    let open_files = std::process::Command::new("lsof")
        .args(["-n", "-P", "-a", "-p", &pid_text, "-Fpf"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_fallback_open_file_count(&out.stdout, pid));
    (rss, open_files)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn direct_child_pids(parent_pid: u32) -> Option<Vec<u32>> {
    std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_fallback_process_relationships(&out.stdout))
        .and_then(|relationships| {
            relationships
                .iter()
                .any(|(pid, _)| *pid == parent_pid)
                .then(|| {
                    relationships
                        .into_iter()
                        .filter_map(|(pid, parent)| (parent == parent_pid).then_some(pid))
                        .collect()
                })
        })
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "linux"))))]
fn parse_fallback_rss_bytes(output: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(output).ok()?;
    let mut fields = text.split_whitespace();
    let kibibytes = parse_ascii_decimal_u64(fields.next()?)?;
    fields.next().is_none().then_some(())?;
    kibibytes.checked_mul(1_024).filter(|bytes| *bytes > 0)
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "linux"))))]
fn parse_fallback_open_file_count(output: &[u8], expected_pid: u32) -> Option<u64> {
    let text = std::str::from_utf8(output).ok()?;
    let mut lines = text.lines();
    let process = parse_ascii_decimal_u32(lines.next()?.strip_prefix('p')?)?;
    if process == 0 || process != expected_pid {
        return None;
    }
    let mut count = 0_u64;
    for line in lines {
        let descriptor = line.strip_prefix('f')?;
        if descriptor.is_empty() || !descriptor.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return None;
        }
        count = count.checked_add(1)?;
        if usize::try_from(count).ok()? >= MAX_PROCESS_SAMPLE_ENTRIES {
            return None;
        }
    }
    (count > 0).then_some(count)
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "linux"))))]
fn parse_fallback_process_relationships(output: &[u8]) -> Option<Vec<(u32, u32)>> {
    let text = std::str::from_utf8(output).ok()?;
    let mut relationships = Vec::new();
    for line in text.lines() {
        if relationships.len() == MAX_PROCESS_TREE_PIDS {
            return None;
        }
        let mut fields = line.split_whitespace();
        let pid = parse_ascii_decimal_u32(fields.next()?)?;
        let parent = parse_ascii_decimal_u32(fields.next()?)?;
        if pid == 0 || fields.next().is_some() {
            return None;
        }
        relationships.push((pid, parent));
    }
    (!relationships.is_empty()).then_some(relationships)
}

fn parse_ascii_decimal_u32(field: &str) -> Option<u32> {
    (!field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| field.parse::<u32>().ok())
        .flatten()
}

fn parse_ascii_decimal_u64(field: &str) -> Option<u64> {
    (!field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| field.parse::<u64>().ok())
        .flatten()
}

#[cfg(target_os = "macos")]
mod macos_process {
    use super::MAX_PROCESS_SAMPLE_ENTRIES;
    use std::ffi::{c_int, c_void};
    use std::mem::{MaybeUninit, align_of, offset_of, size_of};

    const PROC_PIDLISTFDS: c_int = 1;
    const RUSAGE_INFO_V0: c_int = 0;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFdInfo {
        fd: i32,
        fd_type: u32,
    }

    #[repr(C)]
    struct RusageInfoV0 {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        package_idle_wakeups: u64,
        interrupt_wakeups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        physical_footprint: u64,
        process_start_absolute_time: u64,
        process_exit_absolute_time: u64,
    }

    const _: () = assert!(size_of::<ProcFdInfo>() == 8);
    const _: () = assert!(align_of::<ProcFdInfo>() == 4);
    const _: () = assert!(offset_of!(ProcFdInfo, fd) == 0);
    const _: () = assert!(offset_of!(ProcFdInfo, fd_type) == 4);
    const _: () = assert!(size_of::<RusageInfoV0>() == 96);
    const _: () = assert!(align_of::<RusageInfoV0>() == 8);
    const _: () = assert!(offset_of!(RusageInfoV0, resident_size) == 64);

    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffer_size: c_int,
        ) -> c_int;
        fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        fn proc_listchildpids(parent_pid: c_int, buffer: *mut c_void, buffer_size: c_int) -> c_int;
        fn __error() -> *mut c_int;
    }

    pub(super) fn open_file_count(pid: u32) -> Option<u64> {
        let pid = c_int::try_from(pid).ok()?;
        let mut entries = [MaybeUninit::<ProcFdInfo>::uninit(); MAX_PROCESS_SAMPLE_ENTRIES];
        let buffer_bytes = size_of_val(&entries);
        let buffer_size = c_int::try_from(buffer_bytes).ok()?;
        // SAFETY: entries is a live writable buffer of buffer_size bytes. The
        // flavor's element layout and alignment are asserted above. The result
        // is validated before deriving a count from the initialized prefix.
        let returned = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        let count = validated_byte_count(returned, buffer_bytes, size_of::<ProcFdInfo>())?;
        u64::try_from(count).ok().filter(|count| *count > 0)
    }

    pub(super) fn resident_size(pid: u32) -> Option<u64> {
        let pid = c_int::try_from(pid).ok()?;
        let mut info = MaybeUninit::<RusageInfoV0>::uninit();
        // SAFETY: info points to writable storage with the SDK-pinned layout.
        // A zero return is the API's complete-success contract; only then is
        // the initialized structure read.
        let returned =
            unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V0, info.as_mut_ptr().cast::<c_void>()) };
        if returned != 0 {
            return None;
        }
        // SAFETY: proc_pid_rusage returned its documented success code above.
        let info = unsafe { info.assume_init() };
        (info.resident_size > 0).then_some(info.resident_size)
    }

    pub(super) fn child_pids(pid: u32) -> Option<Vec<u32>> {
        let pid = c_int::try_from(pid).ok()?;
        let mut children = [MaybeUninit::<i32>::uninit(); MAX_PROCESS_SAMPLE_ENTRIES];
        let buffer_bytes = size_of_val(&children);
        let buffer_size = c_int::try_from(buffer_bytes).ok()?;
        // SAFETY: children is initialized writable storage of buffer_size
        // bytes. The validated return is an element count, not a byte count.
        clear_errno()?;
        let returned =
            unsafe { proc_listchildpids(pid, children.as_mut_ptr().cast(), buffer_size) };
        let errno = if returned == 0 { current_errno()? } else { 0 };
        let count = validated_child_count(returned, errno, children.len())?;
        (0..count)
            .map(|index| {
                // SAFETY: the API returned count initialized i32 elements,
                // and count was validated against the backing capacity.
                let child = unsafe { children[index].assume_init() };
                u32::try_from(child).ok().filter(|pid| *pid > 0)
            })
            .collect()
    }

    fn clear_errno() -> Option<()> {
        // SAFETY: Darwin's __error returns thread-local writable errno storage.
        let errno = unsafe { __error() };
        if errno.is_null() {
            return None;
        }
        // SAFETY: the pointer was checked and is owned by the calling thread.
        unsafe { errno.write(0) };
        Some(())
    }

    fn current_errno() -> Option<c_int> {
        // SAFETY: Darwin's __error returns thread-local errno storage.
        let errno = unsafe { __error() };
        (!errno.is_null()).then(|| {
            // SAFETY: the pointer was checked and is owned by this thread.
            unsafe { errno.read() }
        })
    }

    fn validated_byte_count(
        returned: c_int,
        capacity: usize,
        element_size: usize,
    ) -> Option<usize> {
        let returned = usize::try_from(returned).ok()?;
        (returned > 0
            && returned < capacity
            && element_size > 0
            && returned.is_multiple_of(element_size))
        .then_some(returned / element_size)
    }

    fn validated_element_count(returned: c_int, capacity: usize) -> Option<usize> {
        let returned = usize::try_from(returned).ok()?;
        (returned < capacity).then_some(returned)
    }

    fn validated_child_count(returned: c_int, errno: c_int, capacity: usize) -> Option<usize> {
        if returned == 0 && errno != 0 {
            return None;
        }
        validated_element_count(returned, capacity)
    }

    #[cfg(test)]
    pub(super) fn assert_sdk_layout_and_return_validation() {
        assert_eq!(size_of::<ProcFdInfo>(), 8);
        assert_eq!(align_of::<ProcFdInfo>(), 4);
        assert_eq!(offset_of!(ProcFdInfo, fd_type), 4);
        assert_eq!(size_of::<RusageInfoV0>(), 96);
        assert_eq!(align_of::<RusageInfoV0>(), 8);
        assert_eq!(offset_of!(RusageInfoV0, resident_size), 64);
        assert_eq!(validated_byte_count(24, 64, 8), Some(3));
        assert_eq!(validated_byte_count(-1, 64, 8), None);
        assert_eq!(validated_byte_count(0, 64, 8), None);
        assert_eq!(validated_byte_count(23, 64, 8), None);
        assert_eq!(validated_byte_count(64, 64, 8), None);
        assert_eq!(validated_byte_count(72, 64, 8), None);
        assert_eq!(validated_element_count(3, 8), Some(3));
        assert_eq!(validated_element_count(-1, 8), None);
        assert_eq!(validated_element_count(8, 8), None);
        assert_eq!(validated_child_count(0, 0, 8), Some(0));
        assert_eq!(validated_child_count(0, 3, 8), None);
        assert_eq!(validated_child_count(-1, 3, 8), None);
    }
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

    fn complete_child_sample() -> Resources {
        Resources {
            peak_rss_bytes: 4_096,
            peak_open_files: 3,
            child_peak_rss_bytes: 4_096,
            child_peak_open_files: 3,
            rss_sample_count: 1,
            open_file_sample_count: 1,
            child_process_sample_count: 1,
            observer_wall: Duration::from_micros(1),
            continuous: true,
        }
    }

    #[test]
    fn child_monitor_waits_for_the_first_completed_merged_attempt() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc::{TryRecvError, sync_channel};

        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let (entered_tx, entered_rx) = sync_channel(0);
        let (release_tx, release_rx) = sync_channel(0);
        let monitor = ChildProcessMonitor::start_with_sampler(7, move |_| {
            if worker_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                entered_tx.send(()).expect("publish first attempt entry");
                release_rx.recv().expect("release first attempt");
            }
            complete_child_sample()
        });
        let (returned_tx, returned_rx) = sync_channel(0);
        thread::scope(|scope| {
            let monitor = &monitor;
            scope.spawn(move || {
                monitor.begin_processing();
                returned_tx
                    .send(
                        monitor
                            .first_sample_aggregate()
                            .expect("first aggregate published before begin returns"),
                    )
                    .expect("publish begin return");
            });
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("first attempt entered");
            assert!(matches!(returned_rx.try_recv(), Err(TryRecvError::Empty)));
            release_tx.send(()).expect("complete first attempt");
            let aggregate = returned_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("begin returns after merge");
            assert_eq!(aggregate.child_process_sample_count, 1);
            assert_eq!(aggregate.child_peak_rss_bytes, 4_096);
            assert_eq!(aggregate.child_peak_open_files, 3);
        });
        let resources = monitor.stop();
        assert!(attempts.load(Ordering::SeqCst) > 0);
        assert_eq!(resources.child_process_sample_count, 1);
        assert_eq!(resources.child_peak_rss_bytes, 4_096);
        assert_eq!(resources.child_peak_open_files, 3);
    }

    #[test]
    fn child_monitor_repeated_begin_is_idempotent_and_stop_joins() {
        let monitor = ChildProcessMonitor::start_with_sampler(7, |_| complete_child_sample());
        monitor.begin_processing();
        monitor.begin_processing();
        let resources = monitor.stop();
        assert_eq!(resources.child_process_sample_count, 1);
    }

    #[test]
    fn child_monitor_unarmed_drop_wakes_without_sampling() {
        let sampled = Arc::new(AtomicBool::new(false));
        let worker_sampled = Arc::clone(&sampled);
        let monitor = ChildProcessMonitor::start_with_sampler(7, move |_| {
            worker_sampled.store(true, Ordering::SeqCst);
            complete_child_sample()
        });
        drop(monitor);
        assert!(!sampled.load(Ordering::SeqCst));
    }

    #[test]
    fn child_monitor_accounts_the_ready_handshake_wall_exactly_once() {
        let monitor = ChildProcessMonitor::start_with_sampler(7, |_| {
            panic!("unarmed monitor must not sample")
        });
        let ready_wall = monitor.ready_wall;
        let resources = monitor.stop();
        assert_eq!(resources.observer_wall, ready_wall);
        assert_eq!(resources.child_process_sample_count, 0);
    }

    #[test]
    fn child_monitor_worker_failure_wakes_and_discards_evidence() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc::sync_channel;

        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let (failing_tx, failing_rx) = sync_channel(0);
        let monitor = ChildProcessMonitor::start_with_sampler(7, move |_| -> Resources {
            if worker_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return complete_child_sample();
            }
            failing_tx.send(()).expect("publish failing attempt");
            panic!("injected sampler failure after successful evidence")
        });
        monitor.begin_processing();
        failing_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker reaches failing attempt");
        let resources = monitor.stop();
        assert_eq!(resources.peak_rss_bytes, 0);
        assert_eq!(resources.peak_open_files, 0);
        assert_eq!(resources.rss_sample_count, 0);
        assert_eq!(resources.open_file_sample_count, 0);
        assert_eq!(resources.child_process_sample_count, 0);
    }

    #[test]
    fn process_tree_discovery_is_recursive_deduplicated_bounded_and_fail_closed() {
        let discovered = discover_process_tree(1, |pid| match pid {
            1 => Some(vec![2, 2]),
            2 => Some(vec![1, 3]),
            3 => Some(Vec::new()),
            _ => None,
        });
        assert_eq!(
            discovered,
            ProcessTreeDiscovery {
                pids: vec![1, 2, 3],
                complete: true,
            }
        );

        let partial = discover_process_tree(1, |pid| (pid == 1).then_some(vec![2]));
        assert_eq!(
            partial,
            ProcessTreeDiscovery {
                pids: vec![1, 2],
                complete: false,
            }
        );

        let bounded = discover_process_tree(1, |pid| Some(vec![pid.saturating_add(1)]));
        assert_eq!(bounded.pids.len(), MAX_PROCESS_TREE_PIDS);
        assert!(!bounded.complete);
    }

    #[test]
    fn native_sampler_observes_the_current_process_completely() {
        let resources = sample_process(std::process::id());
        assert!(resources.peak_rss_bytes > 0);
        assert!(resources.peak_open_files > 0);
        assert_eq!(resources.rss_sample_count, 1);
        assert_eq!(resources.open_file_sample_count, 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_ffi_matches_the_pinned_sdk_contract() {
        macos_process::assert_sdk_layout_and_return_validation();
    }

    #[test]
    fn linux_proc_parsers_are_strict_and_checked() {
        use std::io::Cursor;

        assert_eq!(
            parse_linux_rss_bytes("Name:\tfixture\nVmRSS:\t123 kB\n"),
            Some(123 * 1_024)
        );
        assert_eq!(parse_linux_rss_bytes("VmRSS:\t0 kB\n"), None);
        assert_eq!(parse_linux_rss_bytes("VmRSS:\t123 bytes\n"), None);
        assert_eq!(parse_linux_rss_bytes("VmRSS:\t123 kB surplus\n"), None);
        assert_eq!(parse_linux_rss_bytes("VmRSS:\t+123 kB\n"), None);
        assert_eq!(parse_linux_rss_bytes("VmRSS:\tinvalid kB\n"), None);
        assert_eq!(parse_linux_child_pids("2 3 5\n"), Some(vec![2, 3, 5]));
        assert_eq!(parse_linux_child_pids(""), Some(Vec::new()));
        assert_eq!(parse_linux_child_pids("0\n"), None);
        assert_eq!(parse_linux_child_pids("+2\n"), None);
        assert_eq!(parse_linux_child_pids("-2\n"), None);
        assert_eq!(parse_linux_child_pids("2 invalid\n"), None);
        assert_eq!(parse_linux_child_pids("4294967296\n"), None);

        let boundary = "1 ".repeat(MAX_PROCESS_TREE_PIDS);
        assert_eq!(
            parse_linux_child_pids(&boundary)
                .expect("entry-cap boundary")
                .len(),
            MAX_PROCESS_TREE_PIDS
        );
        let oversized_entries = format!("{boundary}1");
        assert_eq!(parse_linux_child_pids(&oversized_entries), None);

        let byte_cap = linux_child_list_byte_cap().expect("checked child-list byte cap");
        let exact = vec![b'1'; byte_cap];
        assert_eq!(
            read_bounded_utf8(Cursor::new(exact.clone()), byte_cap)
                .expect("exact byte-cap input")
                .len(),
            byte_cap
        );
        let mut oversized_bytes = exact;
        oversized_bytes.push(b'1');
        assert_eq!(
            read_bounded_utf8(Cursor::new(oversized_bytes), byte_cap),
            None
        );
        assert_eq!(read_bounded_utf8(Cursor::new(vec![0xff]), byte_cap), None);
    }

    #[test]
    fn unsupported_platform_command_fallback_parsers_are_fail_closed() {
        assert_eq!(parse_fallback_rss_bytes(b"123\n"), Some(123 * 1_024));
        assert_eq!(parse_fallback_rss_bytes(b"0\n"), None);
        assert_eq!(parse_fallback_rss_bytes(b"123 surplus\n"), None);
        assert_eq!(parse_fallback_rss_bytes(b"+123\n"), None);
        assert_eq!(parse_fallback_rss_bytes(b"invalid\n"), None);
        assert_eq!(parse_fallback_rss_bytes(&[0xff]), None);
        assert_eq!(
            parse_fallback_open_file_count(b"p42\nf0\nfcwd\n", 42),
            Some(2)
        );
        assert_eq!(parse_fallback_open_file_count(b"p42\n", 42), None);
        assert_eq!(parse_fallback_open_file_count(b"p41\nf0\n", 42), None);
        assert_eq!(parse_fallback_open_file_count(b"p+42\nf0\n", 42), None);
        assert_eq!(parse_fallback_open_file_count(b"p42\nf\n", 42), None);
        assert_eq!(parse_fallback_open_file_count(b"p42\nx0\n", 42), None);
        assert_eq!(parse_fallback_open_file_count(b"p42\nf0\np42\n", 42), None);
        assert_eq!(parse_fallback_open_file_count(&[0xff], 42), None);
        assert_eq!(
            parse_fallback_process_relationships(b"2 1\n3 2\n"),
            Some(vec![(2, 1), (3, 2)])
        );
        assert_eq!(parse_fallback_process_relationships(b""), None);
        assert_eq!(parse_fallback_process_relationships(b"0 1\n"), None);
        assert_eq!(parse_fallback_process_relationships(b"+2 1\n"), None);
        assert_eq!(parse_fallback_process_relationships(b"2 invalid\n"), None);
        assert_eq!(parse_fallback_process_relationships(b"2 1 surplus\n"), None);
        assert_eq!(parse_fallback_process_relationships(&[0xff]), None);
    }

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
