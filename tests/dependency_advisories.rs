//! Lockfile floor for crates with RustSec advisories.
//!
//! The Audit workflow fails only on vulnerabilities; advisories marked
//! `informational = "unsound"` (both lru ones) are warnings and would let a
//! lockfile regression back onto an affected version through silently. This
//! test pins the first patched version of every crate fixed here, for every
//! copy of it in the lockfile.

/// (crate, first patched version, advisories it closes)
const FLOORS: &[(&str, (u64, u64, u64), &str)] = &[
    ("rustls", (0, 23, 45), "RUSTSEC-2026-0285"),
    ("lru", (0, 18, 2), "RUSTSEC-2026-0002, RUSTSEC-2026-0253"),
];

/// Every `version` of `name` recorded in a Cargo.lock.
fn locked_versions(lock: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let field = |key: &str| {
            block.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|r| r.trim().strip_prefix('='))
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
        };
        if field("name ").as_deref() == Some(name)
            && let Some(v) = field("version ")
        {
            out.push(v);
        }
    }
    out
}

/// Orderable `(major, minor, patch, is_release)`: a pre-release sorts below
/// its release; build metadata is ignored.
fn parse(v: &str) -> (u64, u64, u64, bool) {
    let v = v.split('+').next().unwrap();
    let (core, pre) = match v.split_once('-') {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap());
    (
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
        !pre,
    )
}

#[test]
fn lockfile_has_no_advisory_affected_versions() {
    let lock = include_str!("../Cargo.lock");
    for &(name, floor, advisories) in FLOORS {
        let versions = locked_versions(lock, name);
        assert!(
            !versions.is_empty(),
            "{name} is no longer in Cargo.lock; drop its floor"
        );
        for v in versions {
            assert!(
                parse(&v) >= (floor.0, floor.1, floor.2, true),
                "Cargo.lock pins {name} {v}, affected by {advisories}; first patched is {}.{}.{}",
                floor.0,
                floor.1,
                floor.2
            );
        }
    }
}

#[test]
fn locked_versions_reads_every_copy_and_parse_orders_prereleases() {
    let lock = r#"
version = 4

[[package]]
name = "lru"
version = "0.12.5"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "lru-slab"
version = "0.1.2"

[[package]]
name = "lru"
version = "0.18.2"
"#;
    assert_eq!(locked_versions(lock, "lru"), ["0.12.5", "0.18.2"]);
    let floor = (0, 18, 2, true);
    assert!(parse("0.12.5") < floor);
    assert!(parse("0.18.2") >= floor);
    assert!(parse("0.18.2-rc.1") < floor);
    assert!(parse("0.18.2+build.7") >= floor);
    assert!(parse("0.19.0-alpha.0") >= floor);
}
