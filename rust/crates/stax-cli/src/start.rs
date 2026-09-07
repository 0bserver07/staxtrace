//! `stax start` — `cli.py:121`–`:211`, the boot path.
//!
//! Everything before the bind is byte-comparable and is ported literally; the
//! bind itself is handed to the wave-5 `stax-server` binary, spawned rather than
//! linked (DIV-308 — linking it back into this crate would undo the dependency
//! split tranche 3 just landed; the manifest carries the full note). Four things
//! about those ninety lines are load-bearing and easy to lose:
//!
//! * **`port or cfg.port` is Python truthiness, not `Option`.** `--port 0` is
//!   *falsy*, so it falls through to the configured port. A `unwrap_or` port
//!   would bind 0 (an ephemeral port) where the reference binds 8081. Same for
//!   `--host ''`, which is the `--project ''` class the wave-8 tranche found
//!   twice.
//! * **`--data-dir` re-execs.** The data paths are bound at import in Python, so
//!   assigning after the fact would leave half the app on the old home; the
//!   reference sets `$STACKUNDERFLOW_HOME` and `execvp`s its own argv. This port
//!   does the same with `CommandExt::exec` — the loop terminates on the same
//!   condition (the variable already holds the resolved path), and `exec` is a
//!   *safe* std function, so `forbid(unsafe_code)` holds.
//! * **The two `--data-dir` failure messages come from different layers.** A
//!   path that *is a file* is rejected by Click's own `Path(file_okay=False)`
//!   conversion, which quotes the option — `Invalid value for '--data-dir':
//!   Directory '…' is a file.` — and prints the **raw** argument. A path that
//!   simply does not exist reaches the body's `click.BadParameter`, which does
//!   **not** quote and prints the **resolved** path. Two hints, two paths, one
//!   `Usage:` block; both are parity rows.
//! * **`_ensure_state_dir` writes `config.json`**, the same file `cfg set`
//!   writes. On a fresh home `start` therefore leaves a config carrying
//!   `version` + `created` and prints a two-line welcome.
//!
//! # Recorded divergences
//!
//! * **DIV-303** — `_ensure_state_dir`'s `config.json` carries `__version__` and
//!   `datetime.now().isoformat()`. The port's version was pinned `0.0.0` through the campaign (§5,
//!   maintainer-only) and the timestamp is a wall clock, so this file can never
//!   be byte-compared, not even between two runs of the *same* implementation.
//!   The differ compares its key set and shape instead.
//! * **DIV-304 — CLOSED at the flip (2026-08-05).** `--no-watcher` /
//!   `--no-lock` were accepted-and-inert while the watcher was unwired; the
//!   resident flip wired it at THIS layer (`stax-cli` links `stax-etl`;
//!   `stax-server` may not — DIV-279/308), so both flags now gate a real
//!   watcher and a real `server.lock`, no environment variable involved —
//!   which is what the injection law wanted all along.
//! * **DIV-305 — NARROWED at the flip.** The lifespan's watcher now runs (see
//!   [`resident_watcher`]); its one-shot boot-time catch-up ingest and
//!   price-book activation remain unported — a store that changed while the
//!   server was down is picked up on the file's next change or a manual
//!   `stax etl backfill`, and the flip's runbook does that catch-up
//!   explicitly. The **synchronous** half — `db.connect` + `schema.apply` —
//!   was already ported (wave 7).
//! * **DIV-306** — a bind failure. The reference launches uvicorn on a daemon
//!   thread, sleeps 1.0 s, and prints `staxtrace is live at …` *whether or
//!   not the bind succeeded*; with the port already in use it then falls
//!   straight through `wait_forever` and exits **0** having claimed success.
//!   This port binds first and reports the failure. Deliberately not reproduced
//!   bug-for-bug — a false success is the one class the campaign has agreed not
//!   to inherit silently — and filed for the maintainer's desk.
//! * **DIV-307** — `webbrowser.open` walks CPython's browser chain
//!   (`$BROWSER`, then a built-in list); the port spawns `xdg-open`. Only
//!   reachable with `auto_browser` on and `--headless` off, which is never the
//!   case under any harness.

use std::io::Write as _;
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use clap::Args;
use stax_core::settings::{APP_DIR_ENV, app_dir};

use crate::click::{self, Output, UsageError};
use crate::settings::{self, Default as SpecDefault};

/// `stax start [OPTIONS]`.
#[derive(Debug, Args)]
pub struct StartArgs {
    /// Server port
    #[arg(short = 'p', long)]
    pub port: Option<i64>,
    /// Bind address
    #[arg(short = 'H', long)]
    pub host: Option<String>,
    /// Don't open the browser
    #[arg(long)]
    pub headless: bool,
    /// Clear disk cache first
    #[arg(long)]
    pub fresh: bool,
    /// Disable the Wave 2C ETL filesystem watcher (headless / debugging).
    #[arg(long = "no-watcher")]
    pub no_watcher: bool,
    /// Skip the singleton watcher lock at ~/.stackunderflow/server.lock.
    /// Headless / test scenarios only — letting two instances run watchers
    /// against the same store will race on ingest+marts.
    #[arg(long = "no-lock")]
    pub no_lock: bool,
    /// Serve a dataset from somewhere other than ~/.stackunderflow — a store
    /// copied off another machine, or a backup's stackunderflow-state/
    /// directory. Same as setting STACKUNDERFLOW_HOME.
    #[arg(long = "data-dir", value_name = "DIRECTORY")]
    pub data_dir: Option<PathBuf>,
}

/// The `Usage:` tail Click prints for both `start` and, through
/// `ctx.invoke`, for `init` when it delegates.
const START_ARG_SPEC: &str = "[OPTIONS]";

/// Run `start`.
///
/// # Errors
/// A filesystem failure clearing the cache or scaffolding the state directory,
/// or a bind failure (DIV-306).
pub fn run_start(args: &StartArgs) -> Result<Output> {
    run_start_with(args, "start", String::new())
}

/// [`run_start`], told which command path Click would print in a usage error and
/// what a delegating command has already produced.
///
/// `init` delegates through `ctx.invoke(start_cmd, …)`, which runs `start`'s
/// body under `init`'s context: the skills-install lines are already on stdout
/// when `start` begins, and any usage error `start` raised would carry `init`'s
/// command path. `prefix` is those lines, handed down so they are flushed in the
/// reference's order — before the welcome banner and before the bind.
///
/// # Errors
/// As [`run_start`].
pub fn run_start_with(args: &StartArgs, command_path: &str, prefix: String) -> Result<Output> {
    if let Some(data_dir) = &args.data_dir {
        match reexec_with_data_dir(data_dir, command_path)? {
            DataDir::Usage(error) => return Ok(Output::usage(&error, click::PROGRAM)),
            // The environment already points here; fall through and serve.
            DataDir::AlreadyThere => {}
            DataDir::ReExec(resolved) => return exec_self(&resolved),
        }
    }

    let mut out = prefix;

    if args.fresh {
        let cache = app_dir().join("cache");
        if cache.exists() {
            std::fs::remove_dir_all(&cache)?;
            out.push_str(&format!("  cache cleared: {}\n", cache.display()));
        }
    }

    out.push_str(&ensure_state_dir(&app_dir())?);

    let config = settings::load();
    let env = settings::ProcessEnv;
    let port = resolve_port(args.port, &config, &env);
    let host = resolve_host(args.host.as_deref(), &config, &env);

    let mut err = String::new();
    if !is_loopback(&host) {
        err.push_str(&exposure_warning(&host));
    }

    boot(args, &host, port, &config, &env, out, err)
}

// ── `--data-dir` ─────────────────────────────────────────────────────────────

/// What `_reexec_with_data_dir` decided.
enum DataDir {
    /// One of Click's two rejections.
    Usage(UsageError),
    /// `os.environ.get(APP_DIR_ENV) == str(resolved)` — the loop's brake.
    AlreadyThere,
    /// Set the variable and `execvp` this same argv.
    ReExec(PathBuf),
}

/// `_reexec_with_data_dir`, minus the exec itself.
fn reexec_with_data_dir(data_dir: &Path, command_path: &str) -> Result<DataDir> {
    // Click's `Path(file_okay=False)` conversion runs at PARSE time, so it wins
    // over the body's check and it prints the argument as given.
    if data_dir.is_file() {
        return Ok(DataDir::Usage(UsageError::bad_parameter(
            command_path,
            START_ARG_SPEC,
            "'--data-dir'",
            format!("Directory '{}' is a file.", data_dir.display()),
        )));
    }

    let resolved = py_resolve(&expand_user(data_dir));
    if !resolved.is_dir() {
        return Ok(DataDir::Usage(UsageError::bad_parameter(
            command_path,
            START_ARG_SPEC,
            "--data-dir",
            format!("not a directory: {}", resolved.display()),
        )));
    }
    if std::env::var_os(APP_DIR_ENV).is_some_and(|current| current == resolved.as_os_str()) {
        return Ok(DataDir::AlreadyThere);
    }
    Ok(DataDir::ReExec(resolved))
}

/// `os.execvp(sys.argv[0], sys.argv)` with `$STACKUNDERFLOW_HOME` set.
///
/// `CommandExt::exec` replaces the process image, so it only *returns* when the
/// exec failed — which is the one branch that produces output, and the
/// reference's message for it, verbatim.
#[cfg(unix)]
fn exec_self(resolved: &Path) -> Result<Output> {
    use std::os::unix::process::CommandExt as _;

    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some((program, rest)) = argv.split_first() else {
        anyhow::bail!("no argv[0] to re-exec");
    };
    let error = std::process::Command::new(program)
        .args(rest)
        .env(APP_DIR_ENV, resolved)
        .exec();
    Err(anyhow::anyhow!(
        "could not re-exec to apply --data-dir ({error}). \
         Run with the environment variable instead: \
         {APP_DIR_ENV}={} stax start",
        resolved.display()
    ))
}

#[cfg(not(unix))]
fn exec_self(resolved: &Path) -> Result<Output> {
    anyhow::bail!(
        "could not re-exec to apply --data-dir (no exec on this platform). \
         Run with the environment variable instead: \
         {APP_DIR_ENV}={} stax start",
        resolved.display()
    )
}

/// `Path.expanduser()` — a leading `~` only, as `stax_core::settings` does.
fn expand_user(path: &Path) -> PathBuf {
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Normal(first)) if first == "~" =>
        {
            #[allow(
                deprecated,
                reason = "the same call `stax_core::settings::home_dir` makes, \
                for the same reason: it is the platform-correct answer on the pin"
            )]
            match std::env::home_dir() {
                Some(home) => home.join(parts.as_path()),
                None => path.to_path_buf(),
            }
        }
        _ => path.to_path_buf(),
    }
}

/// `Path.resolve()` — i.e. `resolve(strict=False)`: absolutise, drop `.`, pop
/// `..`, and follow the symlinks that exist.
///
/// `std::fs::canonicalize` is not this: it *fails* when the path does not exist,
/// and the message for a missing `--data-dir` prints the resolved path — so the
/// missing case is precisely the case that has to work.
///
/// Two steps, in this order, and the order is the reason the first draft was
/// wrong: the components are normalised **first**, then the longest existing
/// prefix of the result is canonicalised. Walking up before normalising stalls
/// on a `..` component, because `Path::file_name` answers `None` for it.
///
/// Where this differs from CPython, recorded: `realpath` resolves symlinks
/// left-to-right and therefore lets a symlink change what a following `..` means.
/// Textual normalisation cannot. No `--data-dir` the campaign has seen is a
/// symlink followed by `..`, and the alternative is a full realpath walk in a
/// verb whose only use of the value is an error message and an env var.
fn py_resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    let mut normalised = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalised.pop();
            }
            other => normalised.push(other.as_os_str()),
        }
    }

    let mut existing = normalised.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            break;
        };
        tail.push(name);
        if !existing.pop() {
            break;
        }
    }

    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.iter().rev() {
        resolved.push(name);
    }
    resolved
}

// ── the state directory ──────────────────────────────────────────────────────

/// `_ensure_state_dir` — the welcome banner and the `config.json` marker.
///
/// `mkdir(exist_ok=True)` is **not** recursive in the reference, so a home whose
/// own parent is missing fails here rather than being created; reproduced.
fn ensure_state_dir(state_dir: &Path) -> Result<String> {
    let marker = state_dir.join("config.json");
    if marker.exists() {
        return Ok(String::new());
    }
    let mut out = String::new();
    out.push_str("\n  Welcome to staxtrace!\n");
    out.push_str("  Your Claude Code knowledge base\n\n");
    match std::fs::create_dir(state_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }
    std::fs::write(&marker, marker_json())?;
    Ok(out)
}

/// `json.dumps({"version": __version__, "created": datetime.now().isoformat()})`.
///
/// DIV-303: `__version__` tracks the workspace version (`0.0.0` through the campaign, `1.0.0` from the first Rust release), and `created` is a wall clock.
fn marker_json() -> String {
    format!(
        "{{\"version\": \"{}\", \"created\": \"{}\"}}",
        env!("CARGO_PKG_VERSION"),
        local_isoformat(crate::pyclock::now_epoch_secs())
    )
}

/// `datetime.now().isoformat()` at second resolution.
///
/// CPython omits the fractional part when `microsecond == 0`; this port always
/// omits it, because the campaign has no sub-second clock and the field is
/// already unusable for comparison (DIV-303).
fn local_isoformat(utc_epoch_secs: i64) -> String {
    let offset = i64::from(crate::pyclock::local_offset_seconds(utc_epoch_secs));
    let local = utc_epoch_secs + offset;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

// ── settings resolution ──────────────────────────────────────────────────────

/// `port = port or cfg.port` — **Python truthiness**, so `--port 0` falls back.
#[must_use]
pub fn resolve_port(
    flag: Option<i64>,
    config: &settings::ConfigFile,
    env: &dyn settings::Env,
) -> i64 {
    match flag {
        Some(value) if value != 0 => value,
        _ => match settings::get("port", config, env) {
            Some(stax_core::queries::pyjson::Value::Int(value)) => value,
            _ => match settings::spec_of("port").map(|spec| spec.default) {
                Some(SpecDefault::Int(value)) => value,
                _ => 8081,
            },
        },
    }
}

/// `host = host or cfg.host` — the same truthiness, so `--host ''` falls back.
#[must_use]
pub fn resolve_host(
    flag: Option<&str>,
    config: &settings::ConfigFile,
    env: &dyn settings::Env,
) -> String {
    match flag {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => match settings::get("host", config, env) {
            Some(value) => settings::py_str_value(&value),
            None => "127.0.0.1".to_owned(),
        },
    }
}

/// The three spellings the reference treats as "not exposed".
#[must_use]
pub fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// The one-line `click.secho(..., fg="yellow", err=True)` block.
#[must_use]
pub fn exposure_warning(host: &str) -> String {
    format!(
        "  ⚠  Binding to {host} exposes the dashboard to anyone who can reach \
that interface. The API has no authentication — session data, tokens, and cost \
info are served unauthenticated. Use 127.0.0.1 unless you know what you're \
doing.\n"
    )
}

/// `f"http://{host}:{port}"` — the **raw** host string, resolved or not.
#[must_use]
pub fn dashboard_url(host: &str, port: i64) -> String {
    format!("http://{host}:{port}")
}

// ── the boot ─────────────────────────────────────────────────────────────────

/// Resolve the bind address from the host string the way `uvicorn.run` does.
///
/// `--host localhost` is legal in the reference and is not an `IpAddr`, so the
/// name is resolved; a name that resolves to several addresses takes the first,
/// as `socket.getaddrinfo`'s consumer does.
fn bind_addr(host: &str, port: i64) -> Result<SocketAddr> {
    let port = u16::try_from(port)
        .map_err(|_| anyhow::anyhow!("port {port} is out of range for a TCP bind"))?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not resolve host {host}"))
}

/// Launch the server and block until it stops — the `_ServerHandle` half.
///
/// DIV-308: the reference runs uvicorn on a daemon thread inside the CLI
/// process; this spawns the `stax-server` binary. Two consequences, both
/// deliberate:
///
/// * the readiness handshake is **real**. The reference sleeps a flat 1.0 s and
///   then claims the dashboard is live whether or not the bind worked (DIV-306);
///   this waits for the child's own `listening on …` line, so the message is
///   true when it prints and a failed bind is reported instead of celebrated.
/// * Ctrl-C reaches the child through the terminal's process group, exactly as
///   it reaches uvicorn's thread, so `\nStopped.` still prints on the way out.
fn boot(
    args: &StartArgs,
    host: &str,
    port: i64,
    config: &settings::ConfigFile,
    env: &dyn settings::Env,
    pre_stdout: String,
    pre_stderr: String,
) -> Result<Output> {
    let addr = bind_addr(host, port)?;
    let home = app_dir();
    let store_path = home.join("store.db");

    // The synchronous half of the FastAPI lifespan: `db.connect` +
    // `schema.apply`, before anything serves. Wave 7 is the wave that made this
    // callable at all — before the runner existed, `start` could only serve a
    // store somebody else had migrated.
    apply_schema_at_boot(&store_path)?;

    let server = server_binary();
    if !server.is_absolute() && !on_path(&server) {
        anyhow::bail!(
            "stax-server is neither next to this binary nor on PATH. The brew tap and \
             install.sh ship all three binaries side by side — a partial install (e.g. \
             `cargo install` of one crate) is the usual cause. `stax doctor --install` \
             shows what's missing."
        );
    }
    let mut command = std::process::Command::new(server);
    command
        .arg("--host")
        .arg(addr.ip().to_string())
        .arg("--port")
        .arg(addr.port().to_string())
        .arg("--data-dir")
        .arg(&home)
        .stdout(std::process::Stdio::piped());
    // Only name a package dir the resolver actually found — with none, the
    // server's own default applies, which since the wave-10 packaging pass
    // falls back to its embedded copies (an installed binary far from any
    // checkout is exactly that case).
    if let Some(dir) = crate::status::package_dir() {
        command.arg("--package-dir").arg(dir);
    }
    if addr.port() == 8095 {
        // The flip (2026-08-05): a supervised boot on the resident port is the
        // deliberate ask the child's guard exists to distinguish from a stray
        // harness invocation.
        command.arg("--resident");
    }
    let mut child = command.spawn()?;

    // Readiness: the child prints one line on stdout when it is bound.
    let ready = {
        use std::io::BufRead as _;
        let stdout = child.stdout.take();
        let mut line = String::new();
        if let Some(stdout) = stdout {
            let _ = std::io::BufReader::new(stdout).read_line(&mut line);
        }
        line
    };
    if ready.is_empty() {
        let status = child.wait()?;
        anyhow::bail!(
            "the dashboard server exited before it bound {addr} ({status}). \
             Something else is probably already on that port."
        );
    }

    // stdout is flushed before the wait: a buffered "live at" line would arrive
    // only at shutdown, and the harness reads it to know the port is up.
    let url = dashboard_url(host, port);
    print!("{pre_stdout}");
    print!("\n  staxtrace is live at {url}\n");
    print!("  Ctrl+C to stop\n\n");
    std::io::stdout().flush()?;
    if !pre_stderr.is_empty() {
        eprint!("{pre_stderr}");
        std::io::stderr().flush()?;
    }

    if !args.headless && auto_browser(config, env) {
        open_browser(&url);
    }

    // The asynchronous half of the reference's lifespan — the filesystem
    // watcher (DIV-304/305) — wired at the supervisor layer because
    // `stax-server` may not link `stax-etl` (DIV-279/308). Started only after
    // the child is bound, so a failed bind never leaves a held lock; dropped
    // after the child exits, which stops the thread and releases the lock.
    // Cycle reports go to stderr, where the reference's logging goes — stdout
    // stays byte-identical to the pre-watcher shape.
    let watcher = if args.no_watcher {
        None
    } else {
        resident_watcher(&home, &store_path, args.no_lock)
    };

    // `handle.wait_forever()` — the reference swallows the KeyboardInterrupt and
    // falls through to the closing line, so a Ctrl-C is a clean exit here too.
    let _ = child.wait();
    drop(watcher);
    Ok(Output::ok("\nStopped.\n"))
}

/// The resident watcher: singleton lock, pricing-primed normalize context,
/// then [`stax_etl::ingest::watcher::start_watcher`] over the default adapter
/// registry. `None` — with the reason on stderr — means "serve HTTP without a
/// watcher", which is the reference's disposition for every failure here: the
/// dashboard reads the store and is happy without one.
fn resident_watcher(
    home: &Path,
    store_path: &Path,
    no_lock: bool,
) -> Option<(
    Option<stax_etl::ingest::lock::LockHandle>,
    stax_etl::ingest::watcher::WatcherHandle,
)> {
    use stax_etl::ingest::{SystemClock, guard, lock, watcher};

    let lock_handle = if no_lock {
        None
    } else {
        match lock::acquire_watcher_lock(&home.join("server.lock")) {
            Some(handle) => Some(handle),
            None => {
                eprintln!("  watcher: lock held by another instance; serving without one");
                return None;
            }
        }
    };

    let engine = match crate::status::engine_for_cli(crate::status::package_dir().as_deref()) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("  watcher: pricing engine unavailable ({err}); serving without one");
            return None;
        }
    };
    let ctx = stax_etl::normalize::NormalizeContext::new(engine);

    let store = store_path.to_path_buf();
    match watcher::start_watcher(
        move || guard::open_resident(&store),
        stax_adapters::registry::registered,
        ctx,
        Box::new(SystemClock),
        watcher::WatcherConfig::default(),
        |report| {
            // Python's single `_log.info` line per cycle.
            eprintln!(
                "  watcher: +{} messages, {} events, {} marts, {:.1} ms",
                report.messages_added(),
                report.events_normalised,
                report.marts.len(),
                report.elapsed.as_secs_f64() * 1000.0
            );
        },
    ) {
        Ok(handle) => Some((lock_handle, handle)),
        Err(err) => {
            eprintln!("  watcher: could not start ({err}); serving without one");
            None
        }
    }
}

/// Where the `stax-server` binary is.
///
/// Next to this executable is the answer for every layout the campaign has —
/// `target/<profile>/` in development, one install directory in a package — and
/// the bare name on `$PATH` is the fallback rather than a guess at a prefix.
/// Is a bare binary name resolvable through `$PATH`?
fn on_path(name: &Path) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
}

fn server_binary() -> PathBuf {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("stax-server")));
    match sibling {
        Some(path) if path.is_file() => path,
        _ => PathBuf::from("stax-server"),
    }
}

/// The lifespan's `db.connect` + `schema.apply`, with the reference's
/// "log it and keep serving" disposition.
fn apply_schema_at_boot(store_path: &Path) -> Result<()> {
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = rusqlite::Connection::open(store_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    if let Err(err) = stax_core::schema::apply(&conn) {
        // `logger.error("Schema apply failed at startup: %s", e)` — the
        // reference does not abort the boot, and neither does this.
        eprintln!("Schema apply failed at startup: {err}");
    }
    Ok(())
}

/// `cfg.auto_browser`.
fn auto_browser(config: &settings::ConfigFile, env: &dyn settings::Env) -> bool {
    matches!(
        settings::get("auto_browser", config, env),
        Some(stax_core::queries::pyjson::Value::Bool(true)) | None
    )
}

/// `threading.Timer(0.4, lambda: webbrowser.open(url))` — DIV-307.
fn open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// `start`'s local compile-time `package_dir()` (CARGO_MANIFEST_DIR-based) is
// gone: it baked the BUILD machine's checkout into the binary, which is the
// exact hazard the wave-10 packaging pass existed to remove. Both former
// callers now use `crate::status::package_dir()` — env, then walk-up, then
// `None`, which for the spawned server means "no --package-dir flag at all"
// and its own embedded fallbacks apply.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::MapEnv;

    fn empty_config() -> settings::ConfigFile {
        settings::ConfigFile::default()
    }

    #[test]
    fn a_zero_port_is_falsy_and_falls_back_to_the_configured_one() {
        let env = MapEnv(Vec::new());
        assert_eq!(resolve_port(Some(0), &empty_config(), &env), 8081);
        assert_eq!(resolve_port(Some(8100), &empty_config(), &env), 8100);
        assert_eq!(resolve_port(None, &empty_config(), &env), 8081);
    }

    #[test]
    fn an_empty_host_is_falsy_and_falls_back_to_the_configured_one() {
        let env = MapEnv(Vec::new());
        assert_eq!(resolve_host(Some(""), &empty_config(), &env), "127.0.0.1");
        assert_eq!(
            resolve_host(Some("0.0.0.0"), &empty_config(), &env),
            "0.0.0.0"
        );
        assert_eq!(resolve_host(None, &empty_config(), &env), "127.0.0.1");
    }

    #[test]
    fn the_environment_beats_the_default_for_both() {
        // The variables are `PORT` and `HOST`, **unprefixed** — `_Opt(8081,
        // "PORT")` in `settings.py:135`. That is a live footgun on any host that
        // already exports `$PORT` (every PaaS, most CI images): the dashboard
        // silently moves. Ported bug-for-bug, and asserted here so nobody
        // "corrects" it to `STACKUNDERFLOW_PORT` and breaks drop-in parity.
        let env = MapEnv(vec![
            ("PORT".to_owned(), "9001".to_owned()),
            ("HOST".to_owned(), "0.0.0.0".to_owned()),
        ]);
        assert_eq!(resolve_port(None, &empty_config(), &env), 9001);
        assert_eq!(resolve_host(None, &empty_config(), &env), "0.0.0.0");

        let prefixed = MapEnv(vec![("STACKUNDERFLOW_PORT".to_owned(), "9001".to_owned())]);
        assert_eq!(
            resolve_port(None, &empty_config(), &prefixed),
            8081,
            "the prefixed spelling is NOT the reference's"
        );
    }

    #[test]
    fn only_the_three_reference_spellings_are_loopback() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("127.0.0.2"));
        assert!(!is_loopback(""));
    }

    #[test]
    fn the_exposure_warning_is_one_line_and_names_the_host() {
        let warning = exposure_warning("0.0.0.0");
        assert_eq!(warning.matches('\n').count(), 1);
        assert!(warning.starts_with("  ⚠  Binding to 0.0.0.0 exposes"));
        assert!(warning.ends_with("unless you know what you're doing.\n"));
    }

    #[test]
    fn the_url_uses_the_host_string_as_typed() {
        assert_eq!(dashboard_url("localhost", 8100), "http://localhost:8100");
        assert_eq!(dashboard_url("::1", 8100), "http://::1:8100");
    }

    #[test]
    fn a_missing_data_dir_is_the_bodys_unquoted_hint() {
        let DataDir::Usage(error) =
            reexec_with_data_dir(Path::new("/nonexistent-stax-parity-dir"), "start")
                .expect("no io error")
        else {
            panic!("a missing directory must be rejected");
        };
        assert_eq!(
            error.render("stackunderflow"),
            concat!(
                "Usage: stackunderflow start [OPTIONS]\n",
                "Try 'stackunderflow start --help' for help.\n",
                "\n",
                "Error: Invalid value for --data-dir: not a directory: ",
                "/nonexistent-stax-parity-dir\n",
            )
        );
    }

    #[test]
    fn a_file_data_dir_is_clicks_quoted_hint_with_the_raw_path() {
        let dir = std::env::temp_dir().join(format!("stax-start-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let file = dir.join("afile");
        std::fs::write(&file, "x").expect("file");

        let DataDir::Usage(error) = reexec_with_data_dir(&file, "start").expect("no io error")
        else {
            panic!("a file must be rejected");
        };
        assert_eq!(
            error.render("stackunderflow"),
            format!(
                concat!(
                    "Usage: stackunderflow start [OPTIONS]\n",
                    "Try 'stackunderflow start --help' for help.\n",
                    "\n",
                    "Error: Invalid value for '--data-dir': Directory '{}' is a file.\n",
                ),
                file.display()
            )
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_real_directory_asks_for_the_re_exec() {
        let dir = std::env::temp_dir().join(format!("stax-start-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        match reexec_with_data_dir(&dir, "start").expect("no io error") {
            DataDir::ReExec(resolved) => {
                assert_eq!(resolved, py_resolve(&dir));
            }
            // The harness may already point `$STACKUNDERFLOW_HOME` here.
            DataDir::AlreadyThere => {}
            DataDir::Usage(_) => panic!("a real directory is not a usage error"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn py_resolve_normalises_a_path_that_does_not_exist() {
        // `canonicalize` cannot do this — it errors — and the message for a
        // missing `--data-dir` prints the resolved path.
        let resolved = py_resolve(Path::new("/tmp/./nope-a/../nope-b/leaf"));
        assert_eq!(resolved.file_name().and_then(|n| n.to_str()), Some("leaf"));
        assert!(resolved.is_absolute());
        assert!(
            !resolved.to_string_lossy().contains(".."),
            "{} still carries a `..`",
            resolved.display()
        );
    }

    #[test]
    fn the_state_dir_scaffold_is_written_once_with_the_banner() {
        let dir = std::env::temp_dir().join(format!("stax-state-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::remove_dir(&dir).expect("empty it again");

        let first = ensure_state_dir(&dir).expect("scaffold");
        assert_eq!(
            first,
            "\n  Welcome to staxtrace!\n  Your Claude Code knowledge base\n\n"
        );
        let written = std::fs::read_to_string(dir.join("config.json")).expect("marker");
        assert!(written.starts_with("{\"version\": \""), "{written}");
        assert!(written.contains("\", \"created\": \""), "{written}");
        assert!(written.ends_with("\"}"), "{written}");

        let second = ensure_state_dir(&dir).expect("second");
        assert!(second.is_empty(), "the banner prints once");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_marker_is_json_with_exactly_two_keys() {
        // DIV-303: neither value can be compared, but the shape can.
        let text = marker_json();
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let object = parsed.as_object().expect("an object");
        assert_eq!(object.len(), 2);
        assert!(object.contains_key("version"));
        assert!(object.contains_key("created"));
    }

    #[test]
    fn local_isoformat_has_pythons_shape() {
        let stamp = local_isoformat(1_767_225_600); // 2026-01-01T00:00:00Z
        assert_eq!(stamp.len(), 19, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
        assert_eq!(&stamp[13..14], ":");
    }

    #[test]
    fn bind_addr_accepts_the_names_the_reference_accepts() {
        assert_eq!(
            bind_addr("127.0.0.1", 8100).expect("ip"),
            "127.0.0.1:8100".parse::<SocketAddr>().expect("parse")
        );
        assert!(bind_addr("localhost", 8100).is_ok(), "a name must resolve");
        assert!(bind_addr("127.0.0.1", 70_000).is_err(), "out of range");
    }
}
