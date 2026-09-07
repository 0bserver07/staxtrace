//! `stax advanced` — the discoverability home for every verb `--help` hides.
//!
//! The public surface is five verbs; the other ~40 keep working, both at the
//! top level (the parity differs and installed skills invoke them directly)
//! and namespaced here. Execution is a re-parse of the same [`crate::Cli`] —
//! no second command tree to drift out of sync.

use anyhow::Result;
use clap::{Args, CommandFactory, Parser};
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Debug, Args)]
#[command(disable_help_flag = true)]
pub struct AdvancedArgs {
    /// The verb to run, with its arguments (everything after `advanced`).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "VERB [ARGS]..."
    )]
    pub rest: Vec<OsString>,
}

pub fn run_advanced(args: &AdvancedArgs) -> Result<ExitCode> {
    let wants_directory = args.rest.is_empty() || args.rest[0] == "-h" || args.rest[0] == "--help";
    if wants_directory {
        print_directory();
        return Ok(ExitCode::SUCCESS);
    }
    let argv = std::iter::once(OsString::from("stax")).chain(args.rest.iter().cloned());
    match crate::Cli::try_parse_from(argv) {
        Ok(cli) => crate::dispatch(&cli),
        Err(err) => {
            let code = if err.use_stderr() { 2 } else { 0 };
            let _ = err.print();
            Ok(ExitCode::from(code))
        }
    }
}

/// The full verb map, hidden entries included — sorted, one line each.
fn print_directory() {
    let cmd = crate::Cli::command();
    println!("stax advanced — every verb, including the ones `stax --help` hides\n");
    let mut entries: Vec<(String, String)> = cmd
        .get_subcommands()
        .filter(|sub| sub.get_name() != "advanced")
        .map(|sub| {
            (
                sub.get_name().to_string(),
                sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
            )
        })
        .collect();
    entries.sort();
    let width = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, about) in &entries {
        println!("  {name:width$}  {about}");
    }
    println!("\nRun any of them as `stax advanced <verb>` (or `stax <verb>` — they still work).");
}
