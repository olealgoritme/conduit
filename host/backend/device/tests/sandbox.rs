// The sandbox, checked from inside one.
//
// `sandbox::enter` cannot be undone and must run single-threaded, so the
// checks run in a process of their own: `nvgpu-sandbox-selftest` enters the
// sandbox and tries each thing it is supposed to refuse, and each thing the
// backend still needs. Its exit status is the test.

#[test]
fn the_sandbox_refuses_what_it_says_it_refuses() {
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_nvgpu-sandbox-selftest"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run the selftest binary");
    let pid = child.id();
    let out = child.wait_with_output().expect("selftest output");
    // The stand-in broker socket it could not remove from inside its sandbox.
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("nvgpu-broker-{pid}")));
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
