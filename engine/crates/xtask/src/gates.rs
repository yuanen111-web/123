// SPDX-License-Identifier: Apache-2.0
//! The xtask gates of SPEC-0004 (XT-1 to XT-10): `tools`, `toolchain-check`, `fmt`,
//! `fmt-check`, `clippy`, `test`, `doc-test`, `deny`, `typos`, `machete`, `ci` and `install`.
//!
//! Every gate runs from the Cargo workspace root (`engine/`) with `.tools/bin` first on `PATH`,
//! so the pinned tools that `tools` installs there win over any other copy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

/// One command of the gate set (XT-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Install the pinned tools that are missing or at another version (XT-3).
    Tools,
    /// Verify `rustc --version` against `rust-toolchain.toml` or `projectfile.yaml` (XT-5).
    ToolchainCheck,
    /// `cargo fmt --all`.
    Fmt,
    /// `cargo fmt --all -- --check`.
    FmtCheck,
    /// `cargo clippy`, warnings are errors.
    Clippy,
    /// nextest, or `cargo test` when nextest is missing.
    Test,
    /// `cargo test --doc`.
    DocTest,
    /// `cargo deny check`.
    Deny,
    /// `typos`.
    Typos,
    /// `cargo machete`.
    Machete,
    /// Every gate in order, stopping at the first failure (XT-7).
    Ci,
    /// `ci`, then release builds of every binary (XT-8).
    Install,
}

impl Gate {
    /// Every command, in the order `usage` lists them.
    pub const ALL: [Gate; 12] = [
        Gate::Tools,
        Gate::ToolchainCheck,
        Gate::Fmt,
        Gate::FmtCheck,
        Gate::Clippy,
        Gate::Test,
        Gate::DocTest,
        Gate::Deny,
        Gate::Typos,
        Gate::Machete,
        Gate::Ci,
        Gate::Install,
    ];

    /// The command name on the `cargo xtask` command line.
    pub fn name(self) -> &'static str {
        match self {
            Gate::Tools => "tools",
            Gate::ToolchainCheck => "toolchain-check",
            Gate::Fmt => "fmt",
            Gate::FmtCheck => "fmt-check",
            Gate::Clippy => "clippy",
            Gate::Test => "test",
            Gate::DocTest => "doc-test",
            Gate::Deny => "deny",
            Gate::Typos => "typos",
            Gate::Machete => "machete",
            Gate::Ci => "ci",
            Gate::Install => "install",
        }
    }

    /// The gate a command-line name selects, if any.
    pub fn parse(name: &str) -> Option<Gate> {
        Gate::ALL.into_iter().find(|gate| gate.name() == name)
    }
}

/// The steps `ci` runs, in order (XT-7).
pub const CI_STEPS: [Gate; 9] = [
    Gate::Tools,
    Gate::ToolchainCheck,
    Gate::FmtCheck,
    Gate::Clippy,
    Gate::Test,
    Gate::DocTest,
    Gate::Deny,
    Gate::Typos,
    Gate::Machete,
];

/// A cargo tool a gate runs, at its pinned version (XT-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tool {
    /// The crate `cargo install` builds.
    pub krate: &'static str,
    /// The command that prints the version of the copy a gate runs.
    pub probe: &'static [&'static str],
    /// The pinned version.
    pub version: &'static str,
}

/// cargo-deny.
pub const DENY: Tool = Tool {
    krate: crate::stages::TOOLS[0].0,
    probe: &["cargo", "deny", "--version"],
    version: crate::stages::TOOLS[0].1,
};

/// typos.
pub const TYPOS: Tool = Tool {
    krate: crate::stages::TOOLS[3].0,
    probe: &["typos", "--version"],
    version: crate::stages::TOOLS[3].1,
};

/// cargo-machete.
pub const MACHETE: Tool = Tool {
    krate: crate::stages::TOOLS[1].0,
    probe: &["cargo", "machete", "--version"],
    version: crate::stages::TOOLS[1].1,
};

/// cargo-nextest.
pub const NEXTEST: Tool = Tool {
    krate: crate::stages::TOOLS[2].0,
    probe: &["cargo", "nextest", "--version"],
    version: crate::stages::TOOLS[2].1,
};

/// The one table of pinned tools (XT-2).
pub const TOOLS: [Tool; 4] = [DENY, TYPOS, MACHETE, NEXTEST];

/// The Rust that `cargo install` builds the tools with (XT-3).
pub const TOOLS_RUST: &str = "1.94.1";

/// The repository commits `engine/Cargo.lock`, so cargo runs with `--locked`.
const LOCKED: bool = true;

/// True when a `--version` output names exactly the pinned version.
pub fn version_matches(output: &str, pinned: &str) -> bool {
    output
        .split_whitespace()
        .any(|word| word.strip_prefix('v').unwrap_or(word) == pinned)
}

/// The command line that installs a tool into `<root>/.tools` (XT-3).
pub fn install_command(tool: &Tool, root: &Path, jobs: &str) -> Vec<String> {
    let tools_root = root.join(".tools").to_string_lossy().into_owned();
    [
        "rustup",
        "run",
        "--install",
        TOOLS_RUST,
        "cargo",
        "install",
        "--locked",
        "--version",
        tool.version,
        "--root",
        &tools_root,
        "--force",
        "-j",
        jobs,
        tool.krate,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// `PATH` with `<root>/.tools/bin` first.
pub fn path_with_tools(root: &Path, path: Option<OsString>) -> OsString {
    let mut dirs = vec![root.join(".tools").join("bin")];
    if let Some(path) = path {
        dirs.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The variables among `names` that cargo set for the xtask binary itself, which no gate may
/// inherit (XT-6).
pub fn package_vars<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    const EXACT: [&str; 5] = [
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
    ];
    names
        .into_iter()
        .filter(|name| name.starts_with("CARGO_PKG_") || EXACT.contains(name))
        .map(str::to_string)
        .collect()
}

/// The build job count: `JOBS` when it is set and not empty, else 2 (`stages::DEFAULT_JOBS`).
pub fn jobs(env_jobs: Option<String>) -> String {
    env_jobs
        .filter(|jobs| !jobs.trim().is_empty())
        .unwrap_or_else(|| crate::stages::DEFAULT_JOBS.to_string())
}

/// The command line a gate runs (XT-6), program first. `nextest` says whether cargo-nextest
/// is there. `None` for the gates that are not one command.
pub fn command_line(gate: Gate, jobs: &str, nextest: bool) -> Option<Vec<String>> {
    let locked: &[&str] = if LOCKED { &["--locked"] } else { &[] };
    let line: Vec<&str> = match gate {
        Gate::Fmt => vec!["cargo", "fmt", "--all"],
        Gate::FmtCheck => vec!["cargo", "fmt", "--all", "--", "--check"],
        Gate::Clippy => [
            &["cargo", "clippy", "--workspace", "--all-targets"][..],
            locked,
            &["-j", jobs, "--", "-D", "warnings"],
        ]
        .concat(),
        Gate::Test if nextest => [
            &["cargo", "nextest", "run", "--workspace"][..],
            locked,
            &["--build-jobs", jobs],
        ]
        .concat(),
        Gate::Test => [&["cargo", "test", "--workspace"][..], locked, &["-j", jobs]].concat(),
        Gate::DocTest => [
            &["cargo", "test", "--doc", "--workspace"][..],
            locked,
            &["-j", jobs],
        ]
        .concat(),
        Gate::Deny => vec!["cargo", "deny", "check"],
        Gate::Typos => vec!["typos"],
        Gate::Machete => vec!["cargo", "machete"],
        Gate::Tools | Gate::ToolchainCheck | Gate::Ci | Gate::Install => return None,
    };
    Some(line.iter().map(|s| (*s).to_string()).collect())
}

/// The release build `install` runs after `ci` (XT-8).
pub fn install_build_line(jobs: &str) -> Vec<String> {
    let locked: &[&str] = if LOCKED { &["--locked"] } else { &[] };
    [
        &["cargo", "build", "--release"][..],
        locked,
        &["--workspace", "--exclude", "xtask", "--bins", "-j", jobs],
    ]
    .concat()
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// How one step of `ci` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The step ran and passed.
    Passed,
    /// The step ran and failed, with the reason.
    Failed(String),
    /// An earlier step failed, so this one did not run.
    NotRun,
}

/// One line of the `ci` summary.
#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    /// The step.
    pub gate: Gate,
    /// How it ended.
    pub outcome: Outcome,
    /// Seconds it took, when it ran.
    pub secs: Option<f64>,
}

/// Run `steps` in order with `run`, and stop at the first step that fails (XT-7).
pub fn run_steps<F>(steps: &[Gate], run: F) -> Vec<StepResult>
where
    F: FnMut(Gate) -> Result<(), String>,
{
    let mut run = run;
    let mut failed = false;
    steps
        .iter()
        .map(|&gate| {
            if failed {
                return StepResult {
                    gate,
                    outcome: Outcome::NotRun,
                    secs: None,
                };
            }
            let start = Instant::now();
            let outcome = match run(gate) {
                Ok(()) => Outcome::Passed,
                Err(why) => {
                    failed = true;
                    Outcome::Failed(why)
                }
            };
            StepResult {
                gate,
                outcome,
                secs: Some(start.elapsed().as_secs_f64()),
            }
        })
        .collect()
}

/// The summary line for one step (XT-7).
pub fn summary_line(label: &str, result: &StepResult) -> String {
    let head = format!("{label}: {:<15}", result.gate.name());
    let secs = result
        .secs
        .map(|s| format!(" {s:>6.1}s"))
        .unwrap_or_default();
    match &result.outcome {
        Outcome::Passed => format!("{head} {:<6}{secs}", "ok"),
        Outcome::Failed(why) => {
            let why = why.lines().next().unwrap_or("");
            format!("{head} {:<6}{secs}  {why}", "FAILED")
        }
        Outcome::NotRun => format!("{head} not run"),
    }
}

/// What a gate needs at run time.
struct Ctx {
    root: PathBuf,
    path: OsString,
    jobs: String,
}

impl Ctx {
    fn new(root: &Path) -> Ctx {
        Ctx {
            root: root.to_path_buf(),
            path: path_with_tools(root, std::env::var_os("PATH")),
            jobs: jobs(std::env::var("JOBS").ok()),
        }
    }

    /// A command that runs from the root with `.tools/bin` first on `PATH`, without the
    /// xtask binary's own package variables (XT-6).
    fn command(&self, line: &[String]) -> Command {
        let mut cmd = Command::new(&line[0]);
        cmd.args(&line[1..])
            .current_dir(&self.root)
            .env("PATH", &self.path);
        let names: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        for name in package_vars(names.iter().map(String::as_str)) {
            cmd.env_remove(name);
        }
        cmd
    }

    fn status(&self, line: &[String]) -> Result<(), String> {
        let shown = line.join(" ");
        println!("xtask: running `{shown}`");
        let status = self
            .command(line)
            .status()
            .map_err(|e| format!("cannot run `{shown}`: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("`{shown}` exited with {status}"))
        }
    }

    /// The `--version` output of the copy of `tool` a gate would run, if it runs.
    fn probe(&self, tool: &Tool) -> Option<String> {
        let line: Vec<String> = tool.probe.iter().map(|s| (*s).to_string()).collect();
        let out = self.command(&line).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Install each pinned tool that is missing or at another version (XT-3, XT-4).
fn tools(ctx: &Ctx) -> Result<(), String> {
    ensure_tools(
        &TOOLS,
        |tool| ctx.probe(tool),
        |tool| ctx.status(&install_command(tool, &ctx.root, &ctx.jobs)),
    )
}

/// Install each tool that `probe` reports missing or at another version, then probe it
/// again (XT-3, XT-4).
fn ensure_tools(
    tools: &[Tool],
    mut probe: impl FnMut(&Tool) -> Option<String>,
    mut install: impl FnMut(&Tool) -> Result<(), String>,
) -> Result<(), String> {
    for tool in tools {
        match probe(tool) {
            Some(found) if version_matches(&found, tool.version) => {
                println!("tools: {} {} ok", tool.krate, tool.version);
                continue;
            }
            Some(found) => println!(
                "tools: {} reports `{}`, installing {}",
                tool.krate,
                found.lines().next().unwrap_or(""),
                tool.version
            ),
            None => println!("tools: {} missing, installing {}", tool.krate, tool.version),
        }
        install(tool)?;
        match probe(tool) {
            Some(found) if version_matches(&found, tool.version) => {
                println!("tools: {} {} installed", tool.krate, tool.version);
            }
            found => {
                return Err(format!(
                    "{} still reports `{}` after installing {}; another copy is ahead of .tools/bin",
                    tool.krate,
                    found
                        .as_deref()
                        .and_then(|f| f.lines().next())
                        .unwrap_or("nothing"),
                    tool.version
                ));
            }
        }
    }
    Ok(())
}

/// Run one gate that is a single step.
fn step(ctx: &Ctx, gate: Gate) -> Result<(), String> {
    match gate {
        Gate::Tools => tools(ctx),
        Gate::ToolchainCheck => {
            if crate::toolchain::run(&ctx.root) == ExitCode::SUCCESS {
                Ok(())
            } else {
                Err("toolchain-check failed".to_string())
            }
        }
        Gate::Ci | Gate::Install => Err(format!("{} is not a single step", gate.name())),
        _ => {
            let nextest = gate == Gate::Test && ctx.probe(&NEXTEST).is_some();
            let line = command_line(gate, &ctx.jobs, nextest)
                .ok_or_else(|| format!("{} has no command line", gate.name()))?;
            ctx.status(&line)
        }
    }
}

/// Assemble a schema-v4 [`lsc_core::ValidationReceipt`] from `ci` step outcomes (XT-10).
pub fn build_receipt(
    results: &[StepResult],
    commit: &str,
    tree_dirty: bool,
    cache_state: lsc_core::ReceiptCacheState,
    cwd: &str,
) -> lsc_core::ValidationReceipt {
    let named_steps: Vec<(String, Outcome, Option<f64>)> = results
        .iter()
        .map(|r| (r.gate.name().to_owned(), r.outcome.clone(), r.secs))
        .collect();
    build_named_receipt(
        "cargo-xtask-ci",
        &named_steps,
        commit,
        tree_dirty,
        cache_state,
        cwd,
    )
}

/// Assemble a schema-v4 [`lsc_core::ValidationReceipt`] from arbitrary named step outcomes (`ci` or `xwork`).
pub fn build_named_receipt(
    producer: &str,
    steps: &[(String, Outcome, Option<f64>)],
    commit: &str,
    tree_dirty: bool,
    cache_state: lsc_core::ReceiptCacheState,
    cwd: &str,
) -> lsc_core::ValidationReceipt {
    let commit_anchored = !commit.is_empty() && commit != "uncommitted" && !tree_dirty;
    let strength = if commit_anchored {
        lsc_core::ValidityStrength::Exact
    } else {
        lsc_core::ValidityStrength::Advisory
    };
    let tools_pin_digest = lsc_core::compute_blake3_hash(
        TOOLS
            .iter()
            .map(|t| format!("{}={}", t.krate, t.version))
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    let pipeline_digest = lsc_core::compute_blake3_hash(
        steps
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    let fingerprint = lsc_core::ValidityFingerprint::compute(
        strength,
        vec![
            lsc_core::ValidityInput::new(lsc_core::ValidityInputKind::SourceTree, "HEAD", commit),
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::Toolchain,
                "tools-rust",
                TOOLS_RUST,
            ),
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::Lockfile,
                "xtask-tools",
                tools_pin_digest,
            ),
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::VerifyPipeline,
                "pipeline-steps",
                pipeline_digest,
            ),
        ],
    );

    let mut gates = Vec::with_capacity(steps.len());
    let mut failures = 0_u64;
    let mut total_secs = 0.0_f64;

    for (name, outcome, secs_opt) in steps {
        let secs = secs_opt.unwrap_or(0.0);
        total_secs += secs;
        let (result, exit_code) = match outcome {
            Outcome::Passed => (lsc_core::ReceiptResult::Pass, 0),
            Outcome::Failed(_) => {
                failures += 1;
                (lsc_core::ReceiptResult::Fail, 1)
            }
            Outcome::NotRun => (lsc_core::ReceiptResult::NotRun, -1),
        };
        gates.push(lsc_core::ValidationGateReceipt {
            name: name.clone(),
            result,
            exit_code,
            real_seconds: secs,
            executed_tests: None,
            passed_tests: None,
            filtered_tests: None,
        });
    }

    let all_passed = failures == 0
        && !gates.is_empty()
        && gates
            .iter()
            .all(|g| g.result == lsc_core::ReceiptResult::Pass);

    lsc_core::ValidationReceipt {
        schema_version: lsc_core::VALIDATION_RECEIPT_SCHEMA_VERSION,
        producer: producer.to_owned(),
        repo: "xerj".to_owned(),
        started_at: "1970-01-01T00:00:00Z".to_owned(),
        finished_at: "1970-01-01T00:00:00Z".to_owned(),
        host: std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_owned()),
        slot: "worktree".to_owned(),
        cwd: cwd.to_owned(),
        cache_state,
        commit: commit.to_owned(),
        commit_anchored,
        tree_dirty,
        validity_fingerprint: Some(fingerprint),
        result: if all_passed {
            lsc_core::ReceiptResult::Pass
        } else {
            lsc_core::ReceiptResult::Fail
        },
        exit_code: if all_passed { 0 } else { 1 },
        executed_tests: 0,
        passed_tests: 0,
        filtered_tests: 0,
        checks: gates.len() as u64,
        failures,
        real_seconds: total_secs,
        gates,
    }
}

/// Probe git `HEAD` commit SHA and working-tree dirtiness from `root`.
pub fn probe_git_state_at(root: &Path) -> (String, bool) {
    let ctx = Ctx::new(root);
    probe_git_state(&ctx)
}

fn probe_git_state(ctx: &Ctx) -> (String, bool) {
    let head = ctx
        .command(&["git".to_owned(), "rev-parse".to_owned(), "HEAD".to_owned()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "uncommitted".to_owned());
    let dirty = ctx
        .command(&[
            "git".to_owned(),
            "status".to_owned(),
            "--porcelain".to_owned(),
            "--untracked-files=normal".to_owned(),
        ])
        .output()
        .map(|o| !o.status.success() || !o.stdout.is_empty())
        .unwrap_or(true);
    (head, dirty)
}

/// Resolve the validation receipt path (`XTASK_RECEIPT_PATH` or `<root>/.tools/validation-receipt.json`).
pub fn receipt_path(root: &Path) -> PathBuf {
    std::env::var_os("XTASK_RECEIPT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".tools").join("validation-receipt.json"))
}

/// Write a [`lsc_core::ValidationReceipt`] to `XTASK_RECEIPT_PATH` or `<root>/.tools/validation-receipt.json`.
pub fn write_receipt(root: &Path, receipt: &lsc_core::ValidationReceipt) {
    let path = receipt_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(receipt) {
        let _ = std::fs::write(&path, json);
    }
}

/// Verify a [`lsc_core::ValidationReceipt`] against the active git state and recomputed BLAKE3 fingerprint.
pub fn verify_receipt_against(
    receipt: &lsc_core::ValidationReceipt,
    current_commit: &str,
    current_dirty: bool,
    require_anchored: bool,
) -> Result<String, String> {
    receipt
        .validate_fail_closed(false)
        .map_err(|e| format!("invalid validation receipt: {}", e.join("; ")))?;
    if receipt.result != lsc_core::ReceiptResult::Pass
        || receipt.exit_code != 0
        || receipt.failures != 0
    {
        return Err(format!(
            "receipt did not pass (result={:?}, exit_code={}, failures={})",
            receipt.result, receipt.exit_code, receipt.failures
        ));
    }
    if receipt.commit != current_commit {
        return Err(format!(
            "stale receipt: receipt commit `{}` does not match HEAD `{current_commit}`",
            receipt.commit
        ));
    }
    if require_anchored && (current_dirty || !receipt.commit_anchored || receipt.tree_dirty) {
        return Err(format!(
            "receipt is not commit-anchored on a clean tree (commit_anchored={}, receipt.tree_dirty={}, current_dirty={current_dirty})",
            receipt.commit_anchored, receipt.tree_dirty
        ));
    }
    let strength = if receipt.commit_anchored {
        lsc_core::ValidityStrength::Exact
    } else {
        lsc_core::ValidityStrength::Advisory
    };
    let tools_pin_digest = lsc_core::compute_blake3_hash(
        TOOLS
            .iter()
            .map(|t| format!("{}={}", t.krate, t.version))
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    let pipeline_digest = lsc_core::compute_blake3_hash(
        receipt
            .gates
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    let expected_fp = lsc_core::ValidityFingerprint::compute(
        strength,
        vec![
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::SourceTree,
                "HEAD",
                &receipt.commit,
            ),
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::Toolchain,
                "tools-rust",
                TOOLS_RUST,
            ),
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::Lockfile,
                "xtask-tools",
                tools_pin_digest,
            ),
            lsc_core::ValidityInput::new(
                lsc_core::ValidityInputKind::VerifyPipeline,
                "pipeline-steps",
                pipeline_digest,
            ),
        ],
    );
    let Some(actual_fp) = &receipt.validity_fingerprint else {
        return Err("receipt is missing validity_fingerprint".to_owned());
    };
    if actual_fp.composite_digest != expected_fp.composite_digest {
        return Err(format!(
            "receipt validity_fingerprint digest mismatch: got `{}`, expected `{}`",
            actual_fp.composite_digest, expected_fp.composite_digest
        ));
    }
    Ok(format!(
        "schema=v{}, producer={}, commit={}, anchored={}, checks={}, digest={}",
        receipt.schema_version,
        receipt.producer,
        receipt.commit,
        receipt.commit_anchored,
        receipt.checks,
        actual_fp.composite_digest
    ))
}

/// Inspect and verify the on-disk validation receipt at `XTASK_RECEIPT_PATH` or `<root>/.tools/validation-receipt.json`.
pub fn receipt_check(root: &Path, require_anchored: bool) -> ExitCode {
    let path = receipt_path(root);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("receipt-check: cannot read {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let receipt: lsc_core::ValidationReceipt = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("receipt-check: invalid JSON in {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let (commit, dirty) = probe_git_state_at(root);
    match verify_receipt_against(&receipt, &commit, dirty, require_anchored) {
        Ok(summary) => {
            println!("receipt-check: ok ({summary})");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("receipt-check: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Run `ci`'s steps, print the summary, write the validation receipt, and say whether every step passed.
fn ci(ctx: &Ctx, label: &str) -> bool {
    let cache_state = if ctx.root.join("target").is_dir() {
        lsc_core::ReceiptCacheState::Warm
    } else {
        lsc_core::ReceiptCacheState::Cold
    };
    let results = run_steps(&CI_STEPS, |gate| {
        println!("==> {label}: {}", gate.name());
        step(ctx, gate)
    });
    for result in &results {
        println!("{}", summary_line(label, result));
    }
    let (commit, tree_dirty) = probe_git_state(ctx);
    let receipt = build_receipt(
        &results,
        &commit,
        tree_dirty,
        cache_state,
        &ctx.root.display().to_string(),
    );
    write_receipt(&ctx.root, &receipt);
    results.iter().all(|r| r.outcome == Outcome::Passed)
}

/// Run one command of the gate set.
pub fn run(root: &Path, gate: Gate) -> ExitCode {
    let ctx = Ctx::new(root);
    let ok = match gate {
        Gate::Ci => ci(&ctx, "xtask ci"),
        Gate::Install => {
            ci(&ctx, "xtask install")
                && match ctx.status(&install_build_line(&ctx.jobs)) {
                    Ok(()) => {
                        println!("xtask install: release build ok");
                        true
                    }
                    Err(err) => {
                        eprintln!("xtask install: release build FAILED: {err}");
                        false
                    }
                }
        }
        _ => match step(&ctx, gate) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("{}: {err}", gate.name());
                false
            }
        },
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn xt1_every_command_parses_by_its_name() {
        let names: Vec<&str> = Gate::ALL.iter().map(|g| g.name()).collect();
        assert_eq!(
            names,
            [
                "tools",
                "toolchain-check",
                "fmt",
                "fmt-check",
                "clippy",
                "test",
                "doc-test",
                "deny",
                "typos",
                "machete",
                "ci",
                "install"
            ]
        );
        for gate in Gate::ALL {
            assert_eq!(Gate::parse(gate.name()), Some(gate));
        }
    }

    #[test]
    fn xt1_other_names_are_not_gates() {
        for name in [
            "",
            "projectfile-check",
            "workflow-timeout-check",
            "xwork",
            "stages-check",
            "stages-gen",
            "CI",
            "fmt-check ",
            "nextest",
        ] {
            assert_eq!(Gate::parse(name), None, "{name:?}");
        }
    }

    #[test]
    fn xt2_one_table_of_four_pinned_tools_matches_stages_table() {
        let table: Vec<(&str, &str)> = TOOLS.iter().map(|t| (t.krate, t.version)).collect();
        assert_eq!(
            table,
            [
                ("cargo-deny", "0.19.9"),
                ("typos-cli", "1.49.0"),
                ("cargo-machete", "0.9.1"),
                ("cargo-nextest", "0.9.104")
            ]
        );
        for tool in TOOLS {
            assert!(
                crate::stages::TOOLS
                    .iter()
                    .any(|&(krate, ver, _)| krate == tool.krate && ver == tool.version),
                "tool {:?} missing from lowered stages::TOOLS",
                tool.krate
            );
        }
    }

    #[test]
    fn xt3_version_lines() {
        assert!(version_matches("cargo-deny 0.19.9", "0.19.9"));
        assert!(version_matches("typos-cli 1.49.0\n", "1.49.0"));
        assert!(version_matches("0.9.1", "0.9.1"));
        assert!(version_matches("cargo-machete v0.9.1", "0.9.1"));
        assert!(version_matches(
            "cargo-nextest 0.9.104 (abc123 2025-09-15)\nrelease: 0.9.104\nhost: x86_64",
            "0.9.104"
        ));
        assert!(!version_matches("0.9.10", "0.9.1"));
        assert!(!version_matches("0.9.2", "0.9.1"));
        assert!(!version_matches("cargo-deny 0.20.2", "0.19.9"));
        assert!(!version_matches("typos-cli 1.49", "1.49.0"));
        assert!(!version_matches("", "0.9.1"));
    }

    #[test]
    fn xt3_install_builds_the_pin_into_dot_tools_with_rust_1_94_1() {
        let line = install_command(&TYPOS, Path::new("/r"), "2");
        assert_eq!(
            line,
            strings(&[
                "rustup",
                "run",
                "--install",
                "1.94.1",
                "cargo",
                "install",
                "--locked",
                "--version",
                "1.49.0",
                "--root",
                "/r/.tools",
                "--force",
                "-j",
                "2",
                "typos-cli"
            ])
        );
    }

    #[test]
    fn xt3_tools_come_first_on_path() {
        let path = path_with_tools(Path::new("/r"), Some(OsString::from("/usr/bin:/bin")));
        let parts: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(
            parts,
            [
                PathBuf::from("/r/.tools/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin")
            ]
        );
        let path = path_with_tools(Path::new("/r"), None);
        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            [PathBuf::from("/r/.tools/bin")]
        );
    }

    #[test]
    fn xt4_a_tool_at_the_pin_is_left_alone() {
        let mut installed = Vec::new();
        let result = ensure_tools(
            &[DENY],
            |tool| Some(format!("{} {}", tool.krate, tool.version)),
            |tool| {
                installed.push(tool.krate);
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert!(installed.is_empty());
    }

    #[test]
    fn xt4_a_missing_tool_is_installed_and_probed_again() {
        let mut probes = 0;
        let mut installed = Vec::new();
        let result = ensure_tools(
            &[DENY],
            |tool| {
                probes += 1;
                (probes > 1).then(|| format!("{} {}", tool.krate, tool.version))
            },
            |tool| {
                installed.push(tool.krate);
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(installed, ["cargo-deny"]);
        assert_eq!(probes, 2);
    }

    #[test]
    fn xt4_another_copy_ahead_of_dot_tools_is_an_error() {
        let result = ensure_tools(
            &[DENY],
            |_| Some("cargo-deny 0.18.0".to_string()),
            |_| Ok(()),
        );
        let err = result.expect_err("a wrong version after install must fail");
        assert!(err.contains("ahead of .tools/bin"), "{err}");
        assert!(err.contains("cargo-deny 0.18.0"), "{err}");
    }

    #[test]
    fn xt6_gates_do_not_inherit_the_xtask_package_variables() {
        let names = [
            "CARGO",
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_JOBS",
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION",
            "CARGO_PKG_AUTHORS",
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_CRATE_NAME",
            "CARGO_BIN_NAME",
            "CARGO_PRIMARY_PACKAGE",
            "PATH",
            "RUSTUP_TOOLCHAIN",
        ];
        assert_eq!(
            package_vars(names),
            strings(&[
                "CARGO_PKG_NAME",
                "CARGO_PKG_VERSION",
                "CARGO_PKG_AUTHORS",
                "CARGO_MANIFEST_DIR",
                "CARGO_MANIFEST_PATH",
                "CARGO_CRATE_NAME",
                "CARGO_BIN_NAME",
                "CARGO_PRIMARY_PACKAGE"
            ])
        );
    }

    #[test]
    fn xt6_spawned_commands_drop_the_package_variables() {
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let cmd = Ctx::new(Path::new(".")).command(&strings(&["true"]));
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in ["CARGO_PKG_NAME", "CARGO_MANIFEST_DIR"] {
            assert!(
                removed.iter().any(|r| r == name),
                "{name} is not removed: {removed:?}"
            );
        }
    }

    #[test]
    fn xt6_jobs_default_to_two() {
        assert_eq!(jobs(None), "2");
        assert_eq!(jobs(Some(String::new())), "2");
        assert_eq!(jobs(Some("10".to_string())), "10");
    }

    #[test]
    fn xt6_command_lines() {
        let line = |gate| command_line(gate, "2", true).unwrap();
        assert_eq!(line(Gate::Fmt), strings(&["cargo", "fmt", "--all"]));
        assert_eq!(
            line(Gate::FmtCheck),
            strings(&["cargo", "fmt", "--all", "--", "--check"])
        );
        assert_eq!(
            line(Gate::Clippy),
            strings(&[
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "-j",
                "2",
                "--",
                "-D",
                "warnings"
            ])
        );
        assert_eq!(
            line(Gate::Test),
            strings(&[
                "cargo",
                "nextest",
                "run",
                "--workspace",
                "--locked",
                "--build-jobs",
                "2"
            ])
        );
        assert_eq!(
            line(Gate::DocTest),
            strings(&[
                "cargo",
                "test",
                "--doc",
                "--workspace",
                "--locked",
                "-j",
                "2"
            ])
        );
        assert_eq!(line(Gate::Deny), strings(&["cargo", "deny", "check"]));
        assert_eq!(line(Gate::Typos), strings(&["typos"]));
        assert_eq!(line(Gate::Machete), strings(&["cargo", "machete"]));
    }

    #[test]
    fn xt6_test_falls_back_to_cargo_test_without_nextest() {
        assert_eq!(
            command_line(Gate::Test, "2", false),
            Some(strings(&[
                "cargo",
                "test",
                "--workspace",
                "--locked",
                "-j",
                "2"
            ]))
        );
    }

    #[test]
    fn xt7_ci_runs_the_nine_steps_in_contract_order() {
        let names: Vec<&str> = CI_STEPS.iter().map(|g| g.name()).collect();
        assert_eq!(
            names,
            [
                "tools",
                "toolchain-check",
                "fmt-check",
                "clippy",
                "test",
                "doc-test",
                "deny",
                "typos",
                "machete"
            ]
        );
    }

    #[test]
    fn xt7_ci_stops_at_the_first_failure() {
        let mut ran = Vec::new();
        let results = run_steps(&CI_STEPS, |gate| {
            ran.push(gate);
            if gate == Gate::Clippy {
                Err("clippy found 3 warnings".to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(
            ran,
            [
                Gate::Tools,
                Gate::ToolchainCheck,
                Gate::FmtCheck,
                Gate::Clippy
            ]
        );
        let outcomes: Vec<&Outcome> = results.iter().map(|r| &r.outcome).collect();
        assert_eq!(results.len(), 9);
        assert_eq!(outcomes[..3], [&Outcome::Passed; 3]);
        assert_eq!(
            outcomes[3],
            &Outcome::Failed("clippy found 3 warnings".to_string())
        );
        assert!(outcomes[4..].iter().all(|o| **o == Outcome::NotRun));
    }

    #[test]
    fn xt8_install_builds_every_binary_in_release_with_locked() {
        assert_eq!(
            install_build_line("2"),
            strings(&[
                "cargo",
                "build",
                "--release",
                "--locked",
                "--workspace",
                "--exclude",
                "xtask",
                "--bins",
                "-j",
                "2"
            ])
        );
    }

    #[test]
    fn xt10_ci_builds_anchored_validation_receipt_and_fingerprint() {
        let results = run_steps(&CI_STEPS, |_| Ok(()));
        let receipt = build_receipt(
            &results,
            "f5b95636",
            false,
            lsc_core::ReceiptCacheState::Warm,
            "/repo/engine",
        );
        assert_eq!(receipt.schema_version, 4);
        assert_eq!(receipt.repo, "xerj");
        assert!(receipt.commit_anchored);
        assert!(!receipt.tree_dirty);
        assert_eq!(receipt.result, lsc_core::ReceiptResult::Pass);
        assert_eq!(receipt.checks, 9);
        assert_eq!(receipt.failures, 0);
        assert!(receipt.validate_fail_closed(false).is_ok());

        let dirty = build_receipt(
            &results,
            "f5b95636",
            true,
            lsc_core::ReceiptCacheState::Warm,
            "/repo/engine",
        );
        assert!(!dirty.commit_anchored);
        assert_eq!(
            dirty.validity_fingerprint.as_ref().unwrap().strength,
            lsc_core::ValidityStrength::Advisory
        );
        assert!(dirty.validate_fail_closed(false).is_ok());

        let ok = verify_receipt_against(&receipt, "f5b95636", false, true)
            .expect("clean anchored receipt must pass verification");
        assert!(ok.contains("anchored=true"), "{ok}");

        let stale = verify_receipt_against(&receipt, "deadbeef", false, false)
            .expect_err("stale commit must fail verification");
        assert!(stale.contains("stale receipt"), "{stale}");

        let unanchored = verify_receipt_against(&dirty, "f5b95636", true, true)
            .expect_err("dirty receipt must fail --require-anchored");
        assert!(unanchored.contains("not commit-anchored"), "{unanchored}");

        let mut tampered = receipt.clone();
        tampered
            .validity_fingerprint
            .as_mut()
            .unwrap()
            .composite_digest = "blake3:0000".to_owned();
        let mismatch = verify_receipt_against(&tampered, "f5b95636", false, false)
            .expect_err("tampered composite digest must fail verification");
        assert!(
            mismatch.contains("composite_digest") || mismatch.contains("digest mismatch"),
            "{mismatch}"
        );
    }
}
