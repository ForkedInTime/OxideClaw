//! `scripts/bench.py --first-frame` times a TUI's first frame behind a
//! pseudo-terminal. Its `--self-test` runs it against dummy TUIs: a banner
//! drawn 100 ms after a cursor-position query, text that trickles in, a
//! ready marker, a hang, a missing key and a sign-in screen. Python and a
//! Unix pty are needed; without them the test says so and passes.

use std::process::Command;

#[test]
fn bench_first_frame_self_test() {
    if cfg!(not(unix)) {
        eprintln!("skipping: scripts/bench.py --first-frame needs a Unix pseudo-terminal");
        return;
    }
    if !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping: python3 not found");
        return;
    }
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/bench.py");
    let out = Command::new("python3")
        // -B: no __pycache__ in scripts/.
        .args(["-B", script, "--self-test"])
        .output()
        .expect("run scripts/bench.py --self-test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("self-test passed"),
        "bench.py --self-test failed:\n{stdout}\n{stderr}"
    );
}
