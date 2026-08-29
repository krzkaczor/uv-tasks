use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use owo_colors::{AnsiColors, OwoColorize};

/// Colors on only for terminals, and never when NO_COLOR is set.
fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn paint(s: &str, color: AnsiColors) -> String {
    if color_enabled() {
        s.color(color).to_string()
    } else {
        s.to_string()
    }
}

/// One member's task run within a `-w` invocation.
pub struct Invocation {
    pub id: String,
    pub dir: PathBuf,
    pub steps: Vec<String>,
    /// Ids of other invocations that must succeed before this one starts.
    pub deps: BTreeSet<String>,
}

fn uv_command(dir: &Path, step: &str) -> Command {
    let mut cmd = Command::new("uv");
    cmd.args(["run", "--directory"])
        .arg(dir)
        .args(["--", "sh", "-c", step]);
    if let Some(path) = path_with_self() {
        cmd.env("PATH", path);
    }
    cmd
}

/// PATH with this executable's directory prepended, so tasks can call `ut`
/// (e.g. a root task fanning out with `ut run -w test`) regardless of how
/// `ut` itself was invoked.
fn path_with_self() -> Option<std::ffi::OsString> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let current = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(dir).chain(std::env::split_paths(&current));
    std::env::join_paths(paths).ok()
}

/// Run a task in a single package with inherited stdio; returns the exit code.
pub fn run_local(dir: &Path, steps: &[String]) -> Result<i32> {
    for step in steps {
        if color_enabled() {
            eprintln!("{} {}", "$".dimmed(), step.bold());
        } else {
            eprintln!("$ {step}");
        }
        let status = uv_command(dir, step)
            .status()
            .context("failed to spawn uv; is it on PATH?")?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }
    }
    Ok(0)
}

enum Line {
    Out(String),
    Err(String),
}

const PALETTE: [AnsiColors; 6] = [
    AnsiColors::Cyan,
    AnsiColors::Magenta,
    AnsiColors::Green,
    AnsiColors::Yellow,
    AnsiColors::Blue,
    AnsiColors::Red,
];

/// Run invocations in parallel, respecting dependency edges: an invocation
/// starts only after all its `deps` succeeded. Fail-fast: after the first
/// failure no new invocations launch (in-flight ones finish). Returns 0 on
/// full success, 1 otherwise.
pub fn run_workspace(invocations: Vec<Invocation>, jobs: usize) -> Result<i32> {
    let width = invocations.iter().map(|i| i.id.len()).max().unwrap_or(0);
    let colors: BTreeMap<String, AnsiColors> = invocations
        .iter()
        .enumerate()
        .map(|(n, i)| (i.id.clone(), PALETTE[n % PALETTE.len()]))
        .collect();

    let (print_tx, print_rx) = mpsc::channel::<Line>();
    let printer = thread::spawn(move || {
        for line in print_rx {
            match line {
                Line::Out(l) => println!("{l}"),
                Line::Err(l) => eprintln!("{l}"),
            }
        }
    });

    let (event_tx, event_rx) = mpsc::channel::<(String, bool)>();
    let mut pending: BTreeMap<String, Invocation> =
        invocations.into_iter().map(|i| (i.id.clone(), i)).collect();
    let mut succeeded: BTreeSet<String> = BTreeSet::new();
    let mut failed: Vec<String> = Vec::new();
    let mut running = 0usize;

    loop {
        if failed.is_empty() {
            let ready: Vec<String> = pending
                .values()
                .filter(|i| i.deps.iter().all(|d| succeeded.contains(d)))
                .map(|i| i.id.clone())
                .take(jobs.saturating_sub(running))
                .collect();
            for id in ready {
                let inv = pending.remove(&id).expect("id from pending");
                let prefix = paint(&format!("{:width$} | ", inv.id), colors[&inv.id]);
                let print_tx = print_tx.clone();
                let event_tx = event_tx.clone();
                running += 1;
                thread::spawn(move || {
                    let ok = run_streamed(&inv, &prefix, &print_tx);
                    let _ = event_tx.send((inv.id, ok));
                });
            }
        }
        if running == 0 {
            break;
        }
        let (id, ok) = event_rx.recv().expect("worker threads hold senders");
        running -= 1;
        if ok {
            succeeded.insert(id);
        } else {
            failed.push(id);
        }
    }

    drop(print_tx);
    let _ = printer.join();

    if !failed.is_empty() {
        eprintln!(
            "\n{} task failed in: {}",
            paint("✗", AnsiColors::Red),
            failed.join(", ")
        );
        if !pending.is_empty() {
            let skipped: Vec<&String> = pending.keys().collect();
            eprintln!(
                "  skipped: {}",
                skipped
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok(1);
    }
    if !pending.is_empty() {
        // Unreachable with a valid toposorted graph; guard against deadlock bugs.
        anyhow::bail!(
            "internal error: unrunnable invocations: {:?}",
            pending.keys()
        );
    }
    Ok(0)
}

/// Run one invocation's steps, streaming prefixed output. Returns success.
fn run_streamed(inv: &Invocation, prefix: &str, print_tx: &mpsc::Sender<Line>) -> bool {
    for step in &inv.steps {
        let _ = print_tx.send(Line::Out(format!("{prefix}$ {step}")));
        let spawned = uv_command(&inv.dir, step)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = print_tx.send(Line::Err(format!("{prefix}failed to spawn uv: {e}")));
                return false;
            }
        };

        let stderr = child.stderr.take().expect("stderr piped");
        let err_prefix = prefix.to_string();
        let err_tx = print_tx.clone();
        let err_reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = err_tx.send(Line::Err(format!("{err_prefix}{line}")));
            }
        });
        let stdout = child.stdout.take().expect("stdout piped");
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = print_tx.send(Line::Out(format!("{prefix}{line}")));
        }
        let _ = err_reader.join();

        match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let _ = print_tx.send(Line::Err(format!(
                    "{prefix}{} exited with {}",
                    paint("✗", AnsiColors::Red),
                    status
                        .code()
                        .map_or("signal".to_string(), |c| c.to_string())
                )));
                return false;
            }
            Err(e) => {
                let _ = print_tx.send(Line::Err(format!("{prefix}wait failed: {e}")));
                return false;
            }
        }
    }
    true
}
