// SPDX-License-Identifier: Apache-2.0
//! `cargo xtask projectfile-check` (SPEC-0004 XT-11).
//!
//! Validates `projectfile.yaml` through `lsc-core` (`ProjectfileInterpolator`,
//! `CiResolver::resolve_fixture`, and `XtaskCompiler::generate`) and verifies
//! identity, `main` branch hygiene, pinned tools completeness, and version/license
//! parity between `projectfile.yaml` and `engine/Cargo.toml`.

use std::fs;
use std::path::Path;

use lsc_core::{CiResolver, ProjectfileInterpolator, XtaskCompiler, XtaskTargetOptions};
use serde_json::Value;

use crate::toolchain::toml_string;

/// Read `projectfile.yaml` from `repo_root` and `Cargo.toml` from `engine_root`
/// and validate them via [`verify`].
pub fn check(repo_root: &Path, engine_root: &Path) -> Result<String, String> {
    let pf_yaml = fs::read_to_string(repo_root.join("projectfile.yaml"))
        .map_err(|e| format!("read projectfile.yaml: {e}"))?;
    let cargo_toml = fs::read_to_string(engine_root.join("Cargo.toml"))
        .map_err(|e| format!("read engine/Cargo.toml: {e}"))?;
    verify(&pf_yaml, &cargo_toml)
}

/// Pure, zero-I/O verification of `projectfile.yaml` and `engine/Cargo.toml`.
pub fn verify(pf_yaml: &str, cargo_toml: &str) -> Result<String, String> {
    let doc: Value =
        serde_yaml::from_str(pf_yaml).map_err(|e| format!("cannot parse projectfile.yaml: {e}"))?;
    let interp = ProjectfileInterpolator::new(&doc);

    let schema = interp_scalar(&interp, "$schema")?;
    if schema != "https://projectfile.org/schema/v1.json" {
        return Err(format!(
            "expected $schema 'https://projectfile.org/schema/v1.json', found '{schema}'"
        ));
    }

    let kind = interp_scalar(&interp, "kind")?;
    if kind != "SoftwareSourceCode" {
        return Err(format!(
            "expected kind 'SoftwareSourceCode', found '{kind}'"
        ));
    }

    let namespace = interp_scalar(&interp, "identity.namespace")?;
    if namespace != "org.xerj" {
        return Err(format!(
            "unexpected identity.namespace '{namespace}' (expected 'org.xerj')"
        ));
    }

    let name = interp_scalar(&interp, "identity.name")?;
    if name != "xerj" {
        return Err(format!(
            "unexpected identity.name '{name}' (expected 'xerj')"
        ));
    }

    let version = interp_scalar(&interp, "identity.version")?;
    let spdx = interp_scalar(&interp, "license.spdx")?;
    let rust_req = interp_scalar(&interp, "requirements.runtime.rust")?;
    if !rust_req.starts_with(">=") {
        return Err(format!(
            "requirements.runtime.rust must be a '>=' bound, found '{rust_req}'"
        ));
    }

    let origin_url = interp_scalar(&interp, "repositories[role=origin].url")?;
    if origin_url != "https://github.com/xerj-org/xerj.git" {
        return Err(format!(
            "unexpected origin repository url '{origin_url}' (expected 'https://github.com/xerj-org/xerj.git')"
        ));
    }

    let origin_branch = interp_scalar(&interp, "repositories[role=origin].branch")?;
    if origin_branch != "main" {
        return Err(format!(
            "origin repository branch must be 'main' (GitHub Flow), found '{origin_branch}'"
        ));
    }

    let branches = interp.query_address("repositories[].branch", None);
    if branches.is_empty() {
        return Err("projectfile.yaml missing repositories[].branch".to_string());
    }
    for branch_val in branches {
        let b = branch_val.as_str().unwrap_or("");
        if b != "main" {
            return Err(format!(
                "repository branch must be 'main' (non-main branch '{b}' is forbidden)"
            ));
        }
    }

    let cargo_version = toml_string(cargo_toml, Some("workspace.package"), "version")
        .ok_or_else(|| "missing `version` in engine/Cargo.toml [workspace.package]".to_string())?;
    if version != cargo_version {
        return Err(format!(
            "version mismatch: projectfile.yaml has '{version}' but engine/Cargo.toml has '{cargo_version}'"
        ));
    }

    let cargo_license = toml_string(cargo_toml, Some("workspace.package"), "license")
        .ok_or_else(|| "missing `license` in engine/Cargo.toml [workspace.package]".to_string())?;
    if spdx != cargo_license {
        return Err(format!(
            "license mismatch: projectfile.yaml has '{spdx}' but engine/Cargo.toml has '{cargo_license}'"
        ));
    }

    let incomplete_tools = XtaskCompiler::incomplete_tools(&doc);
    if !incomplete_tools.is_empty() {
        return Err(format!(
            "incomplete gg.unsigned.ls.tools rows: {}",
            incomplete_tools.join(", ")
        ));
    }

    let ci_section = doc
        .pointer("/org/projectfile/ci")
        .ok_or_else(|| "missing `org.projectfile.ci` section in projectfile.yaml".to_string())?;
    let resolved = CiResolver::resolve_fixture(ci_section);
    if let Some(code) = &resolved.reject {
        return Err(format!(
            "org.projectfile.ci DAG rejected by lsc-core: {code}"
        ));
    }
    if resolved.runs.is_empty() {
        return Err("org.projectfile.ci resolved to zero executable stages".to_string());
    }
    for required_stage in [
        "toolchain-check",
        "projectfile-check",
        "stages-check",
        "workflow-timeout-check",
        "cargo-fmt",
        "xtask-clippy",
        "xtask-test",
    ] {
        if !resolved.runs.iter().any(|s| s == required_stage) {
            return Err(format!(
                "org.projectfile.ci resolved plan missing required stage '{required_stage}'"
            ));
        }
    }

    let lowered = lower_xtask_stages(pf_yaml)?;
    if !lowered.contains("pub const XWORK_STAGES") || !lowered.contains("pub fn run_xwork") {
        return Err(
            "lsc-core XtaskCompiler::generate did not emit XWORK_STAGES runner".to_string(),
        );
    }

    Ok(format!(
        "{namespace}:{name} v{version}; branch=main; ci stages={}",
        resolved.runs.len()
    ))
}

/// Lower `projectfile.yaml` to the `stages.rs` dispatch runner via `lsc-core`.
pub fn lower_xtask_stages(pf_yaml: &str) -> Result<String, String> {
    XtaskCompiler::generate(
        pf_yaml,
        None,
        &XtaskTargetOptions {
            dispatch: true,
            ..XtaskTargetOptions::default()
        },
    )
    .map_err(|e| format!("lsc-core XtaskCompiler::generate failed: {e}"))
}

fn interp_scalar(interp: &ProjectfileInterpolator<'_>, address: &str) -> Result<String, String> {
    let token = format!("${{{address}}}");
    let rendered = interp.interpolate_string(&token, None, 0);
    if rendered == token || rendered.trim().is_empty() {
        return Err(format!(
            "missing or unresolved `{address}` in projectfile.yaml"
        ));
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PROJECTFILE: &str = r#"
$schema: https://projectfile.org/schema/v1.json
kind: SoftwareSourceCode

identity:
  namespace: org.xerj
  name: xerj
  version: "1.0.0-rc.95"

repositories:
  - url: "https://github.com/xerj-org/xerj.git"
    type: git
    branch: main
    role: origin

license:
  spdx: Apache-2.0
  file: LICENSE

requirements:
  runtime:
    rust: ">=1.85"

org:
  projectfile:
    ci:
      nodes:
        toolchain:
          needs:
            toolchain-check: true
        hygiene:
          needs:
            toolchain: true
            projectfile-check: true
            stages-check: true
            workflow-timeout-check: true
        formatted:
          needs:
            hygiene: true
            cargo-fmt: true
        linted:
          needs:
            formatted: true
            xtask-clippy: true
        check:
          goal: true
          needs:
            linted: true
            xtask-test: true

gg:
  unsigned:
    ls:
      tools:
        deny:
          crate: cargo-deny
          version: "0.19.9"
          bin: cargo-deny
"#;

    const VALID_CARGO_TOML: &str = r#"
[workspace]
resolver = "2"

[workspace.package]
version = "1.0.0-rc.95"
edition = "2021"
license = "Apache-2.0"
"#;

    #[test]
    fn xt11_valid_projectfile_passes_lsc_core_verification() {
        let summary = verify(VALID_PROJECTFILE, VALID_CARGO_TOML);
        assert_eq!(
            summary,
            Ok("org.xerj:xerj v1.0.0-rc.95; branch=main; ci stages=7".to_string())
        );
    }

    #[test]
    fn xt11_rejects_non_main_branch() {
        let dev_pf = VALID_PROJECTFILE.replace("branch: main", "branch: development");
        let err = verify(&dev_pf, VALID_CARGO_TOML).unwrap_err();
        assert!(
            err.contains("main") && err.contains("development"),
            "expected error naming main and development, got: {err}"
        );
    }

    #[test]
    fn xt11_rejects_ci_cycle_via_lsc_core() {
        let cyclic_pf = VALID_PROJECTFILE.replace(
            "toolchain:\n          needs:\n            toolchain-check: true",
            "toolchain:\n          needs:\n            check: true\n            toolchain-check: true",
        );
        let err = verify(&cyclic_pf, VALID_CARGO_TOML).unwrap_err();
        assert!(
            err.contains("cycle"),
            "expected lsc-core cycle rejection, got: {err}"
        );
    }

    #[test]
    fn xt11_rejects_version_or_license_drift() {
        let drifted_cargo =
            VALID_CARGO_TOML.replace("version = \"1.0.0-rc.95\"", "version = \"1.0.0-rc.96\"");
        let cargo_err = verify(VALID_PROJECTFILE, &drifted_cargo).unwrap_err();
        assert!(cargo_err.contains("version mismatch"), "{cargo_err}");

        let drifted_license =
            VALID_CARGO_TOML.replace("license = \"Apache-2.0\"", "license = \"MIT\"");
        let license_err = verify(VALID_PROJECTFILE, &drifted_license).unwrap_err();
        assert!(license_err.contains("license mismatch"), "{license_err}");
    }

    #[test]
    fn xt11_rejects_incomplete_tool_pins() {
        let broken_tools = VALID_PROJECTFILE.replace("bin: cargo-deny", "");
        let err = verify(&broken_tools, VALID_CARGO_TOML).unwrap_err();
        assert!(err.contains("incomplete"), "{err}");
    }
}
