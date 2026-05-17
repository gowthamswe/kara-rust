//! Subcommand argument parsing.
//!
//! Houses `parse_args` (the top-level subcommand dispatcher),
//! `parse_<subcmd>_command` per-subcommand helpers, `parse_file_args` /
//! `parse_file_args_optional` (shared `--output=...` / `--sequential`
//! recognizer), and `parse_profiles_arg` (the build profile flag).

use crate::scaffold::Template;
use std::process;

use super::help::{has_help_flag, print_subcommand_help};
use super::{Command, OutputMode, QueryKind};

pub fn parse_args(args: &[String]) -> Command {
    if args.len() < 2 {
        return Command::Help;
    }

    let subcmd = args[1].as_str();

    // Top-level help / version short-circuit.
    match subcmd {
        "help" | "--help" | "-h" => return Command::Help,
        "version" | "--version" | "-V" => return Command::Version,
        _ => {}
    }

    // Subcommand-scoped `--help` / `-h`: print help for that subcommand and
    // exit 0 before its arg parser rejects the flag as "unknown".
    if has_help_flag(&args[2..]) {
        print_subcommand_help(subcmd);
        process::exit(0);
    }

    match subcmd {
        "run" => {
            // Check for --example NAME before the generic file-arg parser.
            if args.iter().skip(2).any(|a| a == "--example") {
                parse_run_example_command(args)
            } else {
                let p = parse_file_args(args, 2);
                Command::Run {
                    file: p.file,
                    output: p.output,
                    sequential: p.sequential,
                }
            }
        }
        "check" => parse_check_command(args),
        "build" => parse_build_command(args),
        "query" => parse_query_command(args),
        "fmt" => {
            if args.len() < 3 {
                eprintln!("error: karac fmt requires a file argument");
                process::exit(1);
            }
            Command::Fmt {
                file: args[2].clone(),
            }
        }
        "fix" => parse_fix_command(args),
        "init" => parse_init_command(args),
        "test" => parse_test_command(args),
        "repl" => parse_repl_command(args),
        "doc" => Command::Doc,
        "clean" => parse_clean_command(args),
        "install" => parse_install_command(args),
        "vendor" => parse_vendor_command(args),
        "explain" => parse_explain_command(args),
        // Bare file path: treat as `karac run <file>`
        other if other.ends_with(".kara") => {
            let p = parse_file_args(args, 1);
            Command::Run {
                file: p.file,
                output: p.output,
                sequential: p.sequential,
            }
        }
        other => {
            eprintln!("error: unknown command '{other}'");
            eprintln!("Run 'karac help' for usage.");
            process::exit(1);
        }
    }
}

struct ParsedFileArgs {
    file: String,
    output: OutputMode,
    sequential: bool,
}

struct ParsedOptionalFileArgs {
    file: Option<String>,
    output: OutputMode,
    sequential: bool,
}

fn parse_file_args_optional(args: &[String], file_idx: usize) -> ParsedOptionalFileArgs {
    let mut file = None;
    let mut output = OutputMode::Text;
    let mut sequential = false;
    let mut i = file_idx;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--output=json" {
            output = OutputMode::Json;
        } else if arg == "--output=jsonl" {
            output = OutputMode::Jsonl;
        } else if arg == "--sequential" {
            sequential = true;
        } else if arg.starts_with("--output=") {
            eprintln!(
                "error: unknown output mode '{}'. Use json or jsonl.",
                arg.strip_prefix("--output=").unwrap_or(arg)
            );
            process::exit(1);
        } else if arg.starts_with('-') {
            eprintln!("error: unknown flag '{arg}'");
            process::exit(1);
        } else if file.is_none() {
            file = Some(arg.clone());
        } else {
            eprintln!("error: unexpected argument '{arg}'");
            process::exit(1);
        }
        i += 1;
    }
    ParsedOptionalFileArgs {
        file,
        output,
        sequential,
    }
}

fn parse_file_args(args: &[String], file_idx: usize) -> ParsedFileArgs {
    let p = parse_file_args_optional(args, file_idx);
    match p.file {
        Some(f) => ParsedFileArgs {
            file: f,
            output: p.output,
            sequential: p.sequential,
        },
        None => {
            eprintln!("error: missing file argument");
            process::exit(1);
        }
    }
}

fn parse_check_command(args: &[String]) -> Command {
    let mut file: Option<String> = None;
    let mut output = OutputMode::Text;
    let mut profiles: Option<Vec<crate::manifest::CompileProfile>> = None;
    let mut concurrency_report = false;
    let mut i = 2usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--output=json" {
            output = OutputMode::Json;
        } else if arg == "--output=jsonl" {
            output = OutputMode::Jsonl;
        } else if let Some(rest) = arg.strip_prefix("--profiles=") {
            profiles = Some(parse_profiles_arg(rest));
        } else if arg == "--concurrency-report" {
            concurrency_report = true;
        } else if arg.starts_with("--output=") {
            eprintln!(
                "error: unknown output mode '{}'. Use json or jsonl.",
                arg.strip_prefix("--output=").unwrap_or(arg)
            );
            process::exit(1);
        } else if arg.starts_with('-') {
            eprintln!("error: unknown flag '{arg}'");
            process::exit(1);
        } else if file.is_none() {
            file = Some(arg.clone());
        } else {
            eprintln!("error: unexpected argument '{arg}'");
            process::exit(1);
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("error: missing file argument");
        process::exit(1);
    };
    Command::Check {
        file,
        output,
        profiles,
        concurrency_report,
    }
}

/// Parser for `karac build [<file>] [--output=...] [--concurrency-report]`.
/// Mirrors `parse_check_command`'s shape so future build-only flags slot in
/// next to `--concurrency-report` without churning the bare-`build` /
/// project-mode-`build` distinction below.
fn parse_build_command(args: &[String]) -> Command {
    let mut file: Option<String> = None;
    let mut output = OutputMode::Text;
    let mut concurrency_report = false;
    let mut offline = false;
    let mut i = 2usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--output=json" {
            output = OutputMode::Json;
        } else if arg == "--output=jsonl" {
            output = OutputMode::Jsonl;
        } else if arg == "--concurrency-report" {
            concurrency_report = true;
        } else if arg == "--offline" {
            offline = true;
        } else if arg.starts_with("--output=") {
            eprintln!(
                "error: unknown output mode '{}'. Use json or jsonl.",
                arg.strip_prefix("--output=").unwrap_or(arg)
            );
            process::exit(1);
        } else if arg.starts_with('-') {
            eprintln!("error: unknown flag '{arg}'");
            process::exit(1);
        } else if file.is_none() {
            file = Some(arg.clone());
        } else {
            eprintln!("error: unexpected argument '{arg}'");
            process::exit(1);
        }
        i += 1;
    }
    match file {
        Some(f) => Command::Build {
            file: f,
            output,
            concurrency_report,
            offline,
        },
        None => Command::BuildProject { output, offline },
    }
}

fn parse_clean_command(args: &[String]) -> Command {
    let mut global = false;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--global" => global = true,
            flag if flag.starts_with('-') => {
                eprintln!("error: unknown flag '{flag}' for `karac clean`");
                process::exit(1);
            }
            other => {
                eprintln!("error: unexpected argument '{other}' for `karac clean`");
                process::exit(1);
            }
        }
    }
    Command::Clean { global }
}

fn parse_install_command(args: &[String]) -> Command {
    // `karac install <bin-spec>` takes a single positional. The spec is
    // re-parsed downstream against the manifest dependency-entry shape
    // (`path = "..."` / `git = "..."` / bare registry reference). Here
    // we just lift it off the command line.
    let mut spec: Option<String> = None;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            flag if flag.starts_with("--") => {
                eprintln!("error: unknown flag '{flag}' for `karac install`");
                process::exit(1);
            }
            other => {
                if spec.is_some() {
                    eprintln!("error: `karac install` takes exactly one <bin-spec> argument");
                    process::exit(1);
                }
                spec = Some(other.to_string());
            }
        }
    }
    let Some(spec) = spec else {
        eprintln!("error: `karac install` requires a <bin-spec> argument");
        eprintln!("       e.g. `karac install path=./tools/my-tool`");
        process::exit(1);
    };
    Command::Install { spec }
}

fn parse_vendor_command(args: &[String]) -> Command {
    if let Some(arg) = args.get(2) {
        eprintln!("error: `karac vendor` takes no arguments (got '{arg}')");
        process::exit(1);
    }
    Command::Vendor
}

/// Parse the comma-separated profile list passed to `--profiles=...`.
/// `all` expands to every known profile in canonical order. Empty entries
/// (e.g. trailing comma) are rejected. Unknown profile names abort with a
/// hint listing the supported set so a typo doesn't silently fall through.
fn parse_profiles_arg(spec: &str) -> Vec<crate::manifest::CompileProfile> {
    use crate::manifest::CompileProfile;
    if spec.is_empty() {
        eprintln!("error: --profiles requires at least one profile name (e.g. --profiles=all or --profiles=embedded,kernel)");
        process::exit(1);
    }
    if spec == "all" {
        return vec![
            CompileProfile::Default,
            CompileProfile::Embedded,
            CompileProfile::Kernel,
        ];
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in spec.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            eprintln!("error: --profiles entry must not be empty (got '{spec}')");
            process::exit(1);
        }
        let Some(p) = CompileProfile::parse(name) else {
            eprintln!(
                "error: unknown profile '{name}'. Supported: default, embedded, kernel, all."
            );
            process::exit(1);
        };
        // De-duplicate while preserving the user's order — running the same
        // profile twice would otherwise produce identical grouped diagnostics.
        if seen.insert(p) {
            out.push(p);
        }
    }
    out
}

fn parse_run_example_command(args: &[String]) -> Command {
    let mut name: Option<String> = None;
    let mut output = OutputMode::Text;
    let mut sequential = false;
    let mut i = 2usize;
    while i < args.len() {
        match args[i].as_str() {
            "--example" => {
                i += 1;
                name = Some(args.get(i).cloned().unwrap_or_else(|| {
                    eprintln!("error: --example requires a name argument");
                    process::exit(1);
                }));
            }
            "--output=json" => output = OutputMode::Json,
            "--output=jsonl" => output = OutputMode::Jsonl,
            "--sequential" => sequential = true,
            flag if flag.starts_with("--output=") => {
                eprintln!(
                    "error: unknown output mode '{}'. Use json or jsonl.",
                    flag.strip_prefix("--output=").unwrap_or(flag)
                );
                process::exit(1);
            }
            flag if flag.starts_with('-') => {
                eprintln!("error: unknown flag '{flag}' for `karac run --example`");
                process::exit(1);
            }
            other => {
                eprintln!("error: unexpected argument '{other}' (use --example NAME to specify which example to run)");
                process::exit(1);
            }
        }
        i += 1;
    }
    let name = name.unwrap_or_else(|| {
        eprintln!("error: --example requires a name argument");
        process::exit(1);
    });
    Command::RunExample {
        name,
        output,
        sequential,
    }
}

fn parse_test_command(args: &[String]) -> Command {
    let mut filter: Option<String> = None;
    let mut all = false;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--all" => all = true,
            flag if flag.starts_with("--") => {
                eprintln!("error: unknown flag '{flag}' for `karac test`");
                process::exit(1);
            }
            substring => {
                if filter.is_some() {
                    eprintln!("error: `karac test` takes at most one positional substring filter");
                    process::exit(1);
                }
                filter = Some(substring.to_string());
            }
        }
    }
    Command::Test { filter, all }
}

fn parse_repl_command(args: &[String]) -> Command {
    let mut auto_clone = false;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--auto-clone" => auto_clone = true,
            flag if flag.starts_with("--") || flag.starts_with('-') => {
                eprintln!("error: unknown flag '{flag}' for `karac repl`");
                process::exit(1);
            }
            other => {
                eprintln!("error: `karac repl` takes no positional arguments (got '{other}')");
                process::exit(1);
            }
        }
    }
    Command::Repl { auto_clone }
}

fn parse_init_command(args: &[String]) -> Command {
    let mut directory: Option<String> = None;
    let mut bin = false;
    let mut lib = false;
    let mut force = false;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--bin" => bin = true,
            "--lib" => lib = true,
            "--force" => force = true,
            flag if flag.starts_with("--") => {
                eprintln!("error: unknown flag '{flag}' for `karac init`");
                process::exit(1);
            }
            name => {
                if directory.is_some() {
                    eprintln!("error: `karac init` takes at most one positional argument");
                    process::exit(1);
                }
                directory = Some(name.to_string());
            }
        }
    }
    if bin && lib {
        eprintln!("error: --bin and --lib are mutually exclusive");
        process::exit(1);
    }
    // `--bin` is the default per CR-36 T1. Absence of both flags is the
    // common case — scaffold a binary project.
    let template = if lib { Template::Lib } else { Template::Bin };
    Command::Init {
        directory,
        template,
        force,
    }
}

fn parse_fix_command(args: &[String]) -> Command {
    let mut file: Option<String> = None;
    let mut dry_run = false;
    for arg in args.iter().skip(2) {
        match arg.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            flag if flag.starts_with("--") => {
                eprintln!("error: unknown flag '{flag}' for `karac fix`");
                process::exit(1);
            }
            other => {
                if file.is_some() {
                    eprintln!("error: `karac fix` takes at most one file argument");
                    process::exit(1);
                }
                file = Some(other.to_string());
            }
        }
    }
    let Some(file) = file else {
        eprintln!("error: missing file argument for `karac fix`");
        process::exit(1);
    };
    Command::Fix { file, dry_run }
}

/// Parser for `karac explain --concept=<name>`. The concept is required;
/// missing or unrecognized flag forms abort with a usage diagnostic that
/// names the canonical form. The concept *name* itself is validated at
/// render time so the supported-set message lives in one place
/// (`src/cli/explain.rs`).
fn parse_explain_command(args: &[String]) -> Command {
    let mut concept: Option<String> = None;
    for arg in args.iter().skip(2) {
        if let Some(rest) = arg.strip_prefix("--concept=") {
            if rest.is_empty() {
                eprintln!("error: --concept requires a name (e.g. --concept=closures)");
                process::exit(1);
            }
            if concept.is_some() {
                eprintln!("error: --concept may only be specified once");
                process::exit(1);
            }
            concept = Some(rest.to_string());
        } else if arg.starts_with('-') {
            eprintln!("error: unknown flag '{arg}' for `karac explain`");
            process::exit(1);
        } else {
            eprintln!(
                "error: unexpected argument '{arg}' (use --concept=NAME to select a concept)"
            );
            process::exit(1);
        }
    }
    let Some(concept) = concept else {
        eprintln!("error: `karac explain` requires --concept=NAME (e.g. --concept=closures)");
        process::exit(1);
    };
    Command::Explain { concept }
}

fn parse_query_command(args: &[String]) -> Command {
    if args.len() < 4 {
        eprintln!("Usage: karac query <effects|ownership|concurrency|cost-summary> <target>");
        eprintln!("       <target> is `<file>.<function>` for the per-function kinds,");
        eprintln!("                or `<file>` for cost-summary.");
        process::exit(1);
    }
    let kind = match args[2].as_str() {
        "effects" => QueryKind::Effects,
        "ownership" => QueryKind::Ownership,
        "concurrency" => QueryKind::Concurrency,
        "cost-summary" => QueryKind::CostSummary,
        other => {
            eprintln!(
                "error: unknown query kind '{other}'. Use 'effects', 'ownership', 'concurrency', or 'cost-summary'."
            );
            process::exit(1);
        }
    };
    let target = &args[3];
    // cost-summary takes a bare file path — there is no per-function form.
    // The other kinds parse `file.function` via rsplit (multi-dot file paths
    // are fine since Kāra identifiers cannot contain `.`).
    let (file, function) = match kind {
        QueryKind::CostSummary => (target.clone(), String::new()),
        _ => match target.rsplit_once('.') {
            Some((f, func)) => (f.to_string(), func.to_string()),
            None => {
                eprintln!("error: query target must be <file>.<function>, got '{target}'");
                process::exit(1);
            }
        },
    };
    Command::Query {
        kind,
        file,
        function,
    }
}
