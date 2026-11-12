//! `ripple skill`: install the agent skill embedded in this binary.
//!
//! The skill (`skill/ripple/SKILL.md`) is stamped with the ripple version
//! on install. Next to it, `.ripple-skill` records the version and a hash
//! of exactly what was written, so ripple can tell its own file from one
//! edited locally: `update` and `install` never overwrite an edited skill
//! without `--force`, and `uninstall` removes only files it wrote.
//!
//! Locations (`<base>` is the repository root with `--project`, the
//! default, or `$HOME` with `--user`):
//! * `claude`: `<base>/.claude/skills/ripple/SKILL.md`
//! * `codex`:  `<base>/.codex/skills/ripple/SKILL.md`
//! * `agents`: `<base>/.agents/skills/ripple/SKILL.md` (shared convention)

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const SKILL_SOURCE: &str = include_str!("../../../skill/ripple/SKILL.md");
const MANIFEST: &str = ".ripple-skill";
const STAMP_PREFIX: &str = "<!-- ripple-skill ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
    Agents,
}

impl Agent {
    pub fn dir(self) -> &'static str {
        match self {
            Agent::Claude => ".claude",
            Agent::Codex => ".codex",
            Agent::Agents => ".agents",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Agents => "agents",
        }
    }
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Agents];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::User => "user",
        }
    }
}

/// The skill text this binary installs: the embedded SKILL.md with a
/// version stamp after its front matter.
pub fn stamped() -> String {
    let stamp = format!("{STAMP_PREFIX}{} (installed by `ripple skill install`; `ripple skill update` refreshes it) -->\n", env!("CARGO_PKG_VERSION"));
    match SKILL_SOURCE.strip_prefix("---\n").and_then(|rest| rest.find("\n---\n").map(|i| i + 4 + 5)) {
        Some(end) => format!("{}{stamp}{}", &SKILL_SOURCE[..end], &SKILL_SOURCE[end..]),
        None => format!("{stamp}{SKILL_SOURCE}"),
    }
}

fn hash(s: &str) -> String {
    format!("{:016x}", crate::paths::fnv1a64(s.as_bytes()))
}

/// The ripple version stamped into an installed skill, if any.
fn stamped_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with(STAMP_PREFIX))?;
    Some(line[STAMP_PREFIX.len()..].split_whitespace().next()?.to_string())
}

pub fn base(scope: Scope, root: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(root.to_path_buf()),
        Scope::User => std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).context("--user needs $HOME"),
    }
}

pub fn skill_dir(base: &Path, agent: Agent) -> PathBuf {
    base.join(agent.dir()).join("skills").join("ripple")
}

/// The state of the skill at one location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Missing,
    /// Written by this ripple version, unmodified.
    Current,
    /// Written by another ripple version, unmodified.
    Outdated(String),
    /// Edited since ripple wrote it, or not written by ripple at all.
    Modified,
}

impl State {
    pub fn word(&self) -> String {
        match self {
            State::Missing => "not installed".into(),
            State::Current => format!("installed, current ({})", env!("CARGO_PKG_VERSION")),
            State::Outdated(v) => format!("installed, outdated ({v}; this ripple is {})", env!("CARGO_PKG_VERSION")),
            State::Modified => "installed, modified locally (or not written by ripple)".into(),
        }
    }
    fn key(&self) -> &'static str {
        match self {
            State::Missing => "missing",
            State::Current => "current",
            State::Outdated(_) => "outdated",
            State::Modified => "modified",
        }
    }
}

pub fn state(dir: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else {
        return State::Missing;
    };
    let recorded = std::fs::read_to_string(dir.join(MANIFEST)).ok().and_then(|m| serde_json::from_str::<Value>(&m).ok());
    let ours = recorded.as_ref().and_then(|m| m["hash"].as_str()).is_some_and(|h| h == hash(&text));
    if !ours {
        return State::Modified;
    }
    if text == stamped() {
        State::Current
    } else {
        State::Outdated(stamped_version(&text).unwrap_or_else(|| "unknown".into()))
    }
}

fn write(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let text = stamped();
    std::fs::write(dir.join("SKILL.md"), &text).with_context(|| format!("writing {}", dir.join("SKILL.md").display()))?;
    let manifest = json!({"version": env!("CARGO_PKG_VERSION"), "hash": hash(&text), "files": ["SKILL.md"]});
    std::fs::write(dir.join(MANIFEST), format!("{manifest}\n"))?;
    Ok(())
}

pub enum Action {
    Install { force: bool },
    Update { force: bool },
    Uninstall { force: bool },
    Status { json: bool },
    Print,
}

/// Runs one `ripple skill` command. Messages go to stdout (they are the
/// command's output); refusals are errors (exit 2) naming `--force`.
pub fn run(root: &Path, scopes: &[Scope], agents: &[Agent], action: Action, quiet: bool) -> Result<u8> {
    let say = |s: String| {
        if !quiet {
            println!("{s}");
        }
    };
    if let Action::Print = action {
        print!("{}", stamped());
        return Ok(0);
    }
    let mut rows = Vec::new();
    let mut code = 0;
    for &scope in scopes {
        let b = base(scope, root)?;
        for &agent in agents {
            let dir = skill_dir(&b, agent);
            let st = state(&dir);
            let file = dir.join("SKILL.md");
            match &action {
                Action::Install { force } | Action::Update { force } => {
                    let update = matches!(action, Action::Update { .. });
                    match (&st, force) {
                        (State::Missing, _) if update => say(format!("{}: not installed (nothing to update)", file.display())),
                        (State::Current, false) => say(format!("{}: already current ({})", file.display(), env!("CARGO_PKG_VERSION"))),
                        (State::Modified, false) => bail!("{} was modified locally (or not written by ripple); not overwriting it (use --force)", file.display()),
                        _ => {
                            write(&dir)?;
                            say(format!("{}: {} ({})", file.display(), if st == State::Missing { "installed" } else { "updated" }, env!("CARGO_PKG_VERSION")));
                        }
                    }
                }
                Action::Uninstall { force } => match (&st, force) {
                    (State::Missing, _) => say(format!("{}: not installed", file.display())),
                    (State::Modified, false) => bail!("{} was modified locally (or not written by ripple); not removing it (use --force)", file.display()),
                    _ => {
                        std::fs::remove_file(&file).ok();
                        std::fs::remove_file(dir.join(MANIFEST)).ok();
                        // Only directories left empty (ripple/, then skills/ and the agent dir).
                        let mut d = dir.clone();
                        for _ in 0..3 {
                            if std::fs::remove_dir(&d).is_err() {
                                break;
                            }
                            d.pop();
                        }
                        say(format!("{}: removed", file.display()));
                    }
                },
                Action::Status { .. } => {
                    if st != State::Current && st != State::Missing {
                        code = 1;
                    }
                    rows.push(json!({"scope": scope.name(), "agent": agent.name(), "path": file, "state": st.key(), "detail": st.word()}));
                }
                Action::Print => unreachable!(),
            }
        }
    }
    if let Action::Status { json } = action {
        if json {
            println!("{}", json!({"version": env!("CARGO_PKG_VERSION"), "locations": rows}));
        } else {
            for r in &rows {
                println!("{:<8} {:<7} {}  {}", r["scope"].as_str().unwrap_or(""), r["agent"].as_str().unwrap_or(""), r["detail"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""));
            }
        }
    }
    Ok(code)
}

/// One-line summary for `ripple doctor`: the project and user Claude skill.
pub fn summary(root: &Path) -> Vec<(String, State)> {
    let mut out = Vec::new();
    for scope in [Scope::Project, Scope::User] {
        let Ok(b) = base(scope, root) else { continue };
        for agent in Agent::ALL {
            let dir = skill_dir(&b, agent);
            let st = state(&dir);
            if st != State::Missing || agent == Agent::Claude {
                out.push((format!("{} ({})", dir.join("SKILL.md").display(), scope.name()), st));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_follows_the_front_matter() {
        let s = stamped();
        assert!(s.starts_with("---\nname: ripple\n"), "{s}");
        let after = s.split("\n---\n").nth(1).unwrap();
        assert!(after.starts_with(STAMP_PREFIX), "{after}");
        assert_eq!(stamped_version(&s).as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn install_update_uninstall_respect_local_edits() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let dir = skill_dir(root, Agent::Claude);
        let run1 = |a: Action| run(root, &[Scope::Project], &[Agent::Claude], a, true);
        assert_eq!(state(&dir), State::Missing);
        run1(Action::Install { force: false }).unwrap();
        assert_eq!(state(&dir), State::Current);
        // An older install: outdated, and update rewrites it.
        let old = stamped().replace(env!("CARGO_PKG_VERSION"), "0.0.1");
        std::fs::write(dir.join("SKILL.md"), &old).unwrap();
        std::fs::write(dir.join(MANIFEST), json!({"version": "0.0.1", "hash": hash(&old)}).to_string()).unwrap();
        assert_eq!(state(&dir), State::Outdated("0.0.1".into()));
        run1(Action::Update { force: false }).unwrap();
        assert_eq!(state(&dir), State::Current);
        // Edited locally: never overwritten or removed without --force.
        std::fs::write(dir.join("SKILL.md"), "mine").unwrap();
        assert_eq!(state(&dir), State::Modified);
        assert!(run1(Action::Install { force: false }).is_err());
        assert!(run1(Action::Update { force: false }).is_err());
        assert!(run1(Action::Uninstall { force: false }).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), "mine");
        run1(Action::Update { force: true }).unwrap();
        assert_eq!(state(&dir), State::Current);
        // Uninstall removes what it wrote and only empty directories.
        std::fs::write(root.join(".claude/settings.json"), "{}").unwrap();
        run1(Action::Uninstall { force: false }).unwrap();
        assert!(!dir.exists() && !root.join(".claude/skills").exists());
        assert!(root.join(".claude/settings.json").exists());
    }
}
