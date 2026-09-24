//! A command that prints nothing must make rtk print nothing — not even a blank line
//! (`runner::printable`, the "down to emitting nothing" half of the never-worse guarantee).
//! `rtk go generate` goes through `runner::run_filtered_with_exit`; a generator that writes
//! nothing on success used to come out as a lone `\n`.

#![cfg(unix)]

use std::process::Command;

fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn silent_go_generate_writes_zero_stdout_bytes() {
    if !go_available() {
        eprintln!("skipping: go not installed");
        return;
    }
    let module = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        module.path().join("go.mod"),
        "module example.com/e\n\ngo 1.21\n",
    )
    .expect("write go.mod");
    std::fs::write(
        module.path().join("a.go"),
        "package e\n\n//go:generate true\n",
    )
    .expect("write a.go");

    let out = Command::new(env!("CARGO_BIN_EXE_rtk"))
        .args(["go", "generate", "./..."])
        .current_dir(module.path())
        .env("GOWORK", "off")
        .output()
        .expect("run rtk go generate");

    assert!(
        out.status.success(),
        "rtk go generate failed ({:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        b"",
        "a silent generator must emit no bytes, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}
