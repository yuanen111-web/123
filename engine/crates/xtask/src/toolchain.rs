// SPDX-License-Identifier: Apache-2.0
//! `cargo xtask toolchain-check` (SPEC-0004 XT-5).
//!
//! When `rust-toolchain.toml` is present, fails unless the active `rustc` equals
//! `channel` and MSRV is not above that channel. When `rust-toolchain.toml` is
//! absent, verifies the active `rustc` satisfies `requirements.runtime.rust`
//! declared in `projectfile.yaml`.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

pub fn run(engine_root: &Path) -> ExitCode {
    match check(engine_root) {
        Ok(msg) => {
            println!("toolchain-check: {msg}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("toolchain-check: {err}");
            ExitCode::FAILURE
        }
    }
}

pub fn check(engine_root: &Path) -> Result<String, String> {
    let repo_root = engine_root.parent().unwrap_or(engine_root);
    let pf_path = if engine_root.join("projectfile.yaml").exists() {
        engine_root.join("projectfile.yaml")
    } else {
        repo_root.join("projectfile.yaml")
    };
    let pf_yaml = fs::read_to_string(&pf_path)
        .map_err(|e| format!("cannot read {}: {e}", pf_path.display()))?;
    let doc: serde_json::Value = serde_yaml::from_str(&pf_yaml)
        .map_err(|e| format!("cannot parse {}: {e}", pf_path.display()))?;
    let rust_req = doc
        .pointer("/requirements/runtime/rust")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing `requirements.runtime.rust` in projectfile.yaml".to_string())?;
    let min_rust = rust_req.trim().trim_start_matches(">=").trim();

    let out = Command::new("rustc")
        .arg("--version")
        .current_dir(engine_root)
        .output()
        .map_err(|e| format!("cannot run `rustc --version`: {e}"))?;
    if !out.status.success() {
        return Err("`rustc --version` exited non-zero".to_string());
    }
    let rustc_line = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let tc_path = if engine_root.join("rust-toolchain.toml").exists() {
        Some(engine_root.join("rust-toolchain.toml"))
    } else if repo_root.join("rust-toolchain.toml").exists() {
        Some(repo_root.join("rust-toolchain.toml"))
    } else {
        None
    };

    if let Some(tc_file) = tc_path {
        let tc = fs::read_to_string(&tc_file)
            .map_err(|e| format!("cannot read {}: {e}", tc_file.display()))?;
        let channel = toml_string(&tc, Some("toolchain"), "channel")
            .or_else(|| toml_string(&tc, None, "channel"))
            .ok_or_else(|| "no `channel` in rust-toolchain.toml".to_string())?;
        verify(&channel, &rustc_line, min_rust)?;
        Ok(format!(
            "ok ({rustc_line}; channel={channel}; minimum={min_rust})"
        ))
    } else {
        let actual = verify_minimum(&rustc_line, min_rust)?;
        Ok(format!(
            "ok ({rustc_line}; active={actual} satisfies >={min_rust})"
        ))
    }
}

/// Read a string value under an optional `[section]` header in a TOML document.
pub fn toml_string(doc: &str, section: Option<&str>, key: &str) -> Option<String> {
    let mut in_section = section.is_none();
    for raw in doc.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let header = &line[1..line.len() - 1].trim();
            in_section = section == Some(*header);
            continue;
        }
        if !in_section {
            continue;
        }
        let (k, v) = line.split_once('=')?;
        if k.trim() == key {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn parse(version: &str) -> Result<Vec<u64>, String> {
    let core = version.split('-').next().unwrap_or(version);
    core.split('.')
        .map(|p| {
            p.parse::<u64>()
                .map_err(|_| format!("not a version: `{version}`"))
        })
        .collect()
}

/// Compare the pinned channel, the `rustc --version` line and the MSRV (SPEC-0004 XT-5).
pub fn verify(channel: &str, rustc_line: &str, rust_version: &str) -> Result<(), String> {
    let actual = rustc_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("unparsable rustc line `{rustc_line}`"))?;
    if actual != channel {
        return Err(format!(
            "active rustc is {actual}, rust-toolchain.toml pins {channel}"
        ));
    }
    if parse(rust_version)? > parse(channel)? {
        return Err(format!(
            "rust-version {rust_version} is above pinned channel {channel}"
        ));
    }
    Ok(())
}

/// Verify that the active `rustc --version` satisfies the minimum `requirements.runtime.rust`.
pub fn verify_minimum(rustc_line: &str, min_rust: &str) -> Result<String, String> {
    let actual = rustc_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("unparsable rustc line `{rustc_line}`"))?;
    let min_clean = min_rust.trim().trim_start_matches(">=").trim();
    if parse(actual)? < parse(min_clean)? {
        return Err(format!(
            "active rustc {actual} is below minimum required {min_clean}"
        ));
    }
    Ok(actual.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xt5_matching_toolchain_passes() {
        assert!(verify("1.85.0", "rustc 1.85.0 (4d91de4e4 2025-02-17)", "1.85").is_ok());
    }

    #[test]
    fn xt5_channel_mismatch_fails() {
        assert!(verify("1.85.0", "rustc 1.90.0 (abc 2025-09-01)", "1.85").is_err());
    }

    #[test]
    fn xt5_msrv_above_channel_fails() {
        assert!(verify("1.85.0", "rustc 1.85.0 (abc 2025-02-17)", "1.86").is_err());
    }

    #[test]
    fn xt5_minimum_version_check() {
        assert_eq!(
            verify_minimum("rustc 1.97.1 (abc 2026-08-01)", ">=1.85"),
            Ok("1.97.1".to_string())
        );
        assert!(verify_minimum("rustc 1.84.0 (abc 2025-01-01)", ">=1.85").is_err());
    }

    #[test]
    fn xt5_reads_quoted_value() {
        assert_eq!(
            toml_string(
                "[toolchain]\nchannel = \"1.85.0\"\n",
                Some("toolchain"),
                "channel"
            )
            .as_deref(),
            Some("1.85.0")
        );
    }
}
