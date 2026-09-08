#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! ccvault - manage Claude Code chats across the local accounts on this Mac.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
fn home() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}
#[cfg(not(target_os = "windows"))]
fn home() -> String { std::env::var("HOME").unwrap_or_default() }

/// Compare by path components, never by string prefix: on Windows these paths
/// mix "/" and "\" depending on whether they came from a format! or from glob.
fn under(root: &str, p: &str) -> bool {
    Path::new(p).starts_with(Path::new(root))
}

/// Where the desktop app keeps its per-account state.
#[cfg(target_os = "macos")]
fn claude_dir() -> String { format!("{}/Library/Application Support/Claude", home()) }
#[cfg(target_os = "windows")]
fn claude_dir() -> String {
    std::env::var("APPDATA")
        .map(|a| format!("{}\\Claude", a))
        .unwrap_or_else(|_| format!("{}/AppData/Roaming/Claude", home()))
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn claude_dir() -> String { format!("{}/.config/Claude", home()) }

fn sess()  -> String { format!("{}/claude-code-sessions", claude_dir()) }
fn cfg()   -> String { format!("{}/config.json", claude_dir()) }
fn idb()   -> String { format!("{}/IndexedDB", claude_dir()) }
fn proj()  -> String { format!("{}/.claude/projects", home()) }
fn vault() -> String { format!("{}/.ferry", home()) }

/// Connector uuids and tool grants belong to the account that created them.
const ACCOUNT_SCOPED: [&str; 2] = ["remoteMcpServersConfig", "enabledMcpTools"];

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}
fn stamp() -> String {
    // yyyymmdd-hhmmss without pulling in chrono
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let out = Command::new("date").args(["-r", &secs.to_string(), "+%Y%m%d-%H%M%S"]).output();
    out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
       .filter(|s| !s.is_empty()).unwrap_or_else(|| secs.to_string())
}
/// Current Claude Code rule, read out of the shipped CLI:
/// every character that is not [a-zA-Z0-9] becomes '-', runs are NOT collapsed.
fn enc_cwd(p: &str) -> String {
    p.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}
/// Older builds only replaced the separators and left dots alone.
fn enc_cwd_legacy(p: &str) -> String {
    p.chars().map(|c| if c=='/' || c=='\\' { '-' } else { c }).collect()
}

/// Resolve a chat's working directory to its transcript folder.
/// Folders written by different Claude Code versions coexist on disk, and very
/// long paths are truncated with a "-<hash>" suffix, so try each in turn.
fn project_dir(cwd: &str) -> String {
    let root = proj();
    let current = enc_cwd(cwd);
    for cand in [&current, &enc_cwd_legacy(cwd)] {
        let d = format!("{}/{}", root, cand);
        if Path::new(&d).is_dir() { return d; }
    }
    // truncated long path: "<prefix>-<base36 hash>"
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some((prefix, _hash)) = name.rsplit_once('-') {
                if prefix.len() >= 24 && current.starts_with(prefix) {
                    return format!("{}/{}", root, name);
                }
            }
        }
    }
    format!("{}/{}", root, current)
}
fn read_json(p: &str) -> Option<Value> {
    fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok())
}
#[cfg(target_os = "macos")]
fn app_running() -> bool {
    Command::new("pgrep").args(["-f", "Claude.app/Contents/MacOS/Claude"])
        .output().map(|o| !o.stdout.is_empty()).unwrap_or(false)
}
/// On Windows both the desktop app and the bundled CLI are called claude.exe,
/// so match on the install path instead of the image name.
#[cfg(target_os = "windows")]
fn app_running() -> bool {
    Command::new("powershell")
        .args(["-NoProfile", "-Command",
               "(Get-Process -Name Claude -ErrorAction SilentlyContinue |                 Where-Object { $_.Path -like '*WindowsApps*' }).Count"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().unwrap_or(0) > 0)
        .unwrap_or(false)
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn app_running() -> bool { false }
fn guard() -> Result<(), String> {
    if app_running() { Err("Claude desktop is running. Quit it first, then retry.".into()) }
    else { Ok(()) }
}
fn snapshot(p: &str, tag: &str) {
    if !Path::new(p).exists() { return; }
    let name = Path::new(p).file_name().unwrap_or_default().to_string_lossy().to_string();
    let dir = format!("{}/snapshots", vault());
    let _ = fs::create_dir_all(&dir);
    let _ = fs::copy(p, format!("{}/{}-{}-{}", dir, stamp(), tag, name));
}
fn labels_path() -> String { format!("{}/labels.json", vault()) }
fn prefs_path()  -> String { format!("{}/prefs.json",  vault()) }
fn last_export_dir() -> String {
    read_json(&prefs_path())
        .and_then(|p| p["lastExportDir"].as_str().map(String::from))
        .filter(|d| Path::new(d).is_dir())
        .unwrap_or_else(|| format!("{}/Downloads", home()))
}
fn set_last_export_dir(d: &str) {
    let _ = fs::create_dir_all(vault());
    let mut p = read_json(&prefs_path()).unwrap_or_else(|| json!({}));
    p["lastExportDir"] = json!(d);
    let _ = fs::write(prefs_path(), serde_json::to_string_pretty(&p).unwrap());
}
fn labels() -> Value { read_json(&labels_path()).unwrap_or_else(|| json!({})) }

/// Only the currently logged-in account leaves a profile record; logout clears it.
fn profiles() -> Value {
    let mut out = json!({});
    let re = regex::bytes::Regex::new(
        r"(?s)([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}).{0,24}?email_address.{0,4}?([A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,})(?:.{0,12}?full_name.{0,4}?([A-Za-z0-9 ._\-]{2,40}))?"
    ).unwrap();
    for e in walkdir::WalkDir::new(idb()).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() { continue; }
        let Ok(b) = fs::read(e.path()) else { continue };
        for c in re.captures_iter(&b) {
            let uuid = String::from_utf8_lossy(&c[1]).to_string();
            let email = String::from_utf8_lossy(&c[2]).to_string();
            let name = c.get(3).map(|m| String::from_utf8_lossy(m.as_bytes()).trim().to_string())
                        .unwrap_or_default();
            out[uuid] = json!({ "email": email, "name": name });
        }
    }
    out
}

/// Every transcript that makes up one chat: current + prior sessions + their subagents.
fn transcripts_for(rec: &Value) -> Vec<Value> {
    let cwd = rec["cwd"].as_str().unwrap_or("");
    let dir = project_dir(cwd);
    let mut ids: Vec<String> = vec![];
    if let Some(s) = rec["cliSessionId"].as_str() { ids.push(s.into()); }
    // older builds: priorCliSessionIds. newer / Windows builds: bridgeSessionIds.
    for key in ["priorCliSessionIds", "bridgeSessionIds"] {
        if let Some(a) = rec[key].as_array() {
            for v in a {
                if let Some(x) = v.as_str() {
                    if !ids.iter().any(|e| e == x) { ids.push(x.into()); }
                }
            }
        }
    }
    ids.iter().map(|id| {
        let main = format!("{}/{}.jsonl", dir, id);
        let size = fs::metadata(&main).map(|m| m.len()).unwrap_or(0);
        let mut subs = 0u64; let mut sub_bytes = 0u64;
        if let Ok(rd) = glob::glob(&format!("{}/{}/subagents/*.jsonl", dir, id)) {
            for p in rd.flatten() {
                subs += 1;
                sub_bytes += fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            }
        }
        json!({ "id": id, "path": main, "exists": Path::new(&main).exists(),
                "size": size, "subagents": subs, "subBytes": sub_bytes })
    }).collect()
}

#[tauri::command]
fn scan() -> Value {
    let cur = read_json(&cfg()).and_then(|c| c["lastKnownAccountUuid"].as_str().map(String::from));
    let labs = labels();
    let profs = profiles();
    let mut scopes: std::collections::BTreeMap<String, Value> = Default::default();

    let touch = |scopes: &mut std::collections::BTreeMap<String, Value>, acct: &str, org: &str| {
        scopes.entry(format!("{}|{}", acct, org)).or_insert_with(|| json!({
            "acct": acct, "org": org, "chats": [], "deleted": [],
            "connectors": {}, "isCurrent": cur.as_deref() == Some(acct),
            "label": labs[acct].as_str().unwrap_or(""),
            "profile": if profs[acct].is_null() { Value::Null } else { profs[acct].clone() }
        }));
    };

    if let Ok(paths) = glob::glob(&format!("{}/*/*/local_*.json", sess())) {
        for p in paths.flatten() {
            let parts: Vec<String> = p.iter().map(|s| s.to_string_lossy().to_string()).collect();
            let n = parts.len();
            if n < 3 { continue; }
            let (acct, org) = (parts[n-3].clone(), parts[n-2].clone());
            let Some(rec) = read_json(&p.to_string_lossy()) else { continue };
            touch(&mut scopes, &acct, &org);
            let key = format!("{}|{}", acct, org);
            let tr = transcripts_for(&rec);
            let bytes: u64 = tr.iter().map(|t| t["size"].as_u64().unwrap_or(0) + t["subBytes"].as_u64().unwrap_or(0)).sum();
            let missing = tr.iter().filter(|t| !t["exists"].as_bool().unwrap_or(false)).count();
            let subs: u64 = tr.iter().map(|t| t["subagents"].as_u64().unwrap_or(0)).sum();
            let entry = json!({
                "id": rec["sessionId"], "title": rec["title"].as_str().unwrap_or("(untitled)"),
                "cwd": rec["cwd"], "model": rec["model"],
                "created": rec["createdAt"], "last": rec["lastActivityAt"],
                "turns": rec["completedTurns"], "archived": rec["isArchived"],
                "forkedFrom": rec["forkedFromSessionId"],
                "files": tr.len(), "subs": subs, "bytes": bytes, "missing": missing,
                "path": p.to_string_lossy()
            });
            scopes.get_mut(&key).unwrap()["chats"].as_array_mut().unwrap().push(entry);
            if let Some(ms) = rec["remoteMcpServersConfig"].as_array() {
                for m in ms {
                    if let Some(nm) = m["name"].as_str() {
                        let c = &mut scopes.get_mut(&key).unwrap()["connectors"];
                        let v = c[nm].as_u64().unwrap_or(0) + 1;
                        c[nm] = json!(v);
                    }
                }
            }
        }
    }
    if let Ok(paths) = glob::glob(&format!("{}/*/*/deleted_*", sess())) {
        for p in paths.flatten() {
            let parts: Vec<String> = p.iter().map(|s| s.to_string_lossy().to_string()).collect();
            let n = parts.len();
            if n < 3 { continue; }
            let (acct, org) = (parts[n-3].clone(), parts[n-2].clone());
            touch(&mut scopes, &acct, &org);
            let base = parts[n-1].clone();
            let short = base.trim_start_matches("deleted_").to_string();
            let when: Option<u64> = fs::read_to_string(&p).ok().and_then(|s| s.trim().parse().ok());
            let key = format!("{}|{}", acct, org);
            scopes.get_mut(&key).unwrap()["deleted"].as_array_mut().unwrap()
                .push(json!({ "id": format!("local_{}", short), "when": when,
                              "path": p.to_string_lossy() }));
        }
    }
    let mut list: Vec<Value> = scopes.into_values().collect();
    for s in list.iter_mut() {
        let c = s["chats"].as_array_mut().unwrap();
        c.sort_by_key(|x| std::cmp::Reverse(x["last"].as_u64().unwrap_or(0)));
        let d = s["deleted"].as_array_mut().unwrap();
        d.sort_by_key(|x| std::cmp::Reverse(x["when"].as_u64().unwrap_or(0)));
    }
    list.sort_by_key(|s| std::cmp::Reverse(
        s["chats"].as_array().unwrap().iter().map(|c| c["last"].as_u64().unwrap_or(0)).max().unwrap_or(0)));
    json!({ "scopes": list, "current": cur, "appRunning": app_running(),
            "vault": vault(), "exportDir": last_export_dir(),
            "paths": { "sessions": sess(), "projects": proj(), "home": home() } })
}

/// Walk every transcript that belongs to one chat, oldest session first.
fn collect_msgs(tr: &[Value], cap: usize, per_msg: usize) -> Vec<Value> {
    let mut ordered: Vec<&Value> = tr.iter().skip(1).collect();
    ordered.reverse();
    if let Some(first) = tr.first() { ordered.push(first); }

    let mut msgs: Vec<Value> = vec![];
    for t in ordered {
        if !t["exists"].as_bool().unwrap_or(false) { continue; }
        let Ok(txt) = fs::read_to_string(t["path"].as_str().unwrap_or("")) else { continue };
        for line in txt.lines() {
            if msgs.len() >= cap { break; }
            let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
            let ty = d["type"].as_str().unwrap_or("");
            if ty != "user" && ty != "assistant" { continue; }
            let stamp = d["timestamp"].as_str().unwrap_or("").chars().take(16).collect::<String>();
            let c = &d["message"]["content"];
            let mut text = String::new();
            let mut tools: Vec<String> = vec![];
            if let Some(sx) = c.as_str() { text.push_str(sx); }
            else if let Some(arr) = c.as_array() {
                for part in arr {
                    match part["type"].as_str().unwrap_or("") {
                        "text" => { if let Some(x) = part["text"].as_str() {
                            if !text.is_empty() { text.push_str("\n\n"); } text.push_str(x); } }
                        "tool_use" => {
                            let name = part["name"].as_str().unwrap_or("tool");
                            let hint = part["input"]["command"].as_str()
                                .or_else(|| part["input"]["file_path"].as_str())
                                .or_else(|| part["input"]["pattern"].as_str())
                                .or_else(|| part["input"]["description"].as_str())
                                .unwrap_or("");
                            tools.push(if hint.is_empty() { name.to_string() }
                                       else { format!("{} — {}", name, hint.chars().take(90).collect::<String>()) });
                        }
                        _ => {}
                    }
                }
            }
            let trimmed = text.trim();
            if trimmed.is_empty() && tools.is_empty() { continue; }
            if trimmed.starts_with("<command-name>") || trimmed.starts_with("<local-command")
               || trimmed.starts_with("<system-reminder>") { continue; }
            msgs.push(json!({ "role": ty, "t": stamp,
                              "text": trimmed.chars().take(per_msg).collect::<String>(),
                              "tools": tools }));
        }
    }
    msgs
}

#[tauri::command]
fn chat_detail(path: String) -> Result<Value, String> {
    if !under(&sess(), &path) {
        return Err(format!("path is outside the sessions folder\n  path: {}\n  root: {}", path, sess()));
    }
    let rec = read_json(&path).ok_or("chat unreadable")?;
    let tr = transcripts_for(&rec);
    let msgs = collect_msgs(&tr, 800, 24000);
    let bytes: u64 = tr.iter().map(|t| t["size"].as_u64().unwrap_or(0)).sum();
    let subs: u64 = tr.iter().map(|t| t["subagents"].as_u64().unwrap_or(0)).sum();
    Ok(json!({ "rec": rec, "files": tr, "bytes": bytes, "subs": subs, "msgs": msgs }))
}

fn slug(s: &str) -> String {
    let mut out: String = s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>().to_lowercase();
    while out.contains("--") { out = out.replace("--", "-"); }
    out.trim_matches('-').chars().take(60).collect()
}

/// Save the whole conversation to ~/Downloads as Markdown, JSON, or plain text.
#[tauri::command]
async fn export_chat(app: tauri::AppHandle, path: String, fmt: String, ask: bool) -> Result<Value, String> {
    if !under(&sess(), &path) {
        return Err(format!("path is outside the sessions folder\n  path: {}\n  root: {}", path, sess()));
    }
    let rec = read_json(&path).ok_or("chat unreadable")?;
    let tr  = transcripts_for(&rec);
    let msgs = collect_msgs(&tr, 100_000, 2_000_000);
    if msgs.is_empty() && fmt != "json" {
        return Err("nothing to download - this chat's transcript was pruned".into());
    }
    let title = rec["title"].as_str().unwrap_or("chat").to_string();
    let sid   = rec["sessionId"].as_str().unwrap_or("");
    let short: String = sid.trim_start_matches("local_").chars().take(8).collect();

    let (body, ext) = match fmt.as_str() {
        "json" => (serde_json::to_string_pretty(&json!({
                      "title": title, "sessionId": sid, "cwd": rec["cwd"],
                      "model": rec["model"], "createdAt": rec["createdAt"],
                      "lastActivityAt": rec["lastActivityAt"],
                      "transcripts": tr, "messages": msgs })).unwrap(), "json"),
        "txt" => {
            let mut o = format!("{}\n{}\n\n", title, "=".repeat(title.len()));
            for m in &msgs {
                o.push_str(&format!("[{}] {}\n{}\n\n",
                    m["t"].as_str().unwrap_or(""),
                    if m["role"]=="user" {"You"} else {"Claude"},
                    m["text"].as_str().unwrap_or("")));
            }
            (o, "txt")
        }
        _ => {
            let mut o = format!("# {}\n\n", title);
            o.push_str(&format!("- model: `{}`\n- folder: `{}`\n- turns: {}\n- transcripts: {}\n\n---\n\n",
                rec["model"].as_str().unwrap_or("?"),
                rec["cwd"].as_str().unwrap_or("?"),
                rec["completedTurns"].as_u64().unwrap_or(0),
                tr.len()));
            for m in &msgs {
                let who = if m["role"]=="user" {"You"} else {"Claude"};
                o.push_str(&format!("### {} · {}\n\n", who, m["t"].as_str().unwrap_or("")));
                let t = m["text"].as_str().unwrap_or("");
                if !t.is_empty() { o.push_str(t); o.push_str("\n\n"); }
                if let Some(tl) = m["tools"].as_array() {
                    for x in tl { o.push_str(&format!("> `{}`\n", x.as_str().unwrap_or(""))); }
                    if !tl.is_empty() { o.push('\n'); }
                }
            }
            (o, "md")
        }
    };

    let fname = format!("{}-{}.{}", slug(&title), short, ext);
    let start = last_export_dir();

    let target: PathBuf = if ask {
        use tauri_plugin_dialog::DialogExt;
        let label = match ext { "json" => "JSON", "txt" => "Plain text", _ => "Markdown" };
        // the panel must be driven by the main thread; this command is not on it,
        // so hand the choice back over a channel instead of blocking.
        let (tx, rx) = std::sync::mpsc::channel();
        app.dialog().file()
            .set_directory(&start)
            .set_file_name(&fname)
            .add_filter(label, &[ext])
            .save_file(move |p| { let _ = tx.send(p); });
        match rx.recv().map_err(|e| e.to_string())? {
            Some(fp) => fp.into_path().map_err(|e| e.to_string())?,
            None => return Ok(json!({ "ok": false, "cancelled": true })),
        }
    } else {
        fs::create_dir_all(&start).map_err(|e| e.to_string())?;
        PathBuf::from(format!("{}/{}", start, fname))
    };

    if let Some(par) = target.parent() {
        fs::create_dir_all(par).map_err(|e| e.to_string())?;
        set_last_export_dir(&par.to_string_lossy());
    }
    fs::write(&target, body).map_err(|e| e.to_string())?;
    let size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    Ok(json!({ "ok": true, "file": target.to_string_lossy(),
               "dir": target.parent().map(|p| p.to_string_lossy().to_string()),
               "name": target.file_name().map(|p| p.to_string_lossy().to_string()),
               "messages": msgs.len(), "kb": (size as f64/1024.0).round() as u64 }))
}

#[tauri::command]
fn copy_chat(path: String, acct: String, org: String, mv: bool) -> Result<Value, String> {
    guard()?;
    if !under(&sess(), &path) {
        return Err(format!("path is outside the sessions folder\n  path: {}\n  root: {}", path, sess()));
    }
    let rec = read_json(&path).ok_or("source unreadable")?;
    let sid = rec["sessionId"].as_str().ok_or("no sessionId")?.to_string();
    let short = sid.trim_start_matches("local_").to_string();
    let dst_dir = format!("{}/{}/{}", sess(), acct, org);
    fs::create_dir_all(&dst_dir).map_err(|e| e.to_string())?;
    let dst = format!("{}/{}.json", dst_dir, sid);

    let mut out = rec.clone();
    if let Some(o) = out.as_object_mut() { for k in ACCOUNT_SCOPED { o.remove(k); } }
    snapshot(&dst, "overwrite");
    fs::write(&dst, serde_json::to_string_pretty(&out).unwrap()).map_err(|e| e.to_string())?;

    let tomb = format!("{}/deleted_{}", dst_dir, short);
    if Path::new(&tomb).exists() { snapshot(&tomb, "undelete"); let _ = fs::remove_file(&tomb); }

    if mv {
        snapshot(&path, "moved-out");
        let src_dir = Path::new(&path).parent().unwrap().to_string_lossy().to_string();
        let _ = fs::remove_file(&path);
        let _ = fs::write(format!("{}/deleted_{}", src_dir, short), now_ms().to_string());
    }
    Ok(json!({ "ok": true, "wrote": dst, "moved": mv }))
}

#[tauri::command]
fn rename_chat(path: String, title: String) -> Result<Value, String> {
    guard()?;
    if !under(&sess(), &path) {
        return Err(format!("path is outside the sessions folder\n  path: {}\n  root: {}", path, sess()));
    }
    let mut rec = read_json(&path).ok_or("chat unreadable")?;
    snapshot(&path, "rename");
    let t = if title.trim().is_empty() { "(untitled)".to_string() } else { title.trim().to_string() };
    rec["title"] = json!(t);
    fs::write(&path, serde_json::to_string_pretty(&rec).unwrap()).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "title": t }))
}

#[tauri::command]
async fn delete_chat(app: tauri::AppHandle, path: String) -> Result<Value, String> {
    guard()?;
    if !under(&sess(), &path) {
        return Err(format!("path is outside the sessions folder\n  path: {}\n  root: {}", path, sess()));
    }
    let rec = read_json(&path).ok_or("chat unreadable")?;

    {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        let title = rec["title"].as_str().unwrap_or("this chat");
        let (tx, rx) = std::sync::mpsc::channel();
        app.dialog()
            .message(format!(
                "Remove \u{201c}{}\u{201d} from this account?\n\n                 The conversation itself stays on disk and in your archive,                  so you can restore it later.", title))
            .title("Delete chat")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Delete".into(), "Cancel".into()))
            .show(move |ok| { let _ = tx.send(ok); });
        if !rx.recv().map_err(|e| e.to_string())? {
            return Ok(json!({ "ok": false, "cancelled": true }));
        }
    }

    let sid = rec["sessionId"].as_str().ok_or("no sessionId")?;
    let short = sid.trim_start_matches("local_");
    snapshot(&path, "delete");
    let dir = Path::new(&path).parent().unwrap().to_string_lossy().to_string();
    let _ = fs::write(format!("{}/deleted_{}", dir, short), now_ms().to_string());
    fs::remove_file(&path).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
fn undelete_chat(acct: String, org: String, id: String) -> Result<Value, String> {
    guard()?;
    let short = id.trim_start_matches("local_").to_string();
    let mut src: Option<PathBuf> = None;
    if let Ok(p) = glob::glob(&format!("{}/*/*/{}.json", sess(), id)) {
        for c in p.flatten() { src = Some(c); break; }
    }
    if src.is_none() {
        if let Ok(p) = glob::glob(&format!("{}/chats/*/*/{}.json", vault(), id)) {
            let mut all: Vec<PathBuf> = p.flatten().collect();
            all.sort(); src = all.pop();
        }
    }
    let src = src.ok_or("no surviving copy in any account or the vault")?;
    let mut rec = read_json(&src.to_string_lossy()).ok_or("copy unreadable")?;
    if let Some(o) = rec.as_object_mut() { for k in ACCOUNT_SCOPED { o.remove(k); } }
    let dir = format!("{}/{}/{}", sess(), acct, org);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(format!("{}/{}.json", dir, id), serde_json::to_string_pretty(&rec).unwrap())
        .map_err(|e| e.to_string())?;
    let tomb = format!("{}/deleted_{}", dir, short);
    if Path::new(&tomb).exists() { snapshot(&tomb, "undelete"); let _ = fs::remove_file(&tomb); }
    Ok(json!({ "ok": true, "from": src.to_string_lossy() }))
}

#[tauri::command]
fn set_label(acct: String, name: String) -> Result<Value, String> {
    let _ = fs::create_dir_all(vault());
    let mut l = labels();
    l[acct] = json!(name.trim());
    fs::write(labels_path(), serde_json::to_string_pretty(&l).unwrap()).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
fn run_vault() -> Result<Value, String> {
    let base = format!("{}/chats/{}", vault(), stamp());
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let (mut recs, mut files, mut bytes) = (0u64, 0u64, 0u64);

    if let Ok(paths) = glob::glob(&format!("{}/*/*/local_*.json", sess())) {
        for p in paths.flatten() {
            let parts: Vec<String> = p.iter().map(|s| s.to_string_lossy().to_string()).collect();
            let acct = parts[parts.len()-3].clone();
            let d = format!("{}/{}", base, acct);
            let _ = fs::create_dir_all(&d);
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if fs::copy(&p, format!("{}/{}", d, name)).is_ok() { recs += 1; }
        }
    }
    // sweep the whole projects tree so orphans and subagent transcripts survive too
    for e in walkdir::WalkDir::new(proj()).into_iter().filter_map(|x| x.ok()) {
        if !e.file_type().is_file() { continue; }
        if e.path().extension().map(|x| x != "jsonl").unwrap_or(true) { continue; }
        let Ok(rel) = e.path().strip_prefix(proj()) else { continue };
        let dst = PathBuf::from(format!("{}/projects/{}", vault(), rel.to_string_lossy()));
        let src_m = e.metadata().ok().and_then(|m| m.modified().ok());
        let dst_m = fs::metadata(&dst).ok().and_then(|m| m.modified().ok());
        if let (Some(s), Some(d)) = (src_m, dst_m) { if d >= s { continue; } }
        if let Some(par) = dst.parent() { let _ = fs::create_dir_all(par); }
        if fs::copy(e.path(), &dst).is_ok() {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(json!({ "ok": true, "records": recs, "transcripts": files,
               "mb": (bytes as f64 / 1048576.0 * 10.0).round() / 10.0, "dir": base }))
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            scan, chat_detail, export_chat, copy_chat, rename_chat, delete_chat,
            undelete_chat, set_label, run_vault
        ])
        .run(tauri::generate_context!())
        .expect("failed to launch Ferry");
}
