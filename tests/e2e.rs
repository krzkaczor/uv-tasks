//! End-to-end tests against the real `uv`, using the workspace checked in at
//! `tests/fixtures/acme`: a root whose `test` task fans out to every child
//! package. Requires `uv` on PATH; the first run downloads pytest and hatchling.

use std::fs;
use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use tempfile::TempDir;

/// Copy `tests/fixtures/acme` (a production-like workspace: root `acme` whose
/// `test` fans out to `acme-core` <- `acme-api` <- `acme-worker`) into a temp
/// dir, since the tests mutate it and uv writes `.venv`/`uv.lock` into it.
fn fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acme");
    copy_dir(&src, tmp.path()).unwrap();
    tmp
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// `ut` with the real `uv` on PATH. The `ut` binary itself is deliberately
/// not added: ut prepends its own directory to the PATH of spawned tasks, so
/// the root's `ut run -w ...` resolves to this same build.
fn ut(dir: &Path) -> Command {
    let system_path = std::env::var("PATH").unwrap_or_default();
    let has_uv = std::env::split_paths(&system_path).any(|p| p.join("uv").is_file());
    assert!(
        has_uv,
        "the e2e tests need `uv` on PATH (https://docs.astral.sh/uv/getting-started/installation/)"
    );
    let mut cmd = Command::cargo_bin("ut").unwrap();
    cmd.current_dir(dir)
        .env("PATH", system_path)
        .env("NO_COLOR", "1")
        .timeout(Duration::from_secs(600));
    cmd
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn root_test_task_runs_tests_in_every_package() {
    let ws = fixture();
    let root = ws.path();

    let out = ut(root).args(["run", "test"]).assert().success();
    let stdout = utf8(&out.get_output().stdout);
    let stderr = utf8(&out.get_output().stderr);
    assert!(stderr.contains("$ ut run -w test"), "stderr: {stderr}");
    // The outer ut syncs once; the fanned-out inner ut skips via UT_SYNCED.
    assert_eq!(
        stderr.matches("$ uv sync --all-packages").count(),
        1,
        "stderr: {stderr}"
    );
    for pkg in ["acme-core", "acme-api", "acme-worker"] {
        let passed = stdout
            .lines()
            .any(|l| l.starts_with(&format!("{pkg} ")) && l.contains("| ") && l.contains("passed"));
        assert!(
            passed,
            "no pytest pass line for {pkg}\nstdout: {stdout}\nstderr: {stderr}"
        );
    }
    assert_eq!(
        stdout.lines().filter(|l| l.contains("passed")).count(),
        3,
        "stdout: {stdout}"
    );
    // The root is never a -w target, so it doesn't re-enter itself.
    assert!(!stdout.contains("acme |"), "stdout: {stdout}");
    // Linear dependency chain => deterministic start order.
    assert_eq!(
        fs::read_to_string(root.join("order.log")).unwrap(),
        "acme-core\nacme-api\nacme-worker\n"
    );

    // A failing child propagates through the root task and skips dependents.
    fs::write(
        root.join("packages/acme-api/tests/test_acme_api.py"),
        "def test_broken():\n    assert False\n",
    )
    .unwrap();
    fs::remove_file(root.join("order.log")).unwrap();
    let out = ut(root).args(["run", "test"]).assert().code(1);
    let stdout = utf8(&out.get_output().stdout);
    let stderr = utf8(&out.get_output().stderr);
    assert!(
        stderr.contains("task failed in: acme-api"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("skipped: acme-worker"), "stderr: {stderr}");
    assert!(stdout.contains("acme-core "), "stdout: {stdout}");
    assert!(!stdout.contains("acme-worker "), "stdout: {stdout}");
}

#[test]
fn root_check_sequence_runs_lint_then_test() {
    let ws = fixture();

    let out = ut(ws.path()).args(["run", "check"]).assert().success();
    let stderr = utf8(&out.get_output().stderr);
    let lint = stderr.find("$ ut run -w lint").expect("lint step echoed");
    let test = stderr.find("$ ut run -w test").expect("test step echoed");
    assert!(lint < test, "stderr: {stderr}");
}

#[test]
fn list_shows_realistic_workspace() {
    let ws = fixture();

    let out = ut(&ws.path().join("services/acme-worker"))
        .arg("list")
        .assert()
        .success();
    let stdout = utf8(&out.get_output().stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "stdout: {stdout}");
    assert!(
        lines[0].starts_with("acme ") && lines[0].ends_with("check, test"),
        "stdout: {stdout}"
    );
    assert!(lines[0].contains("  .  "), "stdout: {stdout}");
    assert!(lines[1].starts_with("acme-core "), "stdout: {stdout}");
    assert!(lines[2].starts_with("acme-api "), "stdout: {stdout}");
    assert!(lines[3].starts_with("acme-worker "), "stdout: {stdout}");
}
