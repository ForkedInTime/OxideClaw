//! Release pipeline invariants that only show up when a tag is pushed, which
//! is too late: SHA256SUMS and provenance attestations cover every binary, the
//! release stays a draft until its own assets have started on the glibc floor
//! and on Alpine, and every installer asks for an asset name the workflow
//! actually produces.

use serde_yaml::Value;
use std::collections::BTreeSet;

const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

fn workflow() -> Value {
    serde_yaml::from_str(WORKFLOW).expect("release.yml parses")
}

fn job<'a>(wf: &'a Value, name: &str) -> &'a Value {
    &wf["jobs"][name]
}

fn steps(job: &Value) -> &Vec<Value> {
    job["steps"].as_sequence().expect("job has steps")
}

/// Index and `run` script of the first step whose name contains `name`.
fn step(job: &Value, name: &str) -> (usize, String) {
    steps(job)
        .iter()
        .enumerate()
        .find(|(_, s)| s["name"].as_str().is_some_and(|n| n.contains(name)))
        .map(|(i, s)| (i, s["run"].as_str().unwrap_or_default().to_string()))
        .unwrap_or_else(|| panic!("no step named like {name:?}"))
}

fn needs(job: &Value) -> BTreeSet<String> {
    match &job["needs"] {
        Value::String(s) => [s.clone()].into(),
        Value::Sequence(v) => v.iter().map(|n| n.as_str().unwrap().to_string()).collect(),
        _ => BTreeSet::new(),
    }
}

fn permission(job: &Value, scope: &str) -> Option<String> {
    job["permissions"][scope].as_str().map(str::to_string)
}

/// The binary names the build matrix produces, e.g. `oxideclaw-linux-x64`.
fn built_assets(wf: &Value) -> BTreeSet<String> {
    job(wf, "build")["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("build matrix")
        .iter()
        .map(|e| e["artifact"].as_str().expect("artifact").to_string())
        .collect()
}

#[test]
fn build_matrix_produces_the_expected_oxideclaw_assets() {
    let want: BTreeSet<String> = [
        "oxideclaw-linux-x64",
        "oxideclaw-linux-arm64",
        "oxideclaw-linux-x64-musl",
        "oxideclaw-macos-x64",
        "oxideclaw-macos-arm64",
        "oxideclaw-windows-x64.exe",
    ]
    .map(String::from)
    .into();
    assert_eq!(built_assets(&workflow()), want);
}

#[test]
fn release_publishes_sha256sums_over_every_asset_before_upload() {
    let wf = workflow();
    let release = job(&wf, "release");
    let (sums_at, sums) = step(release, "SHA256SUMS");
    let sums_step = &steps(release)[sums_at];
    assert_eq!(sums_step["working-directory"].as_str(), Some("release"));
    // Every file in release/ (binaries, sidecars, manifest), written after
    // the glob expands so SHA256SUMS never lists itself.
    assert!(sums.contains("sha256sum -- *"), "{sums}");
    assert!(sums.contains("> SHA256SUMS"), "{sums}");
    // Sidecars that disagree with their binary abort the release.
    assert!(
        sums.contains("*.sha256") && sums.contains("exit 1"),
        "{sums}"
    );

    let (manifest_at, _) = step(release, "Create manifest");
    let (create_at, create) = step(release, "Create draft release");
    assert!(manifest_at < sums_at, "manifest.json must be summed too");
    assert!(sums_at < create_at, "SHA256SUMS must exist before upload");
    assert!(create.contains("release/*"), "{create}");
}

#[test]
fn every_binary_gets_a_build_provenance_attestation() {
    let wf = workflow();
    let release = job(&wf, "release");
    let attest = steps(release)
        .iter()
        .find(|s| {
            s["uses"]
                .as_str()
                .is_some_and(|u| u.starts_with("actions/attest-build-provenance@"))
        })
        .expect("attest-build-provenance step");
    let subjects: BTreeSet<String> = attest["with"]["subject-path"]
        .as_str()
        .expect("subject-path")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.strip_prefix("release/")
                .expect("release/ path")
                .to_string()
        })
        .collect();
    assert_eq!(subjects, built_assets(&wf));

    assert_eq!(permission(release, "id-token").as_deref(), Some("write"));
    assert_eq!(
        permission(release, "attestations").as_deref(),
        Some("write")
    );
    // Third-party build toolchains never hold an OIDC or write token.
    let build = job(&wf, "build");
    assert_eq!(permission(build, "id-token"), None);
    assert_eq!(wf["permissions"]["contents"].as_str(), Some("read"));
    assert_eq!(wf["permissions"]["id-token"].as_str(), None);
}

#[test]
fn smoke_matrix_runs_release_assets_on_the_glibc_floor_and_alpine() {
    let wf = workflow();
    let smoke = job(&wf, "smoke");
    assert_eq!(needs(smoke), ["release".to_string()].into());

    let runs: BTreeSet<(String, String)> = smoke["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("smoke matrix")
        .iter()
        .map(|e| {
            (
                e["image"].as_str().unwrap().to_string(),
                e["asset"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let gnu = "oxideclaw-linux-x64";
    let want: BTreeSet<(String, String)> = [
        ("ubuntu:22.04", gnu),
        ("debian:12", gnu),
        ("rockylinux:9", gnu),
        ("debian:10", gnu),    // glibc 2.28
        ("rockylinux:8", gnu), // glibc 2.28
        ("alpine:3", "oxideclaw-linux-x64-musl"),
    ]
    .map(|(i, a)| (i.to_string(), a.to_string()))
    .into();
    assert_eq!(runs, want);
    assert_eq!(smoke["strategy"]["fail-fast"].as_bool(), Some(false));

    // The assets come from the draft release itself and are checked
    // against its SHA256SUMS before they run.
    let (_, download) = step(smoke, "Download");
    assert!(download.contains("releases/$RELEASE_ID"), "{download}");
    assert!(download.contains("SHA256SUMS"), "{download}");
    assert!(download.contains("sha256sum -c"), "{download}");
    let (_, run) = step(smoke, "--version");
    assert!(run.contains("docker run"), "{run}");
    assert!(run.contains("--version"), "{run}");
    assert!(run.contains("exit 1"), "{run}");
}

#[test]
fn release_stays_a_draft_until_every_smoke_run_passes() {
    let wf = workflow();
    let (_, create) = step(job(&wf, "release"), "Create draft release");
    assert!(create.contains("--draft"), "{create}");

    let publish = job(&wf, "publish");
    assert!(needs(publish).contains("smoke"));
    assert!(
        publish["if"].is_null(),
        "publish must not run past a failure"
    );
    let (_, flip) = step(publish, "Publish");
    assert!(flip.contains("draft=false"), "{flip}");

    // The Docker image downloads the public musl asset, so it may only be
    // dispatched once the release is published.
    let dispatching: Vec<&str> = wf["jobs"]
        .as_mapping()
        .unwrap()
        .iter()
        .filter(|(_, j)| {
            steps(j)
                .iter()
                .any(|s| s["run"].as_str().unwrap_or("").contains("docker.yml"))
        })
        .map(|(n, _)| n.as_str().unwrap())
        .collect();
    assert_eq!(dispatching, ["publish"]);
    assert_eq!(permission(publish, "actions").as_deref(), Some("write"));
}

/// Every place that downloads a release asset names one the build matrix
/// produces, and none still asks for the pre-rename `rustyclaw-*` names.
#[test]
fn installers_ask_for_assets_the_workflow_produces() {
    let assets = built_assets(&workflow());
    let consumers = [
        ("install.sh", include_str!("../install.sh")),
        ("npm/install.js", include_str!("../npm/install.js")),
        ("Dockerfile", include_str!("../Dockerfile")),
        ("src/main.rs", include_str!("../src/main.rs")),
        (
            "contrib/homebrew/oxideclaw.rb",
            include_str!("../contrib/homebrew/oxideclaw.rb"),
        ),
        (
            "contrib/aur/PKGBUILD",
            include_str!("../contrib/aur/PKGBUILD"),
        ),
        (
            "scripts/update-packaging.sh",
            include_str!("../scripts/update-packaging.sh"),
        ),
        ("README.md", include_str!("../README.md")),
        (".github/workflows/release.yml", WORKFLOW),
    ];
    let name = regex::Regex::new(r"oxideclaw-(?:linux|macos|windows)[A-Za-z0-9.\-]*").unwrap();
    for (file, text) in consumers {
        assert!(
            !text.contains("rustyclaw-"),
            "{file} uses a rustyclaw-* asset name"
        );
        for m in name.find_iter(text) {
            // Templates such as `oxideclaw-linux-${arch}` are built at run time.
            if text[m.end()..].starts_with(['$', '{']) {
                continue;
            }
            let asset = m.as_str().trim_end_matches('.').trim_end_matches(".sha256");
            assert!(assets.contains(asset), "{file} names unknown asset {asset}");
        }
    }
    // install.sh, npm and the Dockerfile verify against the per-asset sidecar.
    let wf = workflow();
    let upload = steps(job(&wf, "build"))
        .iter()
        .find(|s| s["name"].as_str() == Some("Upload artifact"))
        .map(|s| s["with"]["path"].as_str().unwrap().to_string())
        .unwrap();
    assert!(upload.contains("${{ matrix.artifact }}.sha256"), "{upload}");
}
