mod run;
mod tasks;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use usage::{Args, Cli, Run, Subcommands};

use crate::run::Invocation;
use crate::tasks::{Task, with_args};
use crate::workspace::{Workspace, normalize};

/// ut — a task runner for uv workspaces
#[derive(Cli)]
#[usage(bin = "ut", version = "0.1.0", completion)]
struct UtCli {
    #[usage(subcommand)]
    command: Option<Commands>,
    /// Task name (shorthand for `ut run <task>`)
    task: Option<String>,
    /// Extra arguments appended to the task command
    args: Vec<String>,
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
    /// Run workspace tasks one at a time instead of in parallel
    #[usage(short = 's', long)]
    sequential: bool,
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
    let (tasks, label, dir) = match ws.member_containing(&cwd) {
        Some(m) => (&m.tasks, format!("package {:?}", m.name), &m.dir),
        None if cwd.starts_with(&ws.root) => (&ws.root_tasks, "workspace root".into(), &ws.root),
        None => bail!(
            "current directory is not inside workspace {}",
            ws.root.display()
        ),
    };
    let steps = task_steps(tasks, &label, &cmd.task, &cmd.args)?;
    run::run_local(dir, &steps)
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
                steps: task_steps(
                    &m.tasks,
                    &format!("package {:?}", m.name),
                    &cmd.task,
                    &cmd.args,
                )?,
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

    let jobs = if cmd.sequential { 1 } else { thread_count() };
    run::run_workspace(invocations, jobs)
}

fn thread_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn task_steps(
    tasks: &BTreeMap<String, Task>,
    label: &str,
    task: &str,
    args: &[String],
) -> Result<Vec<String>> {
    let def = tasks
        .get(task)
        .with_context(|| format!("{label} has no task {task:?} in [tool.ut.tasks]"))?;
    if def.is_sequence() && !args.is_empty() {
        bail!("task {task:?} is a sequence; passthrough arguments are not supported");
    }
    Ok(def.steps().iter().map(|s| with_args(s, args)).collect())
}

fn list() -> Result<i32> {
    let ws = discover()?;
    // (name, relative dir, tasks); the root comes first when it defines tasks.
    let mut rows: Vec<(&str, String, &BTreeMap<String, Task>)> = Vec::new();
    if !ws.root_tasks.is_empty() {
        let name = ws.root_name.as_deref().unwrap_or("(root)");
        rows.push((name, rel(&ws.root, &ws.root), &ws.root_tasks));
    }
    for m in &ws.members {
        rows.push((&m.name, rel(&m.dir, &ws.root), &m.tasks));
    }
    let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let dir_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    for (name, dir, tasks) in rows {
        let tasks: Vec<&str> = tasks.keys().map(String::as_str).collect();
        println!("{name:name_w$}  {dir:dir_w$}  {}", tasks.join(", "));
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
    let cli = UtCli::parse();
    let code = match (cli.command, cli.task) {
        (Some(command), _) => command.run(),
        (None, Some(task)) => RunCmd {
            workspace: false,
            filter: vec![],
            sequential: false,
            task,
            args: cli.args,
        }
        .run(),
        (None, None) => {
            eprintln!("ut: error: no task or subcommand given (try `ut list` or `ut --help`)");
            2
        }
    };
    std::process::exit(code)
}
