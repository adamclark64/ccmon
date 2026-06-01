use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use walkdir::WalkDir;

mod tui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Agent,
    Subagent,
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    pub(crate) project: String,
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) mtime: SystemTime,
    pub(crate) summary: String,
    pub(crate) cwd: Option<String>,
    pub(crate) tmux_session: Option<String>,
    /// tmux socket name the session lives on (`-L <socket>`). `None` for the
    /// default server. claude-swarm runs each swarm on its own socket.
    pub(crate) tmux_socket: Option<String>,
    pub(crate) running: bool,
    pub(crate) agent_type: Option<String>,
    /// Swarm teammate identity (from the transcript's `agentName`/`teamName`),
    /// used to match the running teammate process which carries no session-id.
    pub(crate) agent_name: Option<String>,
    pub(crate) team_name: Option<String>,
}

pub const HEADER: &str = "  STATE       AGE   KIND        PROJECT                          TITLE";

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = if self.tmux_session.is_some() {
            "● live"
        } else if self.running {
            "○ running"
        } else {
            ""
        };
        let kind = match self.kind {
            Kind::Agent => "agent".to_string(),
            Kind::Subagent => match self.agent_type.as_deref() {
                None | Some("general-purpose") => "sub".to_string(),
                Some(t) => {
                    // For namespaced types like "coderabbit:code-reviewer", keep the suffix.
                    let short = t.rsplit(':').next().unwrap_or(t);
                    let short = short.trim_end_matches("-purpose");
                    format!("sub:{}", &short[..short.len().min(8)])
                }
            },
        };
        let age = format_age(self.mtime);
        let proj = self
            .cwd
            .as_deref()
            .and_then(|c| {
                Path::new(c)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| decode_project(&self.project));
        let summary = match self.agent_name.as_deref() {
            Some(name) => format!("{name}  {}", self.summary),
            None => self.summary.clone(),
        };
        let title = truncate(&summary, 60);
        write!(f, "{state:<10}  {age:>4}  {kind:<10}  {proj:<32}  {title}")
    }
}

pub(crate) fn format_age(t: SystemTime) -> String {
    let now = SystemTime::now();
    let secs = now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

pub(crate) fn decode_project(encoded: &str) -> String {
    // "-Users-adamclark-repos-job-search-app" -> "job-search-app"
    encoded
        .trim_start_matches('-')
        .rsplit_once('-')
        .map(|(_, last)| last.to_string())
        .unwrap_or_else(|| encoded.to_string())
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[derive(Deserialize)]
struct AnyRecord {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    message: Option<MsgWrap>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "agentName")]
    agent_name: Option<String>,
    #[serde(default, rename = "teamName")]
    team_name: Option<String>,
}

/// What a transcript scan extracts about a session.
#[derive(Default)]
struct Scan {
    summary: Option<String>,
    cwd: Option<String>,
    /// Swarm teammate identity, present only for swarm-spawned sessions.
    agent_name: Option<String>,
    team_name: Option<String>,
}

#[derive(Deserialize)]
struct MsgWrap {
    #[serde(default)]
    content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AgentMeta {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    #[serde(rename = "agentType")]
    agent_type: Option<String>,
}

/// For a subagent jsonl at `…/agent-<id>.jsonl`, read the sibling
/// `agent-<id>.meta.json` if present. Returns (description, agent_type).
fn read_agent_meta(jsonl: &Path) -> (Option<String>, Option<String>) {
    let meta_path = jsonl.with_extension("meta.json");
    let bytes = match std::fs::read(&meta_path) {
        Ok(b) => b,
        Err(_) => return (None, None),
    };
    match serde_json::from_slice::<AgentMeta>(&bytes) {
        Ok(m) => (m.description, m.agent_type),
        Err(_) => (None, None),
    }
}

fn scan_jsonl(path: &Path) -> Scan {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Scan::default(),
    };
    let reader = BufReader::new(f);
    let mut summary: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut agent_name: Option<String> = None;
    let mut team_name: Option<String> = None;
    for line in reader.lines().map_while(Result::ok).take(80) {
        let rec: AnyRecord = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if agent_name.is_none() {
            if let Some(n) = rec.agent_name {
                if !n.is_empty() {
                    agent_name = Some(n);
                }
            }
        }
        if team_name.is_none() {
            if let Some(t) = rec.team_name {
                if !t.is_empty() {
                    team_name = Some(t);
                }
            }
        }
        if cwd.is_none() {
            if let Some(c) = rec.cwd {
                if !c.is_empty() {
                    cwd = Some(c);
                }
            }
        }
        if summary.is_none() {
            if let Some(s) = rec.summary {
                if !s.is_empty() {
                    summary = Some(s);
                }
            } else if rec.r#type.as_deref() == Some("user") {
                if let Some(m) = rec.message {
                    if let Some(c) = m.content {
                        let t = extract_text(&c);
                        if !t.is_empty() {
                            summary = Some(t);
                        }
                    }
                }
            }
        }
        if summary.is_some() && cwd.is_some() && agent_name.is_some() && team_name.is_some() {
            break;
        }
    }
    Scan {
        summary,
        cwd,
        agent_name,
        team_name,
    }
}

fn extract_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

pub(crate) fn discover(root: &Path, max_age_hours: u64) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let cutoff =
        SystemTime::now().checked_sub(std::time::Duration::from_secs(max_age_hours * 3600));
    for entry in WalkDir::new(root).max_depth(5).into_iter().flatten() {
        let p = entry.path();
        if !p.is_file() || p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(c) = cutoff {
            if mtime < c {
                continue;
            }
        }
        // Determine kind by path shape:
        //   <root>/<project>/<sid>.jsonl                              -> Agent
        //   <root>/<project>/<sid>/subagents/agent-<id>.jsonl         -> Subagent
        let rel = p.strip_prefix(root).unwrap_or(p);
        let parts: Vec<_> = rel.components().collect();
        let (kind, project, id) = match parts.len() {
            2 => {
                let project = parts[0].as_os_str().to_string_lossy().to_string();
                let id = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                (Kind::Agent, project, id)
            }
            4 => {
                let project = parts[0].as_os_str().to_string_lossy().to_string();
                let sub_dir = parts[2].as_os_str().to_string_lossy();
                if sub_dir != "subagents" {
                    continue;
                }
                let id = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                (Kind::Subagent, project, id)
            }
            _ => continue,
        };
        let scan = scan_jsonl(p);
        let (mut summary, agent_name, team_name) = (scan.summary, scan.agent_name, scan.team_name);
        let cwd = scan.cwd;
        // For subagents, prefer reading cwd from the parent session file if missing,
        // and prefer the meta.json's friendly description as the summary.
        let (cwd, summary, agent_type) = if kind == Kind::Subagent {
            let parent_session = root
                .join(&project)
                .join(format!("{}.jsonl", parts[1].as_os_str().to_string_lossy()));
            let cwd = cwd.or_else(|| scan_jsonl(&parent_session).cwd);
            let (desc, at) = read_agent_meta(p);
            (cwd, desc.or(summary.take()), at)
        } else {
            (cwd, summary, None)
        };
        let summary = summary.unwrap_or_else(|| id.clone());
        out.push(Entry {
            kind,
            project,
            id,
            path: p.to_path_buf(),
            mtime,
            summary,
            cwd,
            tmux_session: None,
            tmux_socket: None,
            running: false,
            agent_type,
            agent_name,
            team_name,
        });
    }
    // Match running claude processes to their JSONL entry: normal sessions by
    // session-id (UUID == file stem), swarm teammates by agentName + teamName.
    for (key, loc) in running_claudes() {
        let found = out.iter_mut().find(|e| {
            e.kind == Kind::Agent
                && match &key {
                    ClaudeKey::Session(sid) => &e.id == sid,
                    ClaudeKey::Agent { name, team } => {
                        e.agent_name.as_deref() == Some(name)
                            && e.team_name.as_deref() == Some(team)
                    }
                }
        });
        if let Some(e) = found {
            e.running = true;
            if let Some((session, socket)) = loc {
                e.tmux_session = Some(session);
                e.tmux_socket = socket;
            }
        }
    }
    // Propagate live-ness to subagents whose parent agent is currently running and
    // whose transcript was touched in the last 60s (i.e. still being appended to).
    let running_agents: std::collections::HashMap<String, (Option<String>, Option<String>)> = out
        .iter()
        .filter(|e| e.kind == Kind::Agent && e.running)
        .map(|e| {
            (
                e.id.clone(),
                (e.tmux_session.clone(), e.tmux_socket.clone()),
            )
        })
        .collect();
    let now = SystemTime::now();
    let fresh = std::time::Duration::from_secs(60);
    for e in out.iter_mut() {
        if e.kind != Kind::Subagent {
            continue;
        }
        // Path shape: <root>/<project>/<parent-sid>/subagents/agent-<id>.jsonl
        let parent_sid = e
            .path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string());
        let Some(parent_sid) = parent_sid else {
            continue;
        };
        let Some((parent_session, parent_socket)) = running_agents.get(&parent_sid) else {
            continue;
        };
        let is_fresh = now
            .duration_since(e.mtime)
            .map(|d| d < fresh)
            .unwrap_or(false);
        if is_fresh {
            e.running = true;
            e.tmux_session = parent_session.clone();
            e.tmux_socket = parent_socket.clone();
        }
    }
    out.sort_by(|a, b| {
        // Order: live (in tmux) > running > others, then by mtime desc.
        let rank = |e: &Entry| {
            if e.tmux_session.is_some() {
                2
            } else if e.running {
                1
            } else {
                0
            }
        };
        rank(b).cmp(&rank(a)).then(b.mtime.cmp(&a.mtime))
    });
    Ok(out)
}

fn tmux_sessions() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Directory tmux keeps its server sockets in: `$TMUX_TMPDIR`, else `/tmp/tmux-<uid>`.
fn tmux_socket_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("TMUX_TMPDIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    use std::os::unix::fs::MetadataExt;
    let uid = dirs::home_dir()?.metadata().ok()?.uid();
    Some(PathBuf::from(format!("/tmp/tmux-{uid}")))
}

/// Every tmux socket name: the default server plus any named `-L` servers
/// (claude-swarm runs each swarm on its own `claude-swarm-<pid>` socket).
fn tmux_socket_names() -> Vec<String> {
    let mut names: Vec<String> = tmux_socket_dir()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    if !names.iter().any(|n| n == "default") {
        names.push("default".to_string());
    }
    names
}

/// Where a running claude lives in tmux: (session_name, socket_name).
/// socket_name is None for the default server, Some(name) for a named `-L` socket (e.g. a swarm).
type TmuxLoc = (String, Option<String>);

/// How a running claude process identifies which transcript it owns.
#[derive(Debug, Clone)]
pub(crate) enum ClaudeKey {
    /// Normal session: `--session-id <UUID>` matches the JSONL file stem.
    Session(String),
    /// Swarm teammate: `--agent-name <name> --team-name <team>`. The teammate's
    /// JSONL records carry matching `agentName`/`teamName` fields.
    Agent { name: String, team: String },
}

/// True if this process command's binary is a claude CLI. Matches both the
/// `claude`/`claude-code` launcher and the versioned binary swarm teammates
/// exec directly (e.g. `~/.local/share/claude/versions/2.1.156`).
fn is_claude_bin(bin: &str) -> bool {
    let base = Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    base == "claude" || base == "claude-code" || bin.contains("/claude/versions/")
}

/// Value of `--flag <value>` in a whitespace-split command, if present.
fn arg_after<'a>(rest: &'a str, flag: &str) -> Option<&'a str> {
    rest.split_whitespace().skip_while(|t| *t != flag).nth(1)
}

/// For each running claude process, returns (identity, Option<TmuxLoc>).
fn running_claudes() -> Vec<(ClaudeKey, Option<TmuxLoc>)> {
    let ps_out = match Command::new("ps").args(["-axo", "tty=,command="]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let ps_str = String::from_utf8_lossy(&ps_out);
    // Collect (identity, "/dev/<tty>") for each running claude.
    let mut running: Vec<(ClaudeKey, Option<String>)> = Vec::new();
    for line in ps_str.lines() {
        let trimmed = line.trim_start();
        let (tty, rest) = match trimmed.split_once(char::is_whitespace) {
            Some(x) => x,
            None => continue,
        };
        let rest = rest.trim_start();
        let bin = rest.split_whitespace().next().unwrap_or("");
        if !is_claude_bin(bin) {
            continue;
        }
        // Prefer --session-id (normal sessions); fall back to the swarm
        // teammate's --agent-name/--team-name pair.
        let key = if let Some(sid) = arg_after(rest, "--session-id") {
            ClaudeKey::Session(sid.to_string())
        } else if let (Some(name), Some(team)) = (
            arg_after(rest, "--agent-name"),
            arg_after(rest, "--team-name"),
        ) {
            ClaudeKey::Agent {
                name: name.to_string(),
                team: team.to_string(),
            }
        } else {
            continue;
        };
        let tty_dev = if tty == "?" || tty == "??" {
            None
        } else {
            Some(format!("/dev/{tty}"))
        };
        running.push((key, tty_dev));
    }
    if running.is_empty() {
        return Vec::new();
    }
    // Build tty -> (session, socket) map by querying every tmux server socket,
    // not just the default one. Swarm panes live on `claude-swarm-*` sockets.
    let mut tty_to_loc: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();
    for socket in tmux_socket_names() {
        let out = Command::new("tmux")
            .args([
                "-L",
                &socket,
                "list-panes",
                "-aF",
                "#{pane_tty}\t#{session_name}",
            ])
            .output();
        let Ok(o) = out else { continue };
        if !o.status.success() {
            continue;
        }
        let socket_opt = if socket == "default" {
            None
        } else {
            Some(socket.clone())
        };
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((tty, sess)) = line.split_once('\t') {
                tty_to_loc
                    .entry(tty.to_string())
                    .or_insert_with(|| (sess.to_string(), socket_opt.clone()));
            }
        }
    }
    // Resolve each running entry's tty -> (session, socket), or None if not in tmux.
    running
        .into_iter()
        .map(|(key, tty)| {
            let loc = tty.and_then(|t| tty_to_loc.get(&t).cloned());
            (key, loc)
        })
        .collect()
}

fn ghostty_run(cmd: &str) -> Result<()> {
    // Split the current terminal pane (Cmd+D, universal on Ghostty/iTerm2/Warp)
    // and run the command. Avoid AppleScript escaping headaches by stashing the
    // command in a temp script and typing only its path.
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("ccmon-{nanos}.sh"));
    std::fs::write(&tmp, format!("#!/bin/bash\n{cmd}\n"))?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&tmp)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp, perms)?;

    let script = format!(
        r#"tell application "System Events"
    keystroke "d" using command down
    delay 0.35
    keystroke "bash {path} && rm -f {path}"
    keystroke return
end tell"#,
        path = tmp.display(),
    );
    let status = Command::new("osascript").arg("-e").arg(&script).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            println!(
                "\nCouldn't drive the terminal (grant Accessibility permission to your terminal app in System Settings → Privacy & Security).\n\nRun this manually in a new pane:\n\n  bash {}\n",
                tmp.display()
            );
            Ok(())
        }
    }
}

fn spawn(path_arg: Option<&str>) -> Result<()> {
    let target = match path_arg {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()?,
    };
    let target = target.canonicalize().unwrap_or(target);
    let base = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();
    let session = format!("claude-{base}");
    // If a session with this name already exists, just attach instead of creating a duplicate.
    let exists = tmux_sessions().iter().any(|s| s == &session);
    let inner = if exists {
        format!("tmux attach -t '{session}'")
    } else {
        format!(
            "tmux new-session -s '{session}' -c '{cwd}' claude",
            cwd = target.display()
        )
    };
    println!("Launching: {inner}");
    ghostty_run(&inner)
}

fn open_viewer(entry: &Entry) -> Result<()> {
    // If a live tmux session is linked, real-attach in a new Ghostty window.
    // Subagents inherit their parent's tmux_session for display purposes, but
    // attaching to it would attach to the parent agent — so for subagents we
    // always fall through to the transcript tailer.
    if entry.kind == Kind::Agent {
        if let Some(s) = entry.tmux_session.as_deref() {
            let cmd = match entry.tmux_socket.as_deref() {
                Some(sock) => format!("tmux -L '{sock}' attach -t '{s}'"),
                None => format!("tmux attach -t '{s}'"),
            };
            println!("Attaching to tmux session: {s}");
            return ghostty_run(&cmd);
        }
    }
    // Otherwise: transcript tailer.
    let path = entry.path.to_string_lossy().to_string();
    let label = format!(
        "{} {} ({})",
        match entry.kind {
            Kind::Agent => "agent",
            Kind::Subagent => "subagent",
        },
        decode_project(&entry.project),
        DateTime::<Local>::from(entry.mtime).format("%H:%M"),
    );
    // Inner shell command: print a header, then tail -f the file piped through jq for readability if available, else raw.
    let jq_filter = r#"select(.type=="user" or .type=="assistant") | (.type[0:1]) as $tag | if (.message.content|type)=="string" then "[\($tag)] \(.message.content)" elif (.message.content|type)=="array" then .message.content[] | if .type=="text" then "[\($tag)] \(.text)" elif .type=="tool_use" then "[\($tag)] → \(.name)(\(.input|tostring|.[0:160]))" elif .type=="tool_result" then "[\($tag)] ← \((.content|tostring)[0:240])" else empty end else empty end"#;
    let inner = format!(
        "printf '\\033]0;ccmon: {label}\\007'; echo '── ccmon viewer: {label} ──'; echo '{path}'; echo; if command -v jq >/dev/null 2>&1; then tail -n 80 -f '{path}' | jq -rc '{jq}'; else tail -n 80 -f '{path}'; fi",
        label = label.replace('\'', "'\"'\"'"),
        path = path.replace('\'', "'\"'\"'"),
        jq = jq_filter.replace('\'', "'\"'\"'"),
    );
    ghostty_run(&inner)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) == Some("spawn") {
        return spawn(args.get(1).map(|s| s.as_str()));
    }

    let home = dirs::home_dir().context("no home dir")?;
    let root = home.join(".claude").join("projects");
    if !root.exists() {
        if args.iter().any(|a| a == "--list" || a == "-l") {
            return Ok(());
        }
        println!("No Claude Code state found at {}.", root.display());
        return Ok(());
    }

    let max_age_hours: u64 = std::env::var("CCMON_MAX_AGE_HOURS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let entries = discover(&root, max_age_hours)?;

    if args.iter().any(|a| a == "--list" || a == "-l") {
        println!("{}", HEADER);
        for e in &entries {
            println!("  {e}");
        }
        return Ok(());
    }

    if entries.is_empty() {
        println!(
            "No claude agents or subagents active in the last {}h.\n(Override with CCMON_MAX_AGE_HOURS.)",
            max_age_hours
        );
        return Ok(());
    }

    let picked = tui::run(entries, root, max_age_hours)?;
    if let Some(p) = picked {
        open_viewer(&p)?;
    }
    Ok(())
}
