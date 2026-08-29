use serde::Deserialize;

/// A task from `[tool.ut.tasks]`: either a single shell command or a sequence
/// of commands run in order, stopping at the first failure.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Task {
    Command(String),
    Steps(Vec<String>),
}

impl Task {
    pub fn steps(&self) -> Vec<String> {
        match self {
            Task::Command(cmd) => vec![cmd.clone()],
            Task::Steps(cmds) => cmds.clone(),
        }
    }

    pub fn is_sequence(&self) -> bool {
        matches!(self, Task::Steps(_))
    }
}

/// Quote a string for safe interpolation into an `sh -c` command line.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Append passthrough args to a task command, npm-script style.
pub fn with_args(cmd: &str, args: &[String]) -> String {
    if args.is_empty() {
        return cmd.to_string();
    }
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    format!("{cmd} {}", quoted.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("pytest"), "pytest");
        assert_eq!(shell_quote("-k"), "-k");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn appends_args() {
        assert_eq!(with_args("pytest -s", &[]), "pytest -s");
        assert_eq!(
            with_args("pytest", &["-k".into(), "scope test".into()]),
            "pytest -k 'scope test'"
        );
    }
}
