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

/// Runs for one tag never overlap, so one run's draft cleanup cannot delete
/// the release another run's smoke or publish job still holds.
#[test]
fn release_runs_for_one_tag_are_serialized() {
    let wf = workflow();
    let group = wf["concurrency"]["group"]
        .as_str()
        .expect("concurrency group");
    assert!(group.contains("github.event.inputs.tag"), "{group}");
    assert!(group.contains("github.ref_name"), "{group}");
    assert_eq!(
        wf["concurrency"]["cancel-in-progress"].as_bool(),
        Some(false),
        "a queued run must not cancel one that is mid-publish"
    );
}

/// The cleanup deletes only drafts this workflow made, the run picks out its
/// own draft by run id, and a dispatched tag lands on the commit that was
/// built and attested rather than on whatever HEAD is at publish time.
#[test]
fn draft_cleanup_spares_hand_made_drafts_and_pins_the_built_commit() {
    let wf = workflow();
    let (_, create) = step(job(&wf, "release"), "Create draft release");
    // The tag must exist, and the build jobs check that tag out, so the
    // release can only point at the built commit.
    assert!(create.contains("--verify-tag"), "{create}");
    assert!(!create.contains("--target"), "{create}");
    let checkout_ref = steps(job(&wf, "build"))
        .iter()
        .find(|s| s["name"].as_str() == Some("Checkout"))
        .and_then(|s| s["with"]["ref"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        checkout_ref.contains("github.event.inputs.tag"),
        "{checkout_ref}"
    );

    let marker = "marker='<!-- oxideclaw-release-workflow -->'";
    assert!(create.contains(marker), "{create}");
    assert!(
        create.contains("$GITHUB_RUN_ID.$GITHUB_RUN_ATTEMPT"),
        "{create}"
    );
    // The marker goes into the body the drafts() filter reads back.
    assert!(
        create.contains(r#"--notes "$marker$run_marker""#),
        "{create}"
    );
    assert!(create.contains("contains("), "{create}");
    assert!(
        create.contains(r#"for id in $(drafts "$marker")"#),
        "{create}"
    );
    assert!(create.contains(r#"id=$(drafts "$run_marker")"#), "{create}");
    // No unfiltered sweep over every draft for the tag remains.
    assert!(!create.contains("$(drafts)"), "{create}");
}

/// This file reads .github/, contrib/ and scripts/, which the crates.io
/// package leaves out, so the file itself must be left out too or
/// `cargo test` on the published source fails to compile.
#[test]
fn this_test_is_left_out_of_the_crates_io_package() {
    let manifest = include_str!("../Cargo.toml");
    let exclude = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("exclude"))
        .expect("Cargo.toml exclude list");
    for dir in [".github/*", "contrib/*", "scripts/*"] {
        assert!(exclude.contains(&format!("\"{dir}\"")), "{exclude}");
    }
    assert!(
        exclude.contains("\"tests/release_workflow.rs\""),
        "{exclude}"
    );
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

/// Every package manifest sends people to this repository, not the
/// pre-rename RustyClaw one, and describes the same product without the
/// retracted "cost-aware routing" headline (the router is opt-in).
#[test]
fn package_metadata_points_at_oxideclaw() {
    const HOME: &str = "https://github.com/ForkedInTime/OxideClaw";
    assert_eq!(env!("CARGO_PKG_HOMEPAGE"), HOME);
    assert_eq!(env!("CARGO_PKG_REPOSITORY"), HOME);

    let npm: serde_json::Value =
        serde_json::from_str(include_str!("../npm/package.json")).expect("package.json parses");
    assert_eq!(npm["homepage"].as_str(), Some(HOME));

    let brew = include_str!("../contrib/homebrew/oxideclaw.rb");
    let pkgbuild = include_str!("../contrib/aur/PKGBUILD");
    let srcinfo = include_str!("../contrib/aur/.SRCINFO");
    assert!(brew.contains(&format!("homepage \"{HOME}\"")));
    assert!(pkgbuild.contains(&format!("url=\"{HOME}\"")));
    assert!(srcinfo.contains(&format!("url = {HOME}")));

    let field = |text: &str, prefix: &str| -> String {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} line"))
            .trim_matches('"')
            .to_string()
    };
    let brew_desc = field(brew, "desc ");
    let pkgdesc = field(pkgbuild, "pkgdesc=");
    assert_eq!(field(srcinfo, "pkgdesc = "), pkgdesc, ".SRCINFO is stale");
    assert!(
        brew_desc.len() <= 80,
        "brew audit rejects desc over 80 chars"
    );
    for (file, text) in [
        ("Cargo.toml", include_str!("../Cargo.toml")),
        ("npm/package.json", include_str!("../npm/package.json")),
        ("contrib/homebrew/oxideclaw.rb", brew),
        ("contrib/aur/PKGBUILD", pkgbuild),
    ] {
        assert!(!text.contains("RustyClaw"), "{file} points at RustyClaw");
    }
    for (what, desc) in [
        ("Cargo.toml", env!("CARGO_PKG_DESCRIPTION")),
        ("npm", npm["description"].as_str().unwrap()),
        ("brew", &brew_desc),
        ("AUR", &pkgdesc),
    ] {
        assert!(!desc.contains("cost-aware"), "{what} description: {desc}");
    }
}

/// npm/install.js opened a CONNECT tunnel and then set `agent: false`, which
/// makes Node dial the host itself and ignore the tunnel: on a proxy-only
/// network the postinstall still failed (`ENOTFOUND github.com`). A local
/// proxy that closes every tunnel must see the TLS fail on the tunnel, never
/// a direct DNS lookup of the (unresolvable) host.
#[test]
fn npm_installer_downloads_through_the_proxy_tunnel() {
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !node_ok {
        return; // no node here; CI runners have it
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("proxy_check.js");
    std::fs::write(
        &script,
        r#"
const http = require("http");
const seen = [];
const proxy = http.createServer();
proxy.on("connect", (req, sock) => {
  seen.push(req.url);
  sock.write("HTTP/1.1 200 Connection Established\r\n\r\n");
  sock.end();
});
proxy.listen(0, "127.0.0.1", async () => {
  for (const k of Object.keys(process.env)) {
    if (/^(https?_proxy|no_proxy|npm_config_(https_)?proxy|npm_config_noproxy)$/i.test(k)) delete process.env[k];
  }
  process.env.HTTPS_PROXY = `http://127.0.0.1:${proxy.address().port}`;
  const { get } = require(process.argv[2]);
  let err = "none";
  try { await get("https://nonexistent-host.invalid/x"); } catch (e) { err = `${e.code} ${e.message}`; }
  console.log(JSON.stringify({ seen, err }));
  proxy.close();
});
"#,
    )
    .unwrap();
    let install = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("npm/install.js");
    let out = std::process::Command::new("node")
        .arg(&script)
        .arg(&install)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(r#""seen":["nonexistent-host.invalid:443"]"#),
        "{stdout}"
    );
    assert!(!stdout.contains("ENOTFOUND"), "direct DNS lookup: {stdout}");
    assert!(!stdout.contains(r#""err":"none""#), "{stdout}");
}
