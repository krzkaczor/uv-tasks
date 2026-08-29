mod run;
mod tasks;
mod workspace;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use usage::{Args, Cli, Run, Subcommands};

use crate::run::Invocation;
use crate::tasks::with_args;
use crate::workspace::{Workspace, normalize};

/// ut — a task runner for uv workspaces
#[derive(Cli)]
#[usage(bin = "ut", version = "0.1.0", completion)]
struct UtCli {
    #[usage(subcommand)]
    command: Commands,
}

#[derive(Subcommands)]
#[usage(run)]
enum Commands {
    Run(RunCmd),
    List(ListCmd),
    Completion(Completion),
}

/// Run a task defined in [tool.ut.tasks]
#[derive(Args)]
struct RunCmd {
    /// Run in every workspace member that defines the task,
    /// in parallel, respecting the dependency graph
    #[usage(short = 'w', long)]
    workspace: bool,
    /// With -w, only run in the given package(s) (repeatable)
    #[usage(long)]
    filter: Vec<String>,
    /// Max concurrent tasks (default: logical CPUs; 1 = sequential)
    #[usage(short = 'j', long)]
    jobs: Option<usize>,
    /// Task name
    task: String,
    /// Extra arguments appended to the task command
    args: Vec<String>,
}

/// List workspace members and their tasks
#[derive(Args)]
struct ListCmd {}

/// Print a shell completion script
#[derive(Args)]
struct Completion {
    /// Which shell to generate for
    #[usage(long, choices("bash", "zsh", "fish"))]
    shell: String,
}

impl Run for RunCmd {
    type Output = i32;
    fn run(self) -> i32 {
        report(run_task(self))
    }
}

impl Run for ListCmd {
    type Output = i32;
    fn run(self) -> i32 {
        report(list())
    }
}

impl Run for Completion {
    type Output = i32;
    fn run(self) -> i32 {
        let shell = match self.shell.as_str() {
            "bash" => usage::complete::Shell::Bash,
            "zsh" => usage::complete::Shell::Zsh,
            _ => usage::complete::Shell::Fish,
        };
        print!("{}", UtCli::completion_script(shell));
        0
    }
}

fn report(result: Result<i32>) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ut: error: {e:#}");
            2
        }
    }
}

fn discover() -> Result<Workspace> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    Workspace::discover(&cwd)
}

fn run_task(cmd: RunCmd) -> Result<i32> {
    let ws = discover()?;
    if cmd.workspace {
        run_across_workspace(&ws, &cmd)
    } else {
        run_in_current_package(&ws, cmd)
    }
}

fn run_in_current_package(ws: &Workspace, cmd: RunCmd) -> Result<i32> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let member = ws.member_containing(&cwd).with_context(|| {
        format!(
            "current directory is not inside a workspace member of {}",
            ws.root.display()
        )
    })?;
    let steps = task_steps(member, &cmd.task, &cmd.args)?;
    run::run_local(&member.dir, &steps)
}

fn run_across_workspace(ws: &Workspace, cmd: &RunCmd) -> Result<i32> {
    let filter: BTreeSet<String> = cmd.filter.iter().map(|f| normalize(f)).collect();
    for f in &filter {
        if ws.member(f).is_none() {
            bail!("--filter {f:?} does not match any workspace member");
        }
    }

    let selected: Vec<_> = ws
        .members
        .iter()
        .filter(|m| m.tasks.contains_key(&cmd.task))
        .filter(|m| filter.is_empty() || filter.contains(&m.id))
        .collect();
    if selected.is_empty() {
        bail!("no workspace member defines task {:?}", cmd.task);
    }

    let ids: BTreeSet<String> = selected.iter().map(|m| m.id.clone()).collect();
    let invocations: Vec<Invocation> = selected
        .iter()
        .map(|m| {
            Ok(Invocation {
                id: m.id.clone(),
                dir: m.dir.clone(),
                steps: task_steps(m, &cmd.task, &cmd.args)?,
                // Order through members outside the selection still holds:
                // transitive deps, restricted to selected members.
                deps: ws
                    .transitive_deps(&m.id)
                    .intersection(&ids)
                    .cloned()
                    .collect(),
            })
        })
        .collect::<Result<_>>()?;

    let jobs = cmd.jobs.unwrap_or_else(thread_count).max(1);
    run::run_workspace(invocations, jobs)
}

fn thread_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn task_steps(
    member: &crate::workspace::Member,
    task: &str,
    args: &[String],
) -> Result<Vec<String>> {
    let def = member.tasks.get(task).with_context(|| {
        format!(
            "package {:?} has no task {task:?} in [tool.ut.tasks]",
            member.name
        )
    })?;
    if def.is_sequence() && !args.is_empty() {
        bail!("task {task:?} is a sequence; passthrough arguments are not supported");
    }
    Ok(def.steps().iter().map(|s| with_args(s, args)).collect())
}

fn list() -> Result<i32> {
    let ws = discover()?;
    let name_w = ws.members.iter().map(|m| m.name.len()).max().unwrap_or(0);
    let dir_w = ws
        .members
        .iter()
        .map(|m| rel(&m.dir, &ws.root).len())
        .max()
        .unwrap_or(0);
    for m in &ws.members {
        let tasks: Vec<&str> = m.tasks.keys().map(String::as_str).collect();
        println!(
            "{:name_w$}  {:dir_w$}  {}",
            m.name,
            rel(&m.dir, &ws.root),
            tasks.join(", ")
        );
    }
    Ok(0)
}

fn rel(dir: &Path, root: &Path) -> String {
    let r = dir.strip_prefix(root).unwrap_or(dir);
    if r.as_os_str().is_empty() {
        ".".to_string()
    } else {
        r.display().to_string()
    }
}

fn main() {
    std::process::exit(UtCli::parse().command.run())
}
