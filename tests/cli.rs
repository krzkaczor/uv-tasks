use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

/// Build a fixture uv workspace:
///   root (virtual, no [project])
///   pkgs/zeta            — no deps
///   pkgs/alpha           — depends on zeta (alphabetically before it!)
///   pkgs/mid             — independent
///   pkgs/skipme          — excluded via [tool.uv.workspace].exclude
///   pkgs/notapkg         — matched by glob but has no pyproject.toml
fn fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("pyproject.toml"),
        r#"
[tool.uv.workspace]
members = ["pkgs/*"]
exclude = ["pkgs/skipme"]
"#,
    )
    .unwrap();

    member(
        root,
        "zeta",
        "",
        "build = \"echo built-zeta\"\ntest = \"echo tested-zeta\"",
    );
    member(
        root,
        "alpha",
        "[tool.uv.sources]\nzeta = { workspace = true }\n",
        "build = \"echo built-alpha\"",
    );
    member(root, "mid", "", "build = \"echo built-mid\"");
    member(root, "skipme", "", "build = \"echo built-skipme\"");
    fs::create_dir_all(root.join("pkgs/notapkg")).unwrap();
    tmp
}

fn member(root: &Path, name: &str, extra: &str, tasks: &str) {
    let dir = root.join("pkgs").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("pyproject.toml"),
        format!(
            "[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n{extra}\n[tool.ut.tasks]\n{tasks}\n"
        ),
    )
    .unwrap();
}

/// A fake `uv` on PATH that emulates `uv run --directory <dir> -- sh -c <cmd>`
/// so tests need no network, Python, or real uv.
fn fake_uv_bin() -> (TempDir, String) {
    let tmp = TempDir::new().unwrap();
    let uv = tmp.path().join("uv");
    fs::write(
        &uv,
        "#!/bin/sh\nshift\nif [ \"$1\" = \"--directory\" ]; then dir=\"$2\"; shift 2; fi\n[ \"$1\" = \"--\" ] && shift\ncd \"$dir\" && exec \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        tmp.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (tmp, path)
}

fn ut(dir: &Path, path: &str) -> Command {
    let mut cmd = Command::cargo_bin("ut").unwrap();
    cmd.current_dir(dir).env("PATH", path).env("NO_COLOR", "1");
    cmd
}

fn set_task(root: &Path, pkg: &str, tasks: &str) {
    let dir = root.join("pkgs").join(pkg);
    let manifest = dir.join("pyproject.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let head = text.split("[tool.ut.tasks]").next().unwrap();
    fs::write(&manifest, format!("{head}[tool.ut.tasks]\n{tasks}\n")).unwrap();
}

#[test]
fn list_shows_members_in_topo_order_and_honors_exclude() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();

    let out = ut(ws.path(), &path).arg("list").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();

    // mid and zeta have no deps (tie broken by name); alpha waits for zeta.
    assert!(lines[0].starts_with("mid"));
    assert!(lines[1].starts_with("zeta"));
    assert!(lines[2].starts_with("alpha"));
    assert!(!stdout.contains("skipme"));
    assert!(!stdout.contains("notapkg"));
    assert!(lines[1].contains("build, test"));
}

#[test]
fn runs_task_in_current_package() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();

    ut(&ws.path().join("pkgs/zeta"), &path)
        .args(["run", "build"])
        .assert()
        .success()
        .stdout(predicates::str::contains("built-zeta"));
}

#[test]
fn bare_task_shorthand_without_run_subcommand() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();

    ut(&ws.path().join("pkgs/zeta"), &path)
        .arg("build")
        .assert()
        .success()
        .stdout(predicates::str::contains("built-zeta"));
}

#[test]
fn appends_passthrough_args() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    set_task(ws.path(), "zeta", "say = \"echo\"");

    ut(&ws.path().join("pkgs/zeta"), &path)
        .args(["run", "say", "hello", "wor ld"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hello wor ld"));
}

#[test]
fn propagates_exit_code() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    set_task(ws.path(), "zeta", "boom = \"exit 3\"");

    ut(&ws.path().join("pkgs/zeta"), &path)
        .args(["run", "boom"])
        .assert()
        .code(3);
}

#[test]
fn sequence_task_runs_steps_and_rejects_args() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    set_task(ws.path(), "zeta", "check = [\"echo one\", \"echo two\"]");

    let out = ut(&ws.path().join("pkgs/zeta"), &path)
        .args(["run", "check"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let one = stdout.find("one").unwrap();
    let two = stdout.find("two").unwrap();
    assert!(one < two);

    ut(&ws.path().join("pkgs/zeta"), &path)
        .args(["run", "check", "extra"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("sequence"));
}

#[test]
fn workspace_run_respects_dependency_order() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    let log = ws.path().join("order.log");
    // zeta is slow; if alpha didn't wait for its dependency it would log first.
    set_task(
        ws.path(),
        "zeta",
        &format!("build = \"sleep 0.4 && echo zeta >> {}\"", log.display()),
    );
    set_task(
        ws.path(),
        "alpha",
        &format!("build = \"echo alpha >> {}\"", log.display()),
    );
    set_task(ws.path(), "mid", "other = \"echo nope\"");

    ut(ws.path(), &path)
        .args(["run", "-w", "build"])
        .assert()
        .success();

    let logged = fs::read_to_string(&log).unwrap();
    assert_eq!(logged, "zeta\nalpha\n");
}

#[test]
fn workspace_run_is_parallel_for_independent_members() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    // Deterministic rendezvous: each member drops its flag, then waits for the
    // other's. Both can only succeed if the two tasks overlap in time; any
    // sequential schedule times out the first task after 10s.
    let rendezvous = |mine: &str, theirs: &str| {
        format!(
            "build = \"touch {root}/{mine}; for i in $(seq 100); do [ -f {root}/{theirs} ] && exit 0; sleep 0.1; done; exit 1\"",
            root = ws.path().display(),
        )
    };
    set_task(ws.path(), "zeta", &rendezvous("z.flag", "m.flag"));
    set_task(ws.path(), "mid", &rendezvous("m.flag", "z.flag"));
    set_task(ws.path(), "alpha", "other = \"echo nope\"");

    ut(ws.path(), &path)
        .args(["run", "-w", "build"])
        .assert()
        .success();
}

#[test]
fn workspace_run_sequential_with_s() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    let log = ws.path().join("seq.log");
    // With -s each task fully finishes before the next starts, so start/end
    // markers never interleave; this holds deterministically regardless of
    // machine speed.
    let logging = |name: &str| {
        format!(
            "build = \"echo start-{name} >> {log}; sleep 0.2; echo end-{name} >> {log}\"",
            log = log.display(),
        )
    };
    set_task(ws.path(), "zeta", &logging("zeta"));
    set_task(ws.path(), "mid", &logging("mid"));
    set_task(ws.path(), "alpha", "other = \"echo nope\"");

    ut(ws.path(), &path)
        .args(["run", "-w", "-s", "build"])
        .assert()
        .success();

    let logged = fs::read_to_string(&log).unwrap();
    assert_eq!(logged, "start-mid\nend-mid\nstart-zeta\nend-zeta\n");
}

#[test]
fn workspace_run_fails_fast_and_reports_skipped() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    set_task(ws.path(), "zeta", "build = \"exit 1\"");
    set_task(ws.path(), "alpha", "build = \"echo alpha-ran\"");
    set_task(ws.path(), "mid", "other = \"echo nope\"");

    let out = ut(ws.path(), &path)
        .args(["run", "-w", "build"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stderr.contains("zeta"));
    assert!(stderr.contains("skipped"));
    assert!(stderr.contains("alpha"));
    assert!(!stdout.contains("alpha-ran"));
}

#[test]
fn workspace_run_with_filter() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();

    let out = ut(ws.path(), &path)
        .args(["run", "-w", "--filter", "mid", "build"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("built-mid"));
    assert!(!stdout.contains("built-zeta"));

    ut(ws.path(), &path)
        .args(["run", "-w", "--filter", "nosuch", "build"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("nosuch"));
}

#[test]
fn workspace_output_lines_are_prefixed() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();

    let out = ut(ws.path(), &path)
        .args(["run", "-w", "test"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("zeta | tested-zeta"), "got: {stdout}");
}

/// PATH with the fake uv first and the freshly built `ut` binary second, so a
/// root task can shell out to `ut run -w ...`.
fn path_with_ut(fake_uv_path: &str) -> String {
    let ut_dir = assert_cmd::cargo::cargo_bin("ut")
        .parent()
        .unwrap()
        .to_path_buf();
    format!("{}:{fake_uv_path}", ut_dir.display())
}

fn set_root(root: &Path, contents: &str) {
    fs::write(root.join("pyproject.toml"), contents).unwrap();
}

const ROOT_WITH_TASKS: &str = r#"
[project]
name = "root"
version = "0.1.0"

[tool.uv.workspace]
members = ["pkgs/*"]
exclude = ["pkgs/skipme"]

[tool.ut.tasks]
test = "ut run -w test"
"#;

#[test]
fn root_task_fans_out_to_members() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    let path = path_with_ut(&path);
    set_root(ws.path(), ROOT_WITH_TASKS);
    set_task(ws.path(), "mid", "test = \"echo tested-mid\"");

    let out = ut(ws.path(), &path)
        .args(["run", "test"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("zeta | tested-zeta"), "got: {stdout}");
    assert!(stdout.contains("mid  | tested-mid"), "got: {stdout}");
    // The root is never a -w target, so its own `test` doesn't recurse.
    assert!(!stdout.contains("root |"), "got: {stdout}");

    let out = ut(ws.path(), &path)
        .args(["run", "-w", "test"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("root |"), "got: {stdout}");
}

#[test]
fn list_shows_root_separately_and_not_as_member() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    set_root(ws.path(), ROOT_WITH_TASKS);

    let out = ut(ws.path(), &path).arg("list").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("root"), "got: {stdout}");
    assert!(lines[0].contains("  .  "), "got: {stdout}");
    assert!(lines[0].ends_with("test"), "got: {stdout}");
    assert!(lines[1].starts_with("mid"), "got: {stdout}");
    assert_eq!(lines.len(), 4, "got: {stdout}");

    // A root without tasks doesn't get a line, even if it has [project].
    set_root(
        ws.path(),
        "[project]\nname = \"root\"\nversion = \"0.1.0\"\n[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n",
    );
    let out = ut(ws.path(), &path).arg("list").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("root"), "got: {stdout}");
}

#[test]
fn root_task_runs_from_virtual_root() {
    let ws = fixture();
    let (_bin, path) = fake_uv_bin();
    set_root(
        ws.path(),
        "[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n[tool.ut.tasks]\nhello = \"echo hi-from-root\"\n",
    );

    ut(ws.path(), &path)
        .arg("hello")
        .assert()
        .success()
        .stdout(predicates::str::contains("hi-from-root"));

    ut(ws.path(), &path)
        .arg("nosuch")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("workspace root has no task"));
}
