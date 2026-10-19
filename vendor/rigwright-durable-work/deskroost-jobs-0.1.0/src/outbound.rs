//! Outbound TLS settings that Node reads from its process environment, for the
//! HTTP clients of every crate (`reqwest`), so a deployment keeps the behavior of
//! the Node runtime it replaces:
//!
//! * `NODE_EXTRA_CA_CERTS`: a PEM file whose certificates are added to the
//!   trusted roots of every outbound TLS connection. A file that cannot be read
//!   is reported once on standard error and ignored, as Node does at startup.
//! * `NODE_TLS_REJECT_UNAUTHORIZED=0` (exactly the string `0`): certificate and
//!   host name verification is switched off for every outbound TLS connection,
//!   with Node's one-time process warning. Any other value keeps verification on.
//!
//! `NODE_OPTIONS` has no counterpart: it passes flags to the Node binary.
//! PostgreSQL connections take their trust anchors from `sslrootcert` in
//! `DATABASE_URL` (sqlx), not from these variables.
//!
//! Build every outbound client with [`client_builder`] (or [`client`]).
use std::{
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

/// The settings in force for this process.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Extra trust anchors from `NODE_EXTRA_CA_CERTS`.
    pub extra_roots: Vec<reqwest::Certificate>,
    /// `NODE_TLS_REJECT_UNAUTHORIZED === '0'`.
    pub insecure: bool,
}

/// The warning Node prints for `NODE_TLS_REJECT_UNAUTHORIZED=0`.
pub const INSECURE_WARNING: &str = "Setting the NODE_TLS_REJECT_UNAUTHORIZED environment variable to '0' makes TLS connections and HTTPS requests insecure by disabling certificate verification.";

/// Reads the settings from `vars`; every warning is passed to `warn`.
pub fn load(vars: &HashMap<String, String>, mut warn: impl FnMut(&str)) -> Settings {
    let mut settings = Settings {
        insecure: vars
            .get("NODE_TLS_REJECT_UNAUTHORIZED")
            .is_some_and(|value| value == "0"),
        ..Settings::default()
    };
    if settings.insecure {
        warn(INSECURE_WARNING);
    }
    if let Some(path) = vars
        .get("NODE_EXTRA_CA_CERTS")
        .filter(|path| !path.is_empty())
    {
        let loaded = std::fs::read(path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
                _ => error.to_string(),
            })
            .and_then(|bytes| {
                reqwest::Certificate::from_pem_bundle(&bytes).map_err(|error| error.to_string())
            });
        match loaded {
            Ok(roots) => settings.extra_roots = roots,
            Err(reason) => warn(&format!(
                "Ignoring extra certs from `{path}`, load failed: {reason}"
            )),
        }
    }
    settings
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();
static WARNED: AtomicBool = AtomicBool::new(false);

/// The process-wide settings, read from the environment on first use.
pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| {
        let vars: HashMap<String, String> = std::env::vars().collect();
        load(&vars, |message| {
            // Node prints each process warning once, then the trace hint once.
            let first = !WARNED.swap(true, Ordering::SeqCst);
            eprintln!("(deskroost:{}) Warning: {message}", std::process::id());
            if first {
                eprintln!(
                    "(Use `--trace-warnings` to show where the warning was created; Deskroost reads the Node TLS variables at the first outbound connection)"
                );
            }
        })
    })
}

/// Applies `settings` to a client builder.
pub fn apply(builder: reqwest::ClientBuilder, settings: &Settings) -> reqwest::ClientBuilder {
    let mut builder = builder;
    for root in &settings.extra_roots {
        builder = builder.add_root_certificate(root.clone());
    }
    if settings.insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder
}

/// `reqwest::Client::builder()` with the process TLS settings applied.
pub fn client_builder() -> reqwest::ClientBuilder {
    apply(reqwest::Client::builder(), settings())
}

/// `reqwest::Client::new()` with the process TLS settings applied.
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("the outbound HTTP client builds")
}

/// The one process-wide client of the model provider requests (chat, assistant, compiler): no proxy, at most 20
/// redirects, the process TLS settings. Request deadlines are set per request.
///
/// Deskroost latency work: reusing one client keeps provider connections alive between model calls, as Node's
/// global `fetch` dispatcher does, instead of paying a new TCP and TLS handshake (Ollama Cloud is HTTPS) on every
/// call. Requests and responses are unchanged.
pub fn model_client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            client_builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::limited(20))
                .build()
                .map_err(|error| error.to_string())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn collect(pairs: &[(&str, &str)]) -> (Settings, Vec<String>) {
        let mut warnings = vec![];
        let settings = load(&vars(pairs), |message| warnings.push(message.to_owned()));
        (settings, warnings)
    }

    #[test]
    fn only_the_string_zero_disables_verification_and_warns_once() {
        let (settings, warnings) = collect(&[("NODE_TLS_REJECT_UNAUTHORIZED", "0")]);
        assert!(settings.insecure);
        assert_eq!(warnings, [INSECURE_WARNING]);
        for value in ["1", "", "false", "00", " 0", "off"] {
            let (settings, warnings) = collect(&[("NODE_TLS_REJECT_UNAUTHORIZED", value)]);
            assert!(!settings.insecure, "{value:?}");
            assert!(warnings.is_empty(), "{value:?}");
        }
        let (settings, warnings) = collect(&[]);
        assert!(!settings.insecure && settings.extra_roots.is_empty() && warnings.is_empty());
    }

    #[test]
    fn a_missing_or_invalid_bundle_is_reported_and_ignored() {
        let (settings, warnings) = collect(&[("NODE_EXTRA_CA_CERTS", "/nonexistent/ca.pem")]);
        assert!(settings.extra_roots.is_empty());
        assert_eq!(
            warnings,
            [
                "Ignoring extra certs from `/nonexistent/ca.pem`, load failed: No such file or directory"
            ]
        );
        // An empty variable counts as unset.
        assert!(collect(&[("NODE_EXTRA_CA_CERTS", "")]).1.is_empty());
        let file =
            std::env::temp_dir().join(format!("deskroost-outbound-{}.pem", std::process::id()));
        std::fs::write(&file, "not a certificate\n").unwrap();
        let (settings, warnings) = collect(&[("NODE_EXTRA_CA_CERTS", file.to_str().unwrap())]);
        let _ = std::fs::remove_file(&file);
        assert!(settings.extra_roots.is_empty());
        assert!(warnings.len() <= 1);
    }
}
