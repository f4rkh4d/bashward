// bashward. checkpoint + rewind for bash side-effects in claude code.
//
// claude code's built-in /rewind tracks edits made through file-editing
// tools. it does not track bash side-effects. if claude runs `rm`, `mv`,
// `dd`, or a `>` redirection, those changes are permanent under /rewind.
// see https://code.claude.com/docs/en/checkpointing.
//
// bashward is a tiny rust binary that hooks into claude code's PreToolUse
// event for the Bash tool, parses the command, identifies write paths, and
// snapshots them via APFS clonefile (macos) before the command runs. each
// transaction is logged. `bashward rewind` lists transactions and restores.
//
// commands:
//   bashward install        write the PreToolUse hook into ~/.claude/settings.json
//   bashward uninstall      remove the hook
//   bashward snap [paths]   take a manual snapshot of the listed paths
//   bashward list           list recent transactions
//   bashward rewind <id>    restore a transaction
//   bashward prune          drop snapshots older than 7d (configurable)
//   bashward prebash        internal: invoked from the hook with the bash command via stdin
//
// only macos APFS for v0.1. linux overlayfs / btrfs snapshot in a later release.

#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::unnecessary_map_or)]

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser, Debug)]
#[command(
    name = "bashward",
    about = "checkpoint and rewind for bash side-effects in claude code",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// write the PreToolUse hook into ~/.claude/settings.json
    Install,
    /// remove the hook from ~/.claude/settings.json
    Uninstall,
    /// take a manual snapshot of the listed paths
    Snap {
        /// paths to snapshot
        paths: Vec<PathBuf>,
        /// optional label
        #[arg(long)]
        label: Option<String>,
    },
    /// list recent transactions
    List {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// restore a transaction by id (full or short prefix)
    Rewind { id: String },
    /// drop snapshots older than the given age in days
    Prune {
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// internal: invoked from the hook with the bash command on stdin
    #[command(hide = true)]
    Prebash,
    /// print resolved paths and a quick health check
    Doctor,
    /// print the full content of one transaction: timestamp, kind, the
    /// bash command that triggered the snapshot (if any), and the list
    /// of paths covered. useful before running `bashward rewind`.
    Show { id: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Transaction {
    id: String,
    timestamp: DateTime<Utc>,
    kind: String,
    label: Option<String>,
    cmd: Option<String>,
    snapshots: Vec<SnapshotEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SnapshotEntry {
    src: PathBuf,
    /// path inside the bashward store where the original lives
    snap: PathBuf,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

fn store_root() -> Result<PathBuf> {
    Ok(home()?.join(".bashward"))
}

fn log_path() -> Result<PathBuf> {
    Ok(store_root()?.join("log.jsonl"))
}

fn snaps_root() -> Result<PathBuf> {
    Ok(store_root()?.join("snaps"))
}

fn settings_path() -> Result<PathBuf> {
    Ok(home()?.join(".claude").join("settings.json"))
}

fn ensure_store() -> Result<()> {
    fs::create_dir_all(store_root()?)?;
    fs::create_dir_all(snaps_root()?)?;
    let log = log_path()?;
    if !log.exists() {
        fs::write(&log, "")?;
    }
    Ok(())
}

fn random_id() -> String {
    // 12-hex-char id from the system clock + getpid xor.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mix = now ^ (pid.rotate_left(17));
    format!("{:012x}", (mix >> 32) & 0xffff_ffff_ffffu128)
}

fn append_transaction(t: &Transaction) -> Result<()> {
    ensure_store()?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path()?)?;
    let line = serde_json::to_string(t)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn read_transactions() -> Result<Vec<Transaction>> {
    ensure_store()?;
    let s = fs::read_to_string(log_path()?)?;
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Transaction>(line) {
            Ok(t) => out.push(t),
            Err(_) => continue,
        }
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("/bin/cp")
        .args(["-c", "-R"])
        .arg(src)
        .arg(dst)
        .status()
        .context("running /bin/cp -c (APFS clonefile)")?;
    if !status.success() {
        anyhow::bail!(
            "/bin/cp -c failed for {} -> {}",
            src.display(),
            dst.display()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("cp")
        .args(["-a"])
        .arg(src)
        .arg(dst)
        .status()
        .context("running cp -a (linux fallback)")?;
    if !status.success() {
        anyhow::bail!("cp -a failed for {} -> {}", src.display(), dst.display());
    }
    Ok(())
}

fn snap_path_for(id: &str, src: &Path) -> Result<PathBuf> {
    // store a snapshot under ~/.bashward/snaps/<id>/<absolute-path>
    let mut p = snaps_root()?;
    p.push(id);
    let stripped = src.strip_prefix("/").unwrap_or(src);
    p.push(stripped);
    Ok(p)
}

fn snapshot_paths(
    paths: &[PathBuf],
    kind: &str,
    label: Option<String>,
    cmd: Option<String>,
) -> Result<Transaction> {
    ensure_store()?;
    let id = random_id();
    let mut entries = Vec::new();
    for p in paths {
        let canon = match fs::canonicalize(p) {
            Ok(c) => c,
            Err(_) => continue, // path does not exist yet (e.g. about to be created); nothing to snapshot
        };
        let dst = snap_path_for(&id, &canon)?;
        match clone_path(&canon, &dst) {
            Ok(()) => entries.push(SnapshotEntry {
                src: canon,
                snap: dst,
            }),
            Err(e) => {
                eprintln!("bashward: skipping {} ({e})", canon.display());
            }
        }
    }
    let t = Transaction {
        id,
        timestamp: Utc::now(),
        kind: kind.to_string(),
        label,
        cmd,
        snapshots: entries,
    };
    append_transaction(&t)?;
    Ok(t)
}

// parse a bash command line for paths it might mutate. heuristic: handles
// the obvious cases (rm/mv/cp dd/sed -i / >redirect/>>append). this is the
// "good enough" scope for v0.1; the goal is "snapshot when in doubt", not
// formal command parsing.
fn extract_write_paths(cmd: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let tokens = shell_split(cmd);

    if tokens.is_empty() {
        return paths;
    }

    // scan for redirection (`> path` and `>> path`) first since redirections
    // can appear anywhere on the command line.
    for w in tokens.windows(2) {
        if w[0] == ">" || w[0] == ">>" || w[0] == "|>" {
            paths.push(PathBuf::from(&w[1]));
        }
        if let Some(rest) = w[0].strip_prefix(">>") {
            if !rest.is_empty() {
                paths.push(PathBuf::from(rest));
            }
        } else if let Some(rest) = w[0].strip_prefix('>') {
            if !rest.is_empty() && !rest.starts_with('&') {
                paths.push(PathBuf::from(rest));
            }
        }
    }

    // first positional after the command is the prog name. classify by it.
    let prog = tokens[0].as_str();
    match prog {
        "rm" | "/bin/rm" => {
            for t in &tokens[1..] {
                if !t.starts_with('-') {
                    paths.push(PathBuf::from(t));
                }
            }
        }
        "mv" | "/bin/mv" | "cp" | "/bin/cp" => {
            // mv/cp: the LAST arg is the destination, prior args are sources.
            // we snapshot all of them since either side can be clobbered.
            for t in &tokens[1..] {
                if !t.starts_with('-') {
                    paths.push(PathBuf::from(t));
                }
            }
        }
        "dd" => {
            for t in &tokens[1..] {
                if let Some(rest) = t.strip_prefix("of=") {
                    paths.push(PathBuf::from(rest));
                }
            }
        }
        "sed" | "/usr/bin/sed" => {
            // -i means in-place. assume any non-option positional after -i is a file.
            let mut saw_inplace = false;
            for t in &tokens[1..] {
                if t.starts_with("-i") {
                    saw_inplace = true;
                    continue;
                }
                if saw_inplace
                    && !t.starts_with('-')
                    && !t.starts_with("s/")
                    && !t.starts_with("'s")
                {
                    paths.push(PathBuf::from(t));
                }
            }
        }
        "truncate" | "tee" => {
            for t in &tokens[1..] {
                if !t.starts_with('-') {
                    paths.push(PathBuf::from(t));
                }
            }
        }
        _ => {}
    }

    // dedup + drop obvious non-files
    paths.sort();
    paths.dedup();
    paths.retain(|p| {
        let s = p.to_string_lossy();
        !s.is_empty() && !s.starts_with('-') && !s.contains('*')
    });
    paths
}

// minimal posix-y tokenizer: splits on whitespace, respects single + double
// quotes. no shell expansion.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
        } else if in_double {
            if c == '"' {
                in_double = false;
            } else if c == '\\' {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '\'' => in_single = true,
                '"' => in_double = true,
                '\\' => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                ' ' | '\t' | '\n' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn cmd_install() -> Result<()> {
    let path = settings_path()?;
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut v: serde_json::Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let exe = std::env::current_exe().context("locating bashward exe")?;
    let exe_str = exe.to_string_lossy().to_string();

    // hook shape per claude code docs:
    //   "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "..." } ] } ] }
    let entry = serde_json::json!({
        "matcher": "Bash",
        "hooks": [ { "type": "command", "command": format!("{exe_str} prebash") } ],
    });

    let hooks = v
        .as_object_mut()
        .context("settings.json is not a json object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .context("settings.json hooks is not an object")?;
    let arr = hooks_obj
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    let arr = arr
        .as_array_mut()
        .context("PreToolUse hook is not an array")?;
    // remove any prior bashward entry to make this idempotent
    arr.retain(|x| {
        x.get("hooks")
            .and_then(|h| h.as_array())
            .map_or(true, |hs| {
                !hs.iter().any(|hh| {
                    hh.get("command")
                        .and_then(|c| c.as_str())
                        .map_or(false, |s| s.contains("bashward") && s.contains("prebash"))
                })
            })
    });
    arr.push(entry);

    fs::write(&path, serde_json::to_string_pretty(&v)?)?;
    println!(
        "wrote PreToolUse hook into {} (using {} prebash)",
        path.display(),
        exe_str
    );
    println!("snapshots will live under {}", store_root()?.display());
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    let path = settings_path()?;
    if !path.exists() {
        println!("no settings.json at {}, nothing to do", path.display());
        return Ok(());
    }
    let mut v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    if let Some(arr) = v
        .pointer_mut("/hooks/PreToolUse")
        .and_then(|p| p.as_array_mut())
    {
        let before = arr.len();
        arr.retain(|x| {
            x.get("hooks")
                .and_then(|h| h.as_array())
                .map_or(true, |hs| {
                    !hs.iter().any(|hh| {
                        hh.get("command")
                            .and_then(|c| c.as_str())
                            .map_or(false, |s| s.contains("bashward") && s.contains("prebash"))
                    })
                })
        });
        let after = arr.len();
        fs::write(&path, serde_json::to_string_pretty(&v)?)?;
        println!("removed {} bashward hook(s)", before - after);
    } else {
        println!("no PreToolUse hooks present");
    }
    Ok(())
}

fn cmd_snap(paths: Vec<PathBuf>, label: Option<String>) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("usage: bashward snap <path> [<path>...]");
    }
    let t = snapshot_paths(&paths, "manual", label, None)?;
    println!(
        "snapped {} path(s) as transaction {}",
        t.snapshots.len(),
        t.id
    );
    Ok(())
}

fn cmd_show(id: String) -> Result<()> {
    let txs = read_transactions()?;
    let matches: Vec<&Transaction> = txs.iter().filter(|t| t.id.starts_with(&id)).collect();
    if matches.is_empty() {
        anyhow::bail!("no transaction starting with {id}");
    }
    if matches.len() > 1 {
        anyhow::bail!(
            "ambiguous prefix {id}, matches {} transactions",
            matches.len()
        );
    }
    let t = matches[0];
    let local: DateTime<Local> = t.timestamp.with_timezone(&Local);
    println!("id        {}", t.id);
    println!("timestamp {}", local.format("%Y-%m-%d %H:%M:%S %Z"));
    println!("kind      {}", t.kind);
    if let Some(label) = &t.label {
        println!("label     {label}");
    }
    if let Some(cmd) = &t.cmd {
        println!("command   {cmd}");
    }
    println!("paths     {}", t.snapshots.len());
    for entry in &t.snapshots {
        let exists = if entry.snap.exists() { "ok" } else { "missing" };
        println!("  {:>8}  {}", exists, entry.src.display());
    }
    Ok(())
}

fn cmd_list(limit: usize) -> Result<()> {
    let txs = read_transactions()?;
    let n = txs.len();
    let start = n.saturating_sub(limit);
    if txs.is_empty() {
        println!("no transactions yet. install the hook with `bashward install`.");
        return Ok(());
    }
    for t in &txs[start..] {
        let local: DateTime<Local> = t.timestamp.with_timezone(&Local);
        let label = t.label.as_deref().unwrap_or("");
        let cmd = t.cmd.as_deref().unwrap_or("");
        println!(
            "{}  {}  {:>7}  {:>2} paths  {}{}",
            &t.id[..8],
            local.format("%Y-%m-%d %H:%M:%S"),
            t.kind,
            t.snapshots.len(),
            if cmd.is_empty() {
                label.to_string()
            } else {
                format!("`{}`", &cmd[..cmd.len().min(60)])
            },
            if cmd.len() > 60 { "..." } else { "" },
        );
    }
    Ok(())
}

fn cmd_rewind(id: String) -> Result<()> {
    let txs = read_transactions()?;
    let mut matches: Vec<&Transaction> = txs.iter().filter(|t| t.id.starts_with(&id)).collect();
    if matches.is_empty() {
        anyhow::bail!("no transaction starting with {id}");
    }
    if matches.len() > 1 {
        anyhow::bail!(
            "ambiguous prefix {id}, matches {} transactions",
            matches.len()
        );
    }
    let t = matches.pop().unwrap();
    println!(
        "rewinding transaction {} ({} path(s))",
        t.id,
        t.snapshots.len()
    );
    for entry in &t.snapshots {
        if entry.src.exists() {
            // remove the current state, then clone the snapshot back
            if entry.src.is_dir() {
                fs::remove_dir_all(&entry.src).ok();
            } else {
                fs::remove_file(&entry.src).ok();
            }
        }
        clone_path(&entry.snap, &entry.src)?;
        println!("  restored {}", entry.src.display());
    }
    Ok(())
}

fn cmd_prune(days: i64) -> Result<()> {
    let cutoff = Utc::now() - chrono::Duration::days(days);
    let txs = read_transactions()?;
    let mut keep = Vec::new();
    let mut dropped = 0usize;
    for t in txs {
        if t.timestamp < cutoff {
            // remove the snapshot tree on disk
            let dir = snaps_root()?.join(&t.id);
            if dir.exists() {
                fs::remove_dir_all(&dir).ok();
            }
            dropped += 1;
        } else {
            keep.push(t);
        }
    }
    let mut s = String::new();
    for t in &keep {
        s.push_str(&serde_json::to_string(t)?);
        s.push('\n');
    }
    fs::write(log_path()?, s)?;
    println!("pruned {dropped} transaction(s) older than {days}d");
    Ok(())
}

fn cmd_prebash() -> Result<()> {
    // claude code passes a json payload on stdin describing the tool call.
    // we extract the bash command, snapshot the paths it might write to,
    // and emit `{"continue": true}` to let the call proceed.
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let cmd = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| {
            v.pointer("/tool_input/command")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        });

    if let Some(c) = cmd {
        let paths = extract_write_paths(&c);
        if !paths.is_empty() {
            let _ = snapshot_paths(&paths, "prebash", None, Some(c));
        }
    }

    println!("{{\"continue\": true}}");
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    println!("bashward doctor");
    println!("  store        {}", store_root()?.display());
    println!("  log          {}", log_path()?.display());
    println!("  settings     {}", settings_path()?.display());
    println!(
        "  settings exists  {}",
        if settings_path()?.exists() {
            "yes"
        } else {
            "no"
        }
    );
    let plat = std::env::consts::OS;
    let snap_method = match plat {
        "macos" => "/bin/cp -c (APFS clonefile)",
        _ => "cp -a (fallback, NOT space-efficient)",
    };
    println!("  os           {plat}");
    println!("  snap method  {snap_method}");
    let txs = read_transactions().unwrap_or_default();
    println!("  transactions {}", txs.len());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Install => cmd_install(),
        Cmd::Uninstall => cmd_uninstall(),
        Cmd::Snap { paths, label } => cmd_snap(paths, label),
        Cmd::List { limit } => cmd_list(limit),
        Cmd::Rewind { id } => cmd_rewind(id),
        Cmd::Prune { days } => cmd_prune(days),
        Cmd::Prebash => cmd_prebash(),
        Cmd::Doctor => cmd_doctor(),
        Cmd::Show { id } => cmd_show(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_basic() {
        assert_eq!(
            shell_split("rm -rf foo bar"),
            vec!["rm", "-rf", "foo", "bar"]
        );
        assert_eq!(
            shell_split("echo \"hello world\""),
            vec!["echo", "hello world"]
        );
        assert_eq!(shell_split("echo 'a b c'"), vec!["echo", "a b c"]);
    }

    #[test]
    fn extract_rm() {
        assert_eq!(
            extract_write_paths("rm -rf src/foo"),
            vec![PathBuf::from("src/foo")]
        );
    }

    #[test]
    fn extract_redirect() {
        assert!(extract_write_paths("echo hi > /tmp/x.txt").contains(&PathBuf::from("/tmp/x.txt")));
        assert!(extract_write_paths("ls >> /tmp/log.txt").contains(&PathBuf::from("/tmp/log.txt")));
    }

    #[test]
    fn extract_mv() {
        let p = extract_write_paths("mv old.txt new.txt");
        assert!(p.contains(&PathBuf::from("old.txt")));
        assert!(p.contains(&PathBuf::from("new.txt")));
    }

    #[test]
    fn extract_dd() {
        assert!(extract_write_paths("dd if=/dev/zero of=/tmp/disk bs=1M")
            .contains(&PathBuf::from("/tmp/disk")));
    }

    #[test]
    fn extract_sed_inplace() {
        let p = extract_write_paths("sed -i 's/foo/bar/' file.txt");
        assert!(p.contains(&PathBuf::from("file.txt")));
    }

    #[test]
    fn extract_safe_command_yields_nothing() {
        assert!(extract_write_paths("ls -la").is_empty());
        assert!(extract_write_paths("git status").is_empty());
    }
}
