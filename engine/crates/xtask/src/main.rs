// SPDX-License-Identifier: Apache-2.0
//! Repository automation for `xerj-org/xerj`, run from `engine/` via `cargo xtask <command>`.
//!
//! Commands:
//! - `projectfile-check`: validate `../projectfile.yaml` through `lsc-core` (`ProjectfileInterpolator`,
//!   `CiResolver::resolve_fixture`, `XtaskCompiler::generate`) and check version/license/branch parity.
//! - `workflow-timeout-check`: verify every GitHub Actions job declares `timeout-minutes` in `1..=90`.
//! - `toolchain-check`: verify the active Rust compiler satisfies `rust-toolchain.toml` or `projectfile.yaml`.
//! - `stages-check`: fail if `src/stages.rs` differs from `lsc-core` lowering of `../projectfile.yaml`.
//! - `stages-gen`: regenerate `src/stages.rs` from `../projectfile.yaml` via `lsc-core`.
//! - `xwork`: run the topological verification pipeline lowered in `src/stages.rs` and emit a schema-v4
//!   [`lsc_core::ValidationReceipt`] + [`lsc_core::ValidityFingerprint`].
//! - `tools`, `fmt`, `fmt-check`, `clippy` (alias `lint`), `test`, `doc-test`, `deny`, `typos`,
//!   `machete`, `ci`, and `install`: the gates of SPEC-0004 (see `gates.rs`).

mod gates;
mod projectfile;
mod stages;
mod toolchain;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

fn engine_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask lives at engine/crates/xtask")
        .to_path_buf()
}

fn repo_root() -> PathBuf {
    engine_root()
        .parent()
        .expect("engine/ lives at <repo>/engine")
        .to_path_buf()
}

fn projectfile_check(repo: &Path, engine: &Path) -> ExitCode {
    match projectfile::check(repo, engine) {
        Ok(summary) => {
            println!("projectfile-check: ok ({summary})");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("projectfile-check: {err}");
            ExitCode::FAILURE
        }
    }
}

fn workflow_timeout_check(repo: &Path) -> ExitCode {
    let script = repo.join(".github/scripts/workflow-timeout-guard.py");
    let status = Command::new("python3")
        .arg(&script)
        .current_dir(repo)
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!("workflow-timeout-check: workflow-timeout-guard.py reported errors");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("workflow-timeout-check: cannot run python3: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_repo_script(repo: &Path, label: &str, rel_script: &str, extra_args: &[String]) -> ExitCode {
    let script = repo.join(rel_script);
    let status = Command::new("bash")
        .arg(&script)
        .args(extra_args)
        .current_dir(repo)
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!("{label}: {rel_script} reported errors");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("{label}: cannot run bash {rel_script}: {err}");
            ExitCode::FAILURE
        }
    }
}

fn generated_stages(repo: &Path) -> Option<String> {
    let pf_path = repo.join("projectfile.yaml");
    let pf_yaml = match fs::read_to_string(&pf_path) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("stages: cannot read {}: {err}", pf_path.display());
            return None;
        }
    };
    match projectfile::lower_xtask_stages(&pf_yaml) {
        Ok(code) => Some(code),
        Err(err) => {
            eprintln!("stages: {err}");
            None
        }
    }
}

fn stages_check(repo: &Path, engine: &Path) -> ExitCode {
    let Some(generated) = generated_stages(repo) else {
        return ExitCode::FAILURE;
    };
    let stages_path = engine.join("crates/xtask/src/stages.rs");
    match fs::read_to_string(&stages_path) {
        Ok(current) if current == generated => {
            println!("stages-check: ok (stages.rs matches projectfile.yaml)");
            ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!(
                "stages-check: crates/xtask/src/stages.rs is out of date; run `cargo xtask stages-gen`"
            );
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("stages-check: cannot read {}: {err}", stages_path.display());
            ExitCode::FAILURE
        }
    }
}

fn stages_gen(repo: &Path, engine: &Path) -> ExitCode {
    let Some(generated) = generated_stages(repo) else {
        return ExitCode::FAILURE;
    };
    let stages_path = engine.join("crates/xtask/src/stages.rs");
    match fs::write(&stages_path, generated) {
        Ok(()) => {
            println!("stages-gen: wrote {}", stages_path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("stages-gen: cannot write {}: {err}", stages_path.display());
            ExitCode::FAILURE
        }
    }
}

fn run_cargo(engine: &Path, args: &[&str]) -> ExitCode {
    let status = Command::new(env!("CARGO"))
        .args(args)
        .current_dir(engine)
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("xwork: cannot run cargo {}: {err}", args.join(" "));
            ExitCode::FAILURE
        }
    }
}

/// How one `xwork` stage runs. Every stage `projectfile.yaml` lowers into `stages::XWORK_STAGES`
/// must map to an [`Action`] variant here.
enum Action {
    ToolchainCheck,
    ProjectfileCheck,
    StagesCheck,
    WorkflowTimeoutCheck,
    /// A cargo invocation inside `engine/`; `{jobs}` is replaced by the job limit.
    Cargo(&'static [&'static str]),
}

fn action_for(stage: &str) -> Option<Action> {
    Some(match stage {
        "toolchain-check" => Action::ToolchainCheck,
        "projectfile-check" => Action::ProjectfileCheck,
        "stages-check" => Action::StagesCheck,
        "workflow-timeout-check" => Action::WorkflowTimeoutCheck,
        "cargo-fmt" => Action::Cargo(&["fmt", "--all", "--", "--check"]),
        "xtask-clippy" => Action::Cargo(&[
            "clippy",
            "-p",
            "xtask",
            "--all-targets",
            "--locked",
            "-j",
            "{jobs}",
            "--",
            "-D",
            "warnings",
        ]),
        "xtask-test" => Action::Cargo(&["test", "-p", "xtask", "--locked", "-j", "{jobs}"]),
        _ => return None,
    })
}

fn run_stage(repo: &Path, engine: &Path, stage: &str, _args: &[&str]) -> ExitCode {
    let jobs = env::var("JOBS")
        .or_else(|_| env::var("CARGO_BUILD_JOBS"))
        .unwrap_or_else(|_| stages::DEFAULT_JOBS.to_string());
    match action_for(stage) {
        Some(Action::ToolchainCheck) => toolchain::run(engine),
        Some(Action::ProjectfileCheck) => projectfile_check(repo, engine),
        Some(Action::StagesCheck) => stages_check(repo, engine),
        Some(Action::WorkflowTimeoutCheck) => workflow_timeout_check(repo),
        Some(Action::Cargo(args)) => {
            let args: Vec<String> = args.iter().map(|a| a.replace("{jobs}", &jobs)).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run_cargo(engine, &args)
        }
        None => {
            eprintln!("xwork: projectfile stage `{stage}` has no implementation in xtask");
            ExitCode::FAILURE
        }
    }
}

fn run_xwork(repo: &Path, engine: &Path) -> ExitCode {
    println!("=== xwork: verification pipeline in projectfile.yaml order ===");
    let cache_state = if engine.join("target").is_dir() {
        lsc_core::ReceiptCacheState::Warm
    } else {
        lsc_core::ReceiptCacheState::Cold
    };
    let mut recorded: Vec<(String, gates::Outcome, Option<f64>)> =
        Vec::with_capacity(stages::XWORK_STAGES.len());
    let code = stages::run_xwork(repo, |r, stage, args| {
        let start = Instant::now();
        let stage_code = run_stage(r, engine, stage, args);
        let secs = start.elapsed().as_secs_f64();
        let outcome = if stage_code == ExitCode::SUCCESS {
            gates::Outcome::Passed
        } else {
            gates::Outcome::Failed(format!("stage `{stage}` failed"))
        };
        recorded.push((stage.to_owned(), outcome, Some(secs)));
        stage_code
    });
    for &(stage, _) in stages::XWORK_STAGES.iter().skip(recorded.len()) {
        recorded.push((stage.to_owned(), gates::Outcome::NotRun, None));
    }
    let (commit, tree_dirty) = gates::probe_git_state_at(engine);
    let receipt = gates::build_named_receipt(
        "cargo-xtask-xwork",
        &recorded,
        &commit,
        tree_dirty,
        cache_state,
        &engine.display().to_string(),
    );
    gates::write_receipt(engine, &receipt);
    code
}

/// The gate of SPEC-0004 a command name runs through `gates::run`, if any.
fn gate_of(cmd: &str) -> Option<gates::Gate> {
    match cmd {
        "lint" => Some(gates::Gate::Clippy),
        "toolchain-check" => None,
        other => gates::Gate::parse(other),
    }
}

fn usage() -> String {
    let mut names = vec![
        "projectfile-check",
        "workflow-timeout-check",
        "landing-constants-check",
        "release-notes-check",
        "mcp-schema-check",
        "stages-check",
        "stages-gen",
        "receipt-check",
        "xwork",
    ];
    names.extend(gates::Gate::ALL.iter().map(|g| g.name()));
    format!("usage: cargo xtask <{}>", names.join("|"))
}

fn main() -> ExitCode {
    let engine = engine_root();
    let repo = repo_root();
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("projectfile-check") => projectfile_check(&repo, &engine),
        Some("workflow-timeout-check") => workflow_timeout_check(&repo),
        Some("landing-constants-check") => run_repo_script(
            &repo,
            "landing-constants-check",
            ".github/scripts/landing-constants-guard.sh",
            &args[1..],
        ),
        Some("release-notes-check") => run_repo_script(
            &repo,
            "release-notes-check",
            ".github/scripts/release-notes-gate.sh",
            &args[1..],
        ),
        Some("mcp-schema-check") => run_repo_script(
            &repo,
            "mcp-schema-check",
            "scripts/mcp-schema-check.sh",
            &args[1..],
        ),
        Some("toolchain-check") => toolchain::run(&engine),
        Some("stages-check") => stages_check(&repo, &engine),
        Some("stages-gen") => stages_gen(&repo, &engine),
        Some("receipt-check") => {
            let require_anchored = args[1..].iter().any(|a| a == "--require-anchored");
            gates::receipt_check(&engine, require_anchored)
        }
        Some("xwork") => run_xwork(&repo, &engine),
        other => match other.and_then(gate_of) {
            Some(gate) => gates::run(&engine, gate),
            None => {
                eprintln!("{}", usage());
                ExitCode::from(2)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_names(stages: &[(&str, &[&str])]) -> Vec<String> {
        stages
            .iter()
            .map(|(stage, _)| (*stage).to_string())
            .collect()
    }

    #[test]
    fn repo_projectfile_passes_lsc_core_and_stages_match() {
        let repo = repo_root();
        let engine = engine_root();
        projectfile::check(&repo, &engine).expect("projectfile.yaml must pass lsc-core check");
        assert_eq!(stages_check(&repo, &engine), ExitCode::SUCCESS);
    }

    #[test]
    fn run_xwork_stops_at_the_first_failing_stage() {
        let mut calls = Vec::new();
        let code = stages::run_xwork(Path::new("."), |_, stage, _| {
            calls.push(stage.to_string());
            if calls.len() == 2 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        });
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(calls, stage_names(&stages::XWORK_STAGES[..2]));
    }

    #[test]
    fn run_xwork_runs_every_stage_in_table_order() {
        let mut calls = Vec::new();
        let code = stages::run_xwork(Path::new("."), |_, stage, _| {
            calls.push(stage.to_string());
            ExitCode::SUCCESS
        });
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(calls, stage_names(stages::XWORK_STAGES));
    }

    #[test]
    fn every_projectfile_stage_has_an_implementation() {
        for &(stage, _) in stages::XWORK_STAGES {
            assert!(
                action_for(stage).is_some(),
                "no implementation for `{stage}`"
            );
        }
    }

    #[test]
    fn unknown_stage_has_no_implementation() {
        assert!(action_for("no-such-stage").is_none());
    }

    #[test]
    fn gate_routing_covers_all_spec_0004_gates() {
        for gate in gates::Gate::ALL {
            let expected = match gate {
                gates::Gate::ToolchainCheck => None,
                other => Some(other),
            };
            assert_eq!(gate_of(gate.name()), expected, "{}", gate.name());
        }
        assert_eq!(gate_of("lint"), Some(gates::Gate::Clippy));
    }
}
