use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::tasks::Task;

#[derive(Debug, Deserialize, Default)]
struct PyProject {
    project: Option<Project>,
    #[serde(rename = "dependency-groups", default)]
    dependency_groups: BTreeMap<String, Vec<toml::Value>>,
    #[serde(default)]
    tool: Tool,
}

#[derive(Debug, Deserialize)]
struct Project {
    name: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct Tool {
    #[serde(default)]
    uv: Uv,
    #[serde(default)]
    ut: Ut,
}

#[derive(Debug, Deserialize, Default)]
struct Uv {
    workspace: Option<UvWorkspace>,
    #[serde(default)]
    sources: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct UvWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct Ut {
    #[serde(default)]
    tasks: BTreeMap<String, Task>,
}

#[derive(Debug)]
pub struct Member {
    /// Name as written in `[project.name]`.
    pub name: String,
    /// PEP 503-normalized name; the identity used for dep edges and --filter.
    pub id: String,
    pub dir: PathBuf,
    pub tasks: BTreeMap<String, Task>,
    /// Normalized ids of workspace members this member depends on.
    pub deps: BTreeSet<String>,
}

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    /// Members in deterministic topological order: dependencies before
    /// dependents, ties broken by id.
    pub members: Vec<Member>,
}

/// PEP 503 name normalization: lowercase, runs of `-_.` collapse to `-`.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            prev_sep = true;
        } else {
            if prev_sep && !out.is_empty() {
                out.push('-');
            }
            prev_sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Extract the bare package name from a PEP 508 requirement string.
fn requirement_name(spec: &str) -> Option<String> {
    let req = pep508_rs::Requirement::<pep508_rs::VerbatimUrl>::from_str(spec).ok()?;
    Some(normalize(req.name.as_ref()))
}

impl Workspace {
    /// Walk up from `start` to the pyproject.toml declaring
    /// `[tool.uv.workspace]`, then load all members.
    pub fn discover(start: &Path) -> Result<Workspace> {
        let start = start
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", start.display()))?;
        for dir in start.ancestors() {
            let manifest = dir.join("pyproject.toml");
            if !manifest.is_file() {
                continue;
            }
            let doc = parse_pyproject(&manifest)?;
            if doc.tool.uv.workspace.is_some() {
                return Workspace::load(dir, doc);
            }
        }
        bail!(
            "no uv workspace found: no pyproject.toml with [tool.uv.workspace] in {} or any parent",
            start.display()
        );
    }

    fn load(root: &Path, root_doc: PyProject) -> Result<Workspace> {
        let ws = root_doc
            .tool
            .uv
            .workspace
            .as_ref()
            .expect("checked by caller");

        let excluded = expand_globs(root, &ws.exclude)?;
        let mut member_dirs = BTreeSet::new();
        for dir in expand_globs(root, &ws.members)? {
            if !excluded.contains(&dir) {
                member_dirs.insert(dir);
            }
        }

        let mut members = Vec::new();
        // The workspace root is itself a member when it has a [project] table.
        if root_doc.project.is_some() {
            members.push(build_member(root.to_path_buf(), root_doc));
        }
        for dir in member_dirs {
            if dir == root {
                continue;
            }
            let manifest = dir.join("pyproject.toml");
            if !manifest.is_file() {
                continue; // globs may match non-package directories
            }
            let doc = parse_pyproject(&manifest)?;
            if doc.project.is_none() {
                eprintln!(
                    "ut: warning: skipping {} (no [project.name])",
                    manifest.display()
                );
                continue;
            }
            members.push(build_member(dir, doc));
        }

        // Keep only dep edges that point at actual workspace members.
        let ids: BTreeSet<String> = members.iter().map(|m| m.id.clone()).collect();
        for m in &mut members {
            let id = m.id.clone();
            m.deps.retain(|d| ids.contains(d) && *d != id);
        }

        let members = toposort(members)?;
        Ok(Workspace {
            root: root.to_path_buf(),
            members,
        })
    }

    pub fn member(&self, id: &str) -> Option<&Member> {
        let id = normalize(id);
        self.members.iter().find(|m| m.id == id)
    }

    /// The member whose directory contains `path` (deepest match).
    pub fn member_containing(&self, path: &Path) -> Option<&Member> {
        self.members
            .iter()
            .filter(|m| path.starts_with(&m.dir))
            .max_by_key(|m| m.dir.components().count())
    }

    /// All direct and transitive workspace dependencies of `id`.
    pub fn transitive_deps(&self, id: &str) -> BTreeSet<String> {
        let by_id: BTreeMap<&str, &Member> =
            self.members.iter().map(|m| (m.id.as_str(), m)).collect();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = match by_id.get(id) {
            Some(m) => m.deps.iter().map(String::as_str).collect(),
            None => return seen,
        };
        while let Some(dep) = stack.pop() {
            if seen.insert(dep.to_string())
                && let Some(m) = by_id.get(dep)
            {
                stack.extend(m.deps.iter().map(String::as_str));
            }
        }
        seen
    }
}

fn parse_pyproject(path: &Path) -> Result<PyProject> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

fn expand_globs(root: &Path, patterns: &[String]) -> Result<BTreeSet<PathBuf>> {
    let mut dirs = BTreeSet::new();
    for pattern in patterns {
        let full = root.join(pattern);
        let full = full.to_str().context("non-UTF-8 workspace path")?;
        for entry in glob::glob(full).with_context(|| format!("bad glob {pattern:?}"))? {
            let path = entry?;
            if path.is_dir() {
                dirs.insert(path.canonicalize()?);
            }
        }
    }
    Ok(dirs)
}

fn build_member(dir: PathBuf, doc: PyProject) -> Member {
    let project = doc.project.expect("checked by caller");
    let mut deps = BTreeSet::new();
    // Authoritative edges: [tool.uv.sources] entries marked workspace = true.
    for (name, value) in &doc.tool.uv.sources {
        if value.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
            deps.insert(normalize(name));
        }
    }
    // Fallback: requirement names that happen to be members (filtered later).
    for spec in &project.dependencies {
        deps.extend(requirement_name(spec));
    }
    for specs in doc.dependency_groups.values() {
        for spec in specs {
            // Entries can also be tables like { include-group = "..." }; skip those.
            if let Some(spec) = spec.as_str() {
                deps.extend(requirement_name(spec));
            }
        }
    }
    Member {
        id: normalize(&project.name),
        name: project.name,
        dir,
        tasks: doc.tool.ut.tasks,
        deps,
    }
}

/// Kahn's algorithm with a min-heap on id for deterministic output.
fn toposort(members: Vec<Member>) -> Result<Vec<Member>> {
    let mut by_id: BTreeMap<String, Member> =
        members.into_iter().map(|m| (m.id.clone(), m)).collect();
    let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
    for (id, m) in &by_id {
        in_degree.entry(id.clone()).or_insert(0);
        for dep in &m.deps {
            *in_degree.entry(id.clone()).or_insert(0) += 1;
            dependents.entry(dep.clone()).or_default().push(id.clone());
        }
    }

    let mut ready: BinaryHeap<std::cmp::Reverse<String>> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| std::cmp::Reverse(id.clone()))
        .collect();
    let mut sorted = Vec::with_capacity(by_id.len());
    while let Some(std::cmp::Reverse(id)) = ready.pop() {
        for dependent in dependents.get(&id).into_iter().flatten() {
            let d = in_degree.get_mut(dependent).expect("all ids seeded");
            *d -= 1;
            if *d == 0 {
                ready.push(std::cmp::Reverse(dependent.clone()));
            }
        }
        sorted.push(by_id.remove(&id).expect("ids are unique"));
    }
    if !by_id.is_empty() {
        let cycle: Vec<&String> = by_id.keys().collect();
        bail!("dependency cycle among workspace members: {cycle:?}");
    }
    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_pep503() {
        assert_eq!(normalize("My.Package_Name"), "my-package-name");
        assert_eq!(normalize("effecton"), "effecton");
        assert_eq!(normalize("a--b__c"), "a-b-c");
    }

    #[test]
    fn extracts_requirement_names() {
        assert_eq!(requirement_name("effecton>=0.1"), Some("effecton".into()));
        assert_eq!(
            requirement_name("Foo.Bar[extra]==1.0; python_version >= '3.14'"),
            Some("foo-bar".into())
        );
        assert_eq!(requirement_name("not a spec !!"), None);
    }
}
