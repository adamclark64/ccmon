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
    pub(crate) running: bool,
    pub(crate) agent_type: Option<String>,
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
            .and_then(|c| Path::new(c).file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| decode_project(&self.project));
        let title = truncate(&self.summary, 60);
        write!(
            f,
            "{state:<10}  {age:>4}  {kind:<10}  {proj:<32}  {title}"
        )
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

fn scan_jsonl(path: &Path) -> (Option<String>, Option<String>) {
    // returns (summary, cwd)
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None),
    };
    let reader = BufReader::new(f);
    let mut summary: Option<String> = None;
    let mut cwd: Option<String> = None;
    for line in reader.lines().map_while(Result::ok).take(80) {
        let rec: AnyRecord = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
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
        if summary.is_some() && cwd.is_some() {
            break;
        }
    }
    (summary, cwd)
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

fn discover(root: &Path, max_age_hours: u64) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_hours * 3600));
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
                let id = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                (Kind::Agent, project, id)
            }
            4 => {
                let project = parts[0].as_os_str().to_string_lossy().to_string();
                let sub_dir = parts[2].as_os_str().to_string_lossy();
                if sub_dir != "subagents" {
                    continue;
                }
                let id = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                (Kind::Subagent, project, id)
            }
            _ => continue,
        };
        let (summary, cwd) = scan_jsonl(p);
        // For subagents, prefer reading cwd from the parent session file if missing,
        // and prefer the meta.json's friendly description as the summary.
        let (cwd, summary, agent_type) = if kind == Kind::Subagent {
            let parent_session = root
                .join(&project)
                .join(format!("{}.jsonl", parts[1].as_os_str().to_string_lossy()));
            let cwd = cwd.or_else(|| scan_jsonl(&parent_session).1);
            let (desc, at) = read_agent_meta(p);
            (cwd, desc.or(summary), at)
        } else {
            (cwd, summary, None)
        };
        let summary = summary.unwrap_or_else(|| id.clone());
        out.push(Entry { kind, project, id, path: p.to_path_buf(), mtime, summary, cwd, tmux_session: None, running: false, agent_type });
    }
    // Match running claude processes to JSONL entries by session-id (UUID == file stem).
    for (sid, tmux) in running_claudes() {
        if let Some(e) = out.iter_mut().find(|e| e.id == sid && e.kind == Kind::Agent) {
            e.running = true;
            e.tmux_session = tmux;
        }
    }
    // Propagate live-ness to subagents whose parent agent is currently running and
    // whose transcript was touched in the last 60s (i.e. still being appended to).
    let running_agents: std::collections::HashMap<String, Option<String>> = out
        .iter()
        .filter(|e| e.kind == Kind::Agent && e.running)
        .map(|e| (e.id.clone(), e.tmux_session.clone()))
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
        let Some(parent_sid) = parent_sid else { continue };
        let Some(parent_tmux) = running_agents.get(&parent_sid) else { continue };
        let is_fresh = now.duration_since(e.mtime).map(|d| d < fresh).unwrap_or(false);
        if is_fresh {
            e.running = true;
            e.tmux_session = parent_tmux.clone();
        }
    }
    out.sort_by(|a, b| {
        // Order: live (in tmux) > running > others, then by mtime desc.
        let rank = |e: &Entry| {
            if e.tmux_session.is_some() { 2 }
            else if e.running { 1 }
            else { 0 }
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

/// For each running `claude` CLI process, returns (session_id, tmux_session_name_if_in_tmux).
/// Session ID comes from the process's `--session-id <UUID>` arg, which matches the JSONL filename.
fn running_claudes() -> Vec<(String, Option<String>)> {
    let ps_out = match Command::new("ps")
        .args(["-axo", "tty=,command="])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let ps_str = String::from_utf8_lossy(&ps_out);
    // Collect (session_id, "/dev/<tty>") for each running claude.
    let mut running: Vec<(String, Option<String>)> = Vec::new();
    for line in ps_str.lines() {
        let trimmed = line.trim_start();
        let (tty, rest) = match trimmed.split_once(char::is_whitespace) {
            Some(x) => x,
            None => continue,
        };
        let rest = rest.trim_start();
        let bin = rest.split_whitespace().next().unwrap_or("");
        let base = Path::new(bin).file_name().and_then(|s| s.to_str()).unwrap_or("");
        if base != "claude" && base != "claude-code" {
            continue;
        }
        // Extract --session-id <UUID>.
        let sid = rest
            .split_whitespace()
            .skip_while(|t| *t != "--session-id")
            .nth(1)
            .map(|s| s.to_string());
        let Some(sid) = sid else { continue };
        let tty_dev = if tty == "?" || tty == "??" {
            None
        } else {
            Some(format!("/dev/{tty}"))
        };
        running.push((sid, tty_dev));
    }
    if running.is_empty() {
        return Vec::new();
    }
    // Build tty -> tmux session map.
    let tmux_out = Command::new("tmux")
        .args(["list-panes", "-aF", "#{pane_tty}\t#{session_name}"])
        .output();
    let mut tty_to_session: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Ok(o) = tmux_out {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some((tty, sess)) = line.split_once('\t') {
                    tty_to_session.insert(tty.to_string(), sess.to_string());
                }
            }
        }
    }
    // Resolve each running entry's tty -> tmux session (or None).
    running
        .into_iter()
        .map(|(sid, tty)| {
            let tmux = tty.and_then(|t| tty_to_session.get(&t).cloned());
            (sid, tmux)
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
            let cmd = format!("tmux attach -t '{s}'");
            println!("Attaching to tmux session: {s}");
            return ghostty_run(&cmd);
        }
    }
    // Otherwise: transcript tailer.
    let path = entry.path.to_string_lossy().to_string();
    let label = format!(
        "{} {} ({})",
        match entry.kind { Kind::Agent => "agent", Kind::Subagent => "subagent" },
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

    let picked = tui::run(entries, max_age_hours)?;
    if let Some(p) = picked {
        open_viewer(&p)?;
    }
    Ok(())
}
