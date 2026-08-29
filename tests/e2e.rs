//! End-to-end tests against the real `uv`: a production-like workspace whose
//! root `test` task fans out to every child package. Requires `uv` on PATH;
//! the first run downloads pytest and hatchling.

use std::fs;
use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use tempfile::TempDir;

/// acme/                       root app: `test = "ut run -w test"`
///   packages/acme-core        no deps
///   packages/acme-api         depends on acme-core
///   services/acme-worker      depends on acme-api
fn fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("pyproject.toml"),
        r#"
[project]
name = "acme"
version = "0.1.0"
requires-python = ">=3.9"

[tool.uv]
package = false

[tool.uv.workspace]
members = ["packages/*", "services/*"]

[tool.ut.tasks]
test = "ut run -w test"
check = ["ut run -w lint", "ut run -w test"]
"#,
    )
    .unwrap();

    package(
        root,
        "packages/acme-core",
        "acme-core",
        None,
        "def add(a, b):\n    return a + b\n",
        "from acme_core import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n",
    );
    package(
        root,
        "packages/acme-api",
        "acme-api",
        Some("acme-core"),
        "from acme_core import add\n\n\ndef handle(a, b):\n    return {\"sum\": add(a, b)}\n",
        "from acme_api import handle\nfrom acme_core import add\n\n\ndef test_handle():\n    assert handle(1, 2) == {\"sum\": add(1, 2)}\n",
    );
    package(
        root,
        "services/acme-worker",
        "acme-worker",
        Some("acme-api"),
        "from acme_api import handle\n\n\ndef work(job):\n    return handle(*job)\n",
        "from acme_api import handle\nfrom acme_worker import work\n\n\ndef test_work():\n    assert work((4, 5)) == handle(4, 5)\n",
    );
    tmp
}

fn package(root: &Path, rel: &str, name: &str, dep: Option<&str>, src: &str, test: &str) {
    let dir = root.join(rel);
    let module = name.replace('-', "_");
    fs::create_dir_all(dir.join("src").join(&module)).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(dir.join("src").join(&module).join("__init__.py"), src).unwrap();
    fs::write(dir.join("tests").join(format!("test_{module}.py")), test).unwrap();

    let (deps, sources) = match dep {
        Some(d) => (
            format!("dependencies = [\"{d}\"]"),
            format!("[tool.uv.sources]\n{d} = {{ workspace = true }}\n"),
        ),
        None => ("dependencies = []".to_string(), String::new()),
    };
    fs::write(
        dir.join("pyproject.toml"),
        format!(
            r#"[project]
name = "{name}"
version = "0.1.0"
requires-python = ">=3.9"
{deps}

{sources}
[dependency-groups]
dev = ["pytest>=8"]

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.ut.tasks]
test = "echo {name} >> ../../order.log && pytest -q"
lint = "python -c 'import {module}'"
"#
        ),
    )
    .unwrap();
}

/// `ut` with the real `uv` on PATH and the freshly built `ut` binary dir
/// prepended, so root tasks can shell out to `ut run -w ...`.
fn ut(dir: &Path) -> Command {
    let system_path = std::env::var("PATH").unwrap_or_default();
    let has_uv = std::env::split_paths(&system_path).any(|p| p.join("uv").is_file());
    assert!(
        has_uv,
        "the e2e tests need `uv` on PATH (https://docs.astral.sh/uv/getting-started/installation/)"
    );
    let ut_dir = assert_cmd::cargo::cargo_bin("ut")
        .parent()
        .unwrap()
        .to_path_buf();
    let mut cmd = Command::cargo_bin("ut").unwrap();
    cmd.current_dir(dir)
        .env("PATH", format!("{}:{system_path}", ut_dir.display()))
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
