#!/usr/bin/env python3
"""ferry - manage Claude Code chats across local accounts.

  ./ferry-cli.py ui                    serve the app UI at localhost:7777
  ./ferry-cli.py list                  print accounts + chat counts
  ./ferry-cli.py vault                 archive every chat + transcript into ~/.ferry
  ./ferry-cli.py export <text|id> [md|txt|json]   save a chat to ~/Downloads
  ./ferry-cli.py import <text|id> <account>       add a CLI or VS Code chat
                                                  to an account
  ./ferry-cli.py folder <text|id> <path>          point a chat at the folder
                                                  it belongs to
  ./ferry-cli.py cursor                           list Cursor's conversations
  ./ferry-cli.py cursor-import <text|id> <account>  convert one into a Claude chat

Chats started in the CLI or in VS Code have no per-account record, so Claude
lists them nowhere. "list" shows them under the accounts; "import" gives one a
record in the account you name. The transcript itself is never moved.

Writes are refused while the Claude desktop app is running; every mutation
snapshots the affected file into the vault first.
"""
import json, os, re, shutil, sys, glob, subprocess, threading, webbrowser
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer

HOME  = (os.environ.get("USERPROFILE") or os.path.expanduser("~")) \
        if sys.platform == "win32" else os.path.expanduser("~")

def claude_dir_windows():
    """The Microsoft Store build of Claude runs in an MSIX container that silently
    redirects %APPDATA%\\Claude to
    %LOCALAPPDATA%\\Packages\\Claude_<publisher>\\LocalCache\\Roaming\\Claude.
    Only processes inside the package see the redirect, so a standalone Ferry
    finds %APPDATA%\\Claude empty. Look in both; if more than one has chats,
    take the one written to most recently."""
    appdata = os.environ.get("APPDATA", os.path.join(HOME, "AppData", "Roaming"))
    local   = os.environ.get("LOCALAPPDATA", os.path.join(HOME, "AppData", "Local"))
    cands = [os.path.join(appdata, "Claude")] + sorted(glob.glob(
        os.path.join(local, "Packages", "Claude_*", "LocalCache", "Roaming", "Claude")))
    live = [c for c in cands if os.path.isdir(os.path.join(c, "claude-code-sessions"))]
    if not live: return cands[0]
    def newest(c):
        recs = glob.glob(os.path.join(c, "claude-code-sessions", "*", "*", "local_*.json"))
        return max((os.path.getmtime(f) for f in recs), default=-1)
    return max(live, key=newest)

if sys.platform == "win32":
    CLAUDE = claude_dir_windows()
elif sys.platform == "darwin":
    CLAUDE = f"{HOME}/Library/Application Support/Claude"
else:
    CLAUDE = f"{HOME}/.config/Claude"
SESS  = f"{CLAUDE}/claude-code-sessions"
PROJ  = f"{HOME}/.claude/projects"
CFG   = f"{CLAUDE}/config.json"
IDB   = f"{CLAUDE}/IndexedDB"
VAULT = f"{HOME}/.ferry"
LABELS= f"{VAULT}/labels.json"
PREFS = f"{VAULT}/prefs.json"
PORT  = 7777

# account-scoped fields that must NOT follow a chat into another account
ACCOUNT_SCOPED = ("remoteMcpServersConfig", "enabledMcpTools")

def under(root, p):
    """Windows mixes / and \\ depending on the source, so normalise both ends."""
    a = os.path.normcase(os.path.normpath(root))
    b = os.path.normcase(os.path.normpath(p))
    return b == a or b.startswith(a + os.sep)

def scope_of(f):
    """(account, org) of a file at .../claude-code-sessions/<account>/<org>/<file>.
    Split on the OS separator: glob on Windows joins with backslashes."""
    parts = os.path.normpath(f).split(os.sep)
    return parts[-3], parts[-2]

def enc_cwd(p):
    """Current rule, taken from the shipped CLI: every non-alphanumeric -> '-'."""
    return re.sub(r"[^a-zA-Z0-9]", "-", p)

def enc_cwd_legacy(p):
    """Older builds replaced only the separators and kept dots."""
    return re.sub(r"[/\\]", "-", p)

def project_dir(cwd):
    """Folders from different Claude Code versions coexist; long paths get
    truncated with a -<hash> suffix. Try each shape before giving up."""
    cur = enc_cwd(cwd)
    for cand in (cur, enc_cwd_legacy(cwd)):
        d = os.path.join(PROJ, cand)
        if os.path.isdir(d): return d
    try:
        for name in os.listdir(PROJ):
            prefix = name.rsplit("-", 1)[0]
            if len(prefix) >= 24 and cur.startswith(prefix):
                return os.path.join(PROJ, name)
    except Exception: pass
    return os.path.join(PROJ, cur)
def ts(ms):
    try: return datetime.fromtimestamp(ms/1000).strftime("%Y-%m-%d %H:%M")
    except Exception: return "?"

def app_running():
    if sys.platform == "win32":
        # Chromium holds <user data>\lockfile open, unshared, while the app runs
        # and Windows deletes it on exit; a PowerShell process query took ~2 s
        try: open(f"{CLAUDE}/lockfile", "rb").close(); return False
        except PermissionError: return True
        except OSError: return False
    try:
        out = subprocess.run(["pgrep","-f","Claude.app/Contents/MacOS/Claude"],
                             capture_output=True, text=True).stdout.strip()
        return bool(out)
    except Exception: return False

def current_account():
    try: return json.load(open(CFG, encoding="utf-8")).get("lastKnownAccountUuid")
    except Exception: return None

def export_dir():
    try:
        d = json.load(open(PREFS)).get("lastExportDir")
        if d and os.path.isdir(d): return d
    except Exception: pass
    return os.path.expanduser("~/Downloads")

def set_export_dir(d):
    os.makedirs(VAULT, exist_ok=True)
    try: p = json.load(open(PREFS))
    except Exception: p = {}
    p["lastExportDir"] = d
    json.dump(p, open(PREFS,"w"), indent=1)

def labels():
    try: return json.load(open(LABELS))
    except Exception: return {}

def set_label(acct, name):
    os.makedirs(VAULT, exist_ok=True)
    d = labels(); d[acct] = name
    json.dump(d, open(LABELS,"w"), indent=1)

def profiles():
    """Best-effort: pull account uuid -> email/name from the app's IndexedDB.
    Only the currently logged-in account is ever present; logout clears it."""
    found = {}
    pat = re.compile(
        rb"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
        rb".{0,24}?email_address.{0,4}?([A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,})"
        # Claude's own UI shows display_name ("ahkamboh"), not full_name
        # ("Ali Hamza Kamboh"), so capture both and prefer the former
        rb"(?:.{0,16}?full_name.{0,4}?([A-Za-z0-9 ._-]{2,40}))?"
        rb"(?:.{0,16}?display_name.{0,4}?([A-Za-z0-9 ._-]{2,40}))?", re.S)
    for root,_,files in os.walk(IDB):
        for fn in files:
            try: blob = open(os.path.join(root,fn),"rb").read()
            except Exception: continue
            for m in pat.finditer(blob):
                uuid = m.group(1).decode()
                disp = (m.group(4) or b"").decode().strip()
                full = (m.group(3) or b"").decode().strip()
                found[uuid] = {"email": m.group(2).decode(), "name": disp or full}
    return known_profiles(found)     # the same files exist on macOS

PROFILES = f"{VAULT}/profiles.json"

def known_profiles(found):
    """IndexedDB only knows the signed-in account, and Claude keeps it locked
    while it runs. Claude Code's own config names its account too ("oauthAccount"
    in ~/.claude.json and its backups), and account switchers such as claude-swap
    keep one copy per account. Every account seen is remembered in
    ~/.ferry/profiles.json, so it keeps its name after sign-out."""
    try: known = json.load(open(PROFILES, encoding="utf-8"))
    except Exception: known = {}
    before = dict(known)
    files = [f"{HOME}/.claude.json", f"{HOME}/.claude.json.backup"]
    files += glob.glob(f"{HOME}/.claude/backups/.claude.json.backup*")
    sw = f"{HOME}/.claude-swap-backup/configs"
    if os.path.isdir(sw): files += [os.path.join(sw, n) for n in os.listdir(sw) if n.endswith(".json")]
    from_json = {}                     # a parsed field beats a scraped one
    for f in files:
        try: oa = json.load(open(f, encoding="utf-8")).get("oauthAccount") or {}
        except Exception: continue
        if oa.get("accountUuid") and oa.get("emailAddress"):
            from_json[oa["accountUuid"]] = {"email": oa["emailAddress"],
                                            "name": oa.get("displayName") or oa.get("fullName") or ""}
    known.update(from_json)
    for k, v in found.items():         # IndexedDB knows who is signed in right now,
        prev = (known.get(k) or {})    # but its name is scraped out of a binary and
        name = (from_json.get(k) or {}).get("name") \
               or v.get("name") or prev.get("name", "")   # can be truncated
        known[k] = {"email": v.get("email") or prev.get("email", ""), "name": name}
    if known != before:
        os.makedirs(VAULT, exist_ok=True)
        json.dump(known, open(PROFILES, "w", encoding="utf-8"), indent=1)
    return known

def read_rec(path):
    # a session no account owns has no record on disk: describe it from its
    # own transcript, so everything that reads a chat can read one of those too
    if str(path).endswith(".jsonl") and under(PROJ, str(path)):
        return source_rec(path)
    # explicit utf-8: Windows defaults to the ANSI codepage, which fails on
    # non-ASCII titles and silently drops the chat
    try: return json.load(open(path, encoding="utf-8"))
    except Exception: return None

def owned(path):
    """Renaming, copying or deleting needs a record an account owns. A bare
    transcript has none, and is never the thing to write to."""
    if str(path).endswith(".jsonl"):
        raise RuntimeError("this chat is not in an account yet - import it first")
    return path

def transcripts_for(rec):
    """Every transcript file that makes up one chat: current + prior + subagents."""
    cwd = rec.get("cwd","")
    d   = project_dir(cwd)
    ids = [rec.get("cliSessionId")]
    for key in ("priorCliSessionIds", "bridgeSessionIds"):   # key renamed between builds
        for x in (rec.get(key) or []):
            if x not in ids: ids.append(x)
    out = []
    for i in [x for x in ids if x]:
        main = f"{d}/{i}.jsonl"
        subs = sorted(glob.glob(f"{d}/{i}/subagents/*.jsonl"))
        out.append({"id": i, "path": main, "exists": os.path.exists(main),
                    "size": os.path.getsize(main) if os.path.exists(main) else 0,
                    "subagents": [{"path":s,"size":os.path.getsize(s)} for s in subs]})
    return out

# ---------- sessions no account claims ----------
# Claude Code writes a transcript for every session it runs, wherever it runs:
# the CLI, the VS Code extension and the desktop app all append to the same
# ~/.claude/projects tree. Only the desktop app also writes the small
# per-account record Ferry lists, so a chat started in the CLI or in VS Code is
# on disk and belongs to nobody - readable, but invisible to every account.
# These are listed as read-only sources. A chat can be imported out of one into
# an account; nothing is ever written back into them.

SOURCES = {"cli":            ("cli",     "Claude Code CLI"),
           "claude-vscode":  ("vscode",  "VS Code"),
           "claude-desktop": ("desktop", "Desktop, no record"),
           "cursor":         ("cursor",  "Cursor")}
INDEX = f"{VAULT}/sessions.json"
# bumped whenever what is read out of a transcript changes, so an index written
# by an older Ferry is re-read rather than believed
INDEX_V = 3
# fields that describe the account and its environment rather than the chat
INHERIT = ("envScopeId", "permissionMode", "effort", "chromePermissionMode",
           "remoteControlAutoEligible", "classifierSummaryEnabled")

def title_from(text):
    """A chat's name, taken from the first thing the person typed. The desktop
    app titles its own chats in a few words, so a whole opening prompt would
    tower over them in the list: keep the first sentence, and cut that at a
    word. A full stop only ends a sentence when a space follows it, or
    "github.com" and "ocid1.tenancy.oc1" would each end one."""
    one = " ".join(text.split())
    s = one
    for i, ch in enumerate(one):
        if ch in ".!?":
            if i + 1 >= len(one): break        # one sentence: it keeps its mark
            if one[i+1] == " ":
                if 12 <= i <= 70: s = one[:i]
                break
    if len(s) <= 60: return s
    cut = s[:60]
    i = cut.rfind(" ")
    return (cut[:i] if i >= 30 else cut.rstrip()) + "…"

def iso_ms(s):
    """Transcripts date every line in ISO-8601 UTC; records count milliseconds."""
    try:
        d  = datetime.strptime(s[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
        ms = int(s[20:23]) if len(s) >= 23 and s[19] == "." else 0
        return int(d.timestamp()*1000) + ms
    except Exception:
        return None

def read_session(path):
    """Everything a transcript says about itself, in one pass: which surface
    wrote it, where it ran, when it started and stopped, how many turns it took
    and what to call it. Lines over a megabyte are tool output, never metadata,
    so they are never parsed - that keeps a 90 MB transcript cheap to read."""
    try: st = os.stat(path)
    except Exception: return None
    info = {"v": INDEX_V, "entrypoint":"", "cwd":"", "version":"", "branch":"", "model":"",
            "title":"", "turns":0, "size": st.st_size}
    first = last = None
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if len(line) > (1 << 20): continue
                try: d = json.loads(line)
                except Exception: continue
                ty = d.get("type")
                if ty not in ("user", "assistant"): continue
                for k, f in (("entrypoint","entrypoint"), ("cwd","cwd"),
                             ("version","version"), ("branch","gitBranch")):
                    if not info[k] and d.get(f): info[k] = d[f]
                if ty == "assistant" and not info["model"]:
                    info["model"] = ((d.get("message") or {}).get("model")) or ""
                t = d.get("timestamp")
                if t:
                    if first is None: first = t
                    if last is None or t > last: last = t
                # a turn is a prompt the person typed: not a subagent's, and not
                # the harness's own <command-name> and <system-reminder> lines
                if ty == "user" and not d.get("isSidechain") and not d.get("isMeta"):
                    c = (d.get("message") or {}).get("content")
                    if isinstance(c, list):
                        c = " ".join(x.get("text","") for x in c
                                     if isinstance(x, dict) and x.get("type") == "text")
                    c = c.strip() if isinstance(c, str) else ""
                    # "[Request interrupted...]" is written by the harness when you
                    # stop a tool, not typed: neither a turn nor a name for the chat
                    if c and not c.startswith(("<", "[Request interrupted")):
                        info["turns"] += 1
                        if not info["title"]: info["title"] = title_from(c)
    except Exception: pass
    mt = int(st.st_mtime * 1000)
    info["title"]   = info["title"] or "(untitled)"
    info["created"] = iso_ms(first or "") or mt
    info["last"]    = iso_ms(last or "") or mt
    return info

def session_info(cache, path, state):
    """Reading every unclaimed transcript on every scan would mean re-reading
    hundreds of megabytes to learn nothing new, so what each one said about
    itself is kept in the vault and re-read only when the file changes."""
    try: st = os.stat(path)
    except Exception: return None
    hit = cache.get(path)
    if (hit and hit.get("v") == INDEX_V
            and hit.get("size") == st.st_size and hit.get("mtime") == int(st.st_mtime)):
        return hit
    info = read_session(path)
    if not info: return None
    info["mtime"] = int(st.st_mtime)
    cache[path] = info
    state["dirty"] = True
    return info

def subagents_of(d, sid):
    subs = glob.glob(f"{d}/{sid}/subagents/*.jsonl")
    return len(subs), sum(os.path.getsize(s) for s in subs if os.path.exists(s))

def source_rec(path):
    """The record a CLI or VS Code session would have, described from the
    transcript itself. The same shape the rest of the tool already reads."""
    info = read_session(path)
    if not info: raise RuntimeError("transcript unreadable")
    sid = os.path.splitext(os.path.basename(path))[0]
    kind, name = SOURCES.get(info["entrypoint"], ("other", "Other sessions"))
    return {"sessionId": None, "cliSessionId": sid, "title": info["title"],
            "cwd": info["cwd"], "model": info["model"],
            "createdAt": info["created"], "lastActivityAt": info["last"],
            "completedTurns": info["turns"], "isArchived": False,
            "gitBranch": info["branch"], "cliVersion": info["version"],
            "source": kind, "sourceName": name}

def source_scopes(claimed):
    """Every transcript no account's record claims, grouped by the surface that
    wrote it and shaped like an account scope so the rest of the tool can list it."""
    cache, state, groups, live = session_index(), {"dirty": False}, {}, set()
    for path in glob.glob(f"{PROJ}/*/*.jsonl"):
        sid = os.path.splitext(os.path.basename(path))[0]
        live.add(path)
        if sid in claimed: continue
        info = session_info(cache, path, state)
        if not info: continue
        nsub, subb = subagents_of(os.path.dirname(path), sid)
        kind, name = SOURCES.get(info["entrypoint"], ("other", "Other sessions"))
        g = groups.setdefault(kind, {
            "acct": f"source:{kind}", "org": "source", "kind": "source",
            "source": kind, "sourceName": name, "chats": [], "deleted": [],
            "connectors": {}, "cwds": {}, "isCurrent": False, "label": "", "profile": None})
        g["chats"].append({
            "id": "local_" + sid, "sid": sid, "title": info["title"],
            "cwd": info["cwd"], "model": info["model"], "created": info["created"],
            "last": info["last"], "turns": info["turns"], "archived": False,
            "forkedFrom": None, "files": 1, "subs": nsub,
            "bytes": info["size"] + subb, "missing": 0, "absent": 0,
            "branch": info["branch"], "version": info["version"],
            "source": kind, "path": path})
    # forget transcripts retention has since pruned, so the index does not grow
    # forever on a machine that churns through sessions
    for stale in [k for k in cache if k not in live]:
        cache.pop(stale, None); state["dirty"] = True
    if state["dirty"]:
        try:
            os.makedirs(VAULT, exist_ok=True)
            json.dump(cache, open(INDEX, "w", encoding="utf-8"))
        except Exception: pass
    out = list(groups.values())
    for g in out: g["chats"].sort(key=lambda c: c["last"] or 0, reverse=True)
    out.sort(key=lambda g: -(max([c["last"] or 0 for c in g["chats"]], default=0)))
    return out

def session_index():
    try: return json.load(open(INDEX, encoding="utf-8"))
    except Exception: return {}

def scan():
    """Full inventory: every account/org scope, its chats and tombstones."""
    cur, labs, profs = current_account(), labels(), profiles()
    scopes = {}
    claimed = set()          # transcripts some account already answers for
    for f in glob.glob(f"{SESS}/*/*/local_*.json"):
        acct, org = scope_of(f)
        rec = read_rec(f)
        if not rec: continue
        key = f"{acct}|{org}"
        s = scopes.setdefault(key, {"acct":acct,"org":org,"chats":[],"deleted":[],
                                    "connectors":{}, "cwds":{}, "isCurrent":acct==cur,
                                    "label":labs.get(acct,""),
                                    "profile":profs.get(acct)})
        tr = transcripts_for(rec)
        claimed.update(t["id"] for t in tr if t.get("id"))
        s["chats"].append({
            "id": rec.get("sessionId"), "title": rec.get("title") or "(untitled)",
            "cwd": rec.get("cwd",""), "model": rec.get("model",""),
            "created": rec.get("createdAt"), "last": rec.get("lastActivityAt"),
            "turns": rec.get("completedTurns"), "archived": bool(rec.get("isArchived")),
            "forkedFrom": rec.get("forkedFromSessionId"),
            "files": len(tr), "bytes": sum(t["size"]+sum(x["size"] for x in t["subagents"]) for t in tr),
            # only the chat's own transcript counts; bridge ids may have none
            "missing": 1 if (tr and not tr[0]["exists"]) else 0,
            "absent": sum(1 for t in tr if not t["exists"]), "path": f})
        for m in (rec.get("remoteMcpServersConfig") or []):
            if isinstance(m,dict) and m.get("name"):
                s["connectors"][m["name"]] = s["connectors"].get(m["name"],0)+1
        s["cwds"][rec.get("cwd","")] = s["cwds"].get(rec.get("cwd",""),0)+1
    for f in glob.glob(f"{SESS}/*/*/deleted_*"):
        acct, org = scope_of(f)
        key = f"{acct}|{org}"
        s = scopes.setdefault(key, {"acct":acct,"org":org,"chats":[],"deleted":[],
                                    "connectors":{},"cwds":{},"isCurrent":acct==cur,
                                    "label":labs.get(acct,""),"profile":profs.get(acct)})
        sid = "local_" + os.path.basename(f)[len("deleted_"):]
        try: when = int(open(f).read().strip())
        except Exception: when = None
        s["deleted"].append({"id": sid, "when": when, "path": f})
    for s in scopes.values():
        s["chats"].sort(key=lambda c: c["last"] or 0, reverse=True)
        s["deleted"].sort(key=lambda d: d["when"] or 0, reverse=True)
    ordered = sorted(scopes.values(),
                     key=lambda s: -(max([c["last"] or 0 for c in s["chats"]], default=0)))
    # CLI and VS Code sessions come after the accounts: they are where chats are
    # imported from, not an account you can send one to
    return {"exportDir": export_dir(), "scopes": ordered + source_scopes(claimed),
            "current": cur, "appRunning": app_running(), "vault": VAULT}

# ---------- mutations ----------

def snapshot(path, tag):
    if not os.path.exists(path): return None
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    dest  = f"{VAULT}/snapshots/{stamp}-{tag}-{os.path.basename(path)}"
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    shutil.copy2(path, dest)
    return dest

def guard(force=False):
    if app_running() and not force:
        raise RuntimeError("Claude desktop is running - quit it first, or use Force.")

def scope_dir(acct, org): return f"{SESS}/{acct}/{org}"

def op_copy(src_path, dst_acct, dst_org, move=False, force=False):
    guard(force); owned(src_path)
    rec = read_rec(src_path)
    if not rec: raise RuntimeError("source chat unreadable")
    sid = rec["sessionId"]
    dst_dir = scope_dir(dst_acct, dst_org); os.makedirs(dst_dir, exist_ok=True)
    dst = f"{dst_dir}/{sid}.json"
    out = dict(rec)
    for k in ACCOUNT_SCOPED: out.pop(k, None)   # connector uuids are per-account
    snapshot(dst, "overwrite")
    json.dump(out, open(dst,"w"), indent=1)
    tomb = f"{dst_dir}/deleted_{sid[len('local_'):]}"
    if os.path.exists(tomb): snapshot(tomb,"undelete"); os.remove(tomb)
    if move:
        snapshot(src_path, "moved-out"); os.remove(src_path)
        sd = os.path.dirname(src_path)
        open(f"{sd}/deleted_{sid[len('local_'):]}","w").write(str(int(datetime.now().timestamp()*1000)))
    return {"ok": True, "wrote": dst, "moved": move}

def template_record(d):
    """The newest record the app itself wrote in this account, to copy the
    fields that describe the account rather than the chat."""
    recs = glob.glob(f"{d}/local_*.json")
    if not recs: return {}
    try: return json.load(open(max(recs, key=os.path.getmtime), encoding="utf-8")) or {}
    except Exception: return {}

def op_import(path, acct, org, force=False):
    """Give a CLI or VS Code session the per-account record it never had, so an
    account claims it and it becomes an ordinary chat: listed by Claude, and
    from here on copyable, movable and deletable like any other. The transcript
    is not touched, so the session stays resumable where it came from."""
    guard(force)
    if not (str(path).endswith(".jsonl") and under(PROJ, str(path))):
        raise RuntimeError(f"path is outside the projects folder\n  path: {path}\n  root: {PROJ}")
    info = read_session(path)
    if not info: raise RuntimeError("transcript unreadable")
    if not info["cwd"]: raise RuntimeError("this transcript does not say which folder it ran in")
    sid = os.path.splitext(os.path.basename(path))[0]
    d = scope_dir(acct, org)
    if not os.path.isdir(d): raise RuntimeError("that account has no folder on this machine")
    # The id the chat keeps for good, derived from the session it already has:
    # importing the same session twice updates one record instead of making a
    # second, and a later copy to another account carries the same id.
    rid = "local_" + sid
    rec = {"sessionId": rid, "cliSessionId": sid,
           "cwd": info["cwd"], "originCwd": info["cwd"],
           "title": info["title"], "titleSource": "auto",
           "createdAt": info["created"], "lastActivityAt": info["last"],
           "lastFocusedAt": info["last"], "completedTurns": info["turns"],
           "isArchived": False}
    if info["model"]: rec["model"] = info["model"]
    t = template_record(d)
    for k in INHERIT:
        if t.get(k) is not None: rec[k] = t[k]
    dst = f"{d}/{rid}.json"
    snapshot(dst, "import")
    json.dump(rec, open(dst, "w", encoding="utf-8"), indent=1)
    tomb = f"{d}/deleted_{sid}"
    if os.path.exists(tomb): snapshot(tomb, "undelete"); os.remove(tomb)
    return {"ok": True, "wrote": dst, "id": rid,
            "title": info["title"], "turns": info["turns"]}

def is_scratch(cwd):
    """A chat started without picking a folder runs in a workspace the app makes
    for it, and shows in Claude as having no folder at all."""
    return (not cwd) or "scratch-workspaces" in cwd.lower()

def relink(src, dst):
    """Put one transcript under a second name so it can be found from another
    folder too. A hard link, not a copy: one file, two names, not a byte
    duplicated, and the folder it came from keeps working. Only a volume that
    refuses links falls back to copying. Returns (linked, copied)."""
    if os.path.exists(dst) or not os.path.exists(src): return (0, 0)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    try: os.link(src, dst); return (1, 0)
    except Exception: pass
    try: shutil.copy2(src, dst); return (0, 1)
    except Exception: return (0, 0)

def op_set_folder(path, folder, force=False):
    """Point a chat at a folder. Its cwd is two things at once: the folder Claude
    names in its header and resumes in, and where the conversation is looked up.
    So changing only the cwd would show the new folder and lose the conversation
    with it - every transcript has to be findable under the new name too."""
    guard(force); owned(path)
    rec = read_rec(path)
    if not rec: raise RuntimeError("chat unreadable")
    was    = rec.get("cwd") or ""
    folder = (folder or "").strip()
    if not folder: raise RuntimeError("say which folder to point it at")
    folder = os.path.abspath(folder)
    if not os.path.isdir(folder): raise RuntimeError(f"no such folder: {folder}")
    if folder == was: return {"ok": True, "unchanged": True, "cwd": folder}

    dst_dir = project_dir(folder)
    os.makedirs(dst_dir, exist_ok=True)
    linked = copied = 0
    for t in transcripts_for(rec):
        l, c = relink(t["path"], f"{dst_dir}/{t['id']}.jsonl")
        linked += l; copied += c
        for s in glob.glob(f"{os.path.dirname(t['path'])}/{t['id']}/subagents/*.jsonl"):
            l, c = relink(s, f"{dst_dir}/{t['id']}/subagents/{os.path.basename(s)}")
            linked += l; copied += c
    snapshot(path, "folder")
    rec["cwd"] = folder; rec["originCwd"] = folder
    json.dump(rec, open(path, "w", encoding="utf-8"), indent=1)
    return {"ok": True, "cwd": folder, "was": was, "linked": linked, "copied": copied}

def op_rename(path, title, force=False):
    guard(force); owned(path)
    rec = read_rec(path)
    if not rec: raise RuntimeError("chat unreadable")
    snapshot(path, "rename")
    rec["title"] = title
    json.dump(rec, open(path,"w"), indent=1)
    return {"ok": True, "title": title}

def op_delete(path, force=False):
    guard(force); owned(path)
    rec = read_rec(path); sid = rec["sessionId"]
    snapshot(path, "delete")
    d = os.path.dirname(path)
    open(f"{d}/deleted_{sid[len('local_'):]}","w").write(str(int(datetime.now().timestamp()*1000)))
    os.remove(path)
    return {"ok": True}

def op_undelete(acct, org, sid, force=False):
    """Restore from the vault, or from any other account that still has it."""
    guard(force)
    short = sid[len("local_"):]
    dst_dir = scope_dir(acct, org)
    src = None
    for cand in glob.glob(f"{SESS}/*/*/{sid}.json"):
        src = cand; break
    if not src:
        v = sorted(glob.glob(f"{VAULT}/chats/*/{sid}.json"))
        if v: src = v[-1]
    if not src: raise RuntimeError("no surviving copy found in any account or the vault")
    rec = read_rec(src)
    for k in ACCOUNT_SCOPED: rec.pop(k, None)
    json.dump(rec, open(f"{dst_dir}/{sid}.json","w"), indent=1)
    tomb = f"{dst_dir}/deleted_{short}"
    if os.path.exists(tomb): snapshot(tomb,"undelete"); os.remove(tomb)
    return {"ok": True, "from": src}

def op_vault():
    """Archive every chat record and every transcript (subagents included)."""
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    base  = f"{VAULT}/chats/{stamp}"; os.makedirs(base, exist_ok=True)
    n_rec = n_tr = 0; nbytes = 0
    for f in glob.glob(f"{SESS}/*/*/local_*.json"):
        acct, _ = scope_of(f)
        d = f"{base}/{acct}"; os.makedirs(d, exist_ok=True)
        shutil.copy2(f, d); n_rec += 1
        rec = read_rec(f)
        if not rec: continue
        for t in transcripts_for(rec):
            if t["exists"]:
                td = f"{VAULT}/transcripts"; os.makedirs(td, exist_ok=True)
                dst = f"{td}/{t['id']}.jsonl"
                if not os.path.exists(dst) or os.path.getmtime(t["path"]) > os.path.getmtime(dst):
                    shutil.copy2(t["path"], dst); n_tr += 1; nbytes += t["size"]
            for s in t["subagents"]:
                sd = f"{VAULT}/transcripts/{t['id']}/subagents"; os.makedirs(sd, exist_ok=True)
                dst = f"{sd}/{os.path.basename(s['path'])}"
                if not os.path.exists(dst):
                    shutil.copy2(s["path"], dst); n_tr += 1; nbytes += s["size"]
    # sweep the whole projects tree so orphaned + subagent transcripts are kept too
    for src in glob.glob(f"{PROJ}/*/**/*.jsonl", recursive=True):
        rel = os.path.relpath(src, PROJ)
        dst = f"{VAULT}/projects/{rel}"
        try:
            if os.path.exists(dst) and os.path.getmtime(dst) >= os.path.getmtime(src): continue
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(src, dst); n_tr += 1; nbytes += os.path.getsize(src)
        except Exception: pass
    return {"ok": True, "records": n_rec, "transcripts": n_tr,
            "mb": round(nbytes/2**20,1), "dir": base}

def transcript_head(path, n=40):
    rows = []
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                try: d = json.loads(line)
                except Exception: continue
                if d.get("type") not in ("user","assistant"): continue
                m = d.get("message",{}) or {}
                c = m.get("content")
                if isinstance(c, list):
                    c = " ".join(x.get("text","") for x in c
                                 if isinstance(x,dict) and x.get("type")=="text")
                if isinstance(c,str) and c.strip() and not c.startswith("<"):
                    rows.append({"role": d.get("type"), "t": (d.get("timestamp") or "")[:16],
                                 "text": c.strip()[:600]})
    except Exception: pass
    return rows[:n]

# ---------- web ----------
# The browser UI is the same dist/index.html the desktop app ships, with a shim
# that turns window.__TAURI__.core.invoke(cmd, args) into POST /api/invoke.

HERE = os.path.dirname(os.path.abspath(__file__))

SHIM = """<script>
window.__TAURI__={core:{invoke:async(cmd,args)=>{
  if(cmd==="delete_chat" && !window.confirm(
      "Remove this chat from this account?\\n\\nThe conversation stays on disk and in the archive."))
    return {ok:false,cancelled:true};
  const r=await fetch("/api/invoke",{method:"POST",
    headers:{"content-type":"application/json"},body:JSON.stringify({cmd:cmd,args:args||{}})});
  const j=await r.json();
  if(j && j.__error) throw new Error(j.__error);
  return j;
}}};
</script>
"""

def page():
    f = os.path.join(HERE, "dist", "index.html")
    if not os.path.exists(f):
        return b"<h1>dist/index.html not found</h1><p>Run this from the ferry checkout.</p>"
    html = open(f, encoding="utf-8").read()
    anchor = "<script>\nconst iv="
    if anchor in html:
        html = html.replace(anchor, SHIM + anchor, 1)
    return html.encode()

def chat_detail(path):
    rec = read_rec(path)
    if not rec: raise RuntimeError("chat unreadable")
    tr = transcripts_for(rec)
    msgs = full_msgs(rec)
    return {"rec": rec, "files": tr,
            "bytes": sum(t["size"] for t in tr),
            "subs": sum(len(t["subagents"]) for t in tr),
            "msgs": msgs}

# the same command names the desktop app calls
OPS = {
    "scan":          lambda **k: scan(),
    "chat_detail":   lambda path, **k: chat_detail(path),
    "export_chat":   lambda **k: op_export(**k),
    "copy_chat":     lambda path, acct, org, mv=False, **k: op_copy(path, acct, org, move=mv),
    "import_session":lambda path, acct, org, **k: op_import(path, acct, org),
    # the browser has no native folder panel, so the UI asks for the path itself
    "set_folder":    lambda path, folder=None, **k: op_set_folder(path, folder),
    "rename_chat":   lambda path, title, **k: op_rename(path, title),
    "delete_chat":   lambda path, **k: op_delete(path),
    "undelete_chat": lambda acct, org, id, **k: op_undelete(acct, org, id),
    "set_label":     lambda acct, name, **k: (set_label(acct, name), {"ok": True})[1],
    "run_vault":     lambda **k: op_vault(),
}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, obj, code=200, ctype="application/json"):
        b = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(b))); self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path.split("?")[0] == "/":
            return self._send(page(), 200, "text/html; charset=utf-8")
        self._send({"__error": "not found"}, 404)

    def do_POST(self):
        if self.path != "/api/invoke":
            return self._send({"__error": "not found"}, 404)
        n = int(self.headers.get("Content-Length") or 0)
        try: req = json.loads(self.rfile.read(n) or b"{}")
        except Exception: return self._send({"__error": "bad json"}, 400)
        cmd, args = req.get("cmd"), (req.get("args") or {})
        fn = OPS.get(cmd)
        if not fn: return self._send({"__error": f"unknown command {cmd!r}"}, 400)
        # a transcript is a legitimate target now: it is what a source chat is
        p = args.get("path")
        if p and not (under(SESS, str(p)) or
                      (str(p).endswith(".jsonl") and under(PROJ, str(p)))):
            return self._send({"__error": f"path is outside the sessions and projects folders"
                                          f"\n  path: {p}\n  roots: {SESS}\n         {PROJ}"}, 400)
        try:
            return self._send(fn(**args))
        except TypeError as e:
            return self._send({"__error": f"{cmd}: {e}"}, 400)
        except Exception as e:
            return self._send({"__error": str(e)}, 400)

def full_msgs(rec):
    """Every turn across all this chat's transcripts, oldest session first."""
    tr = transcripts_for(rec)
    order = list(reversed(tr[1:])) + tr[:1]
    out = []
    for t in order:
        if not t["exists"]: continue
        try: lines = open(t["path"], encoding="utf-8", errors="replace").read().splitlines()
        except Exception: continue
        for line in lines:
            try: d = json.loads(line)
            except Exception: continue
            if d.get("type") not in ("user", "assistant"): continue
            c = (d.get("message") or {}).get("content")
            text, tools = "", []
            if isinstance(c, str): text = c
            elif isinstance(c, list):
                for part in c:
                    if not isinstance(part, dict): continue
                    if part.get("type") == "text":
                        text = (text + "\n\n" + part.get("text","")) if text else part.get("text","")
                    elif part.get("type") == "tool_use":
                        inp = part.get("input") or {}
                        hint = inp.get("command") or inp.get("file_path") or inp.get("pattern") or inp.get("description") or ""
                        tools.append(f"{part.get('name','tool')} - {hint[:90]}" if hint else part.get("name","tool"))
            text = (text or "").strip()
            if not text and not tools: continue
            if text.startswith(("<command-name>", "<local-command", "<system-reminder>")): continue
            out.append({"role": d.get("type"), "t": (d.get("timestamp") or "")[:16],
                        "text": text, "tools": tools})
    return out

def slug(x):
    out = re.sub(r"[^A-Za-z0-9]+", "-", x).strip("-").lower()
    return out[:60] or "chat"

def build_export(rec, fmt):
    msgs = full_msgs(rec)
    title = rec.get("title", "chat")
    if fmt == "json":
        body = json.dumps({"title": title, "sessionId": rec["sessionId"], "cwd": rec.get("cwd"),
                           "model": rec.get("model"), "messages": msgs}, indent=1)
    elif fmt == "txt":
        body = f"{title}\n{'='*len(title)}\n\n" + "".join(
            f"[{m['t']}] {'You' if m['role']=='user' else 'Claude'}\n{m['text']}\n\n" for m in msgs)
    else:
        body = (f"# {title}\n\n- model: `{rec.get('model','?')}`\n- folder: `{rec.get('cwd','?')}`\n"
                f"- turns: {rec.get('completedTurns',0)}\n- transcripts: {len(transcripts_for(rec))}\n\n---\n\n")
        for m in msgs:
            body += f"### {'You' if m['role']=='user' else 'Claude'} · {m['t']}\n\n"
            if m["text"]: body += m["text"] + "\n\n"
            for x in m["tools"]: body += f"> `{x}`\n"
            if m["tools"]: body += "\n"
    return body, msgs

def op_export(path, fmt="md", **_):
    """Browser UI has no native save panel, so write to the remembered folder."""
    rec = read_rec(path)
    if not rec: raise RuntimeError("chat unreadable")
    body, msgs = build_export(rec, fmt)
    ext = fmt if fmt in ("md","txt","json") else "md"
    # a session no account owns has no record id yet: name it after the one it has
    short = (rec.get("sessionId") or "local_" + (rec.get("cliSessionId") or ""))[len("local_"):][:8]
    d = export_dir(); os.makedirs(d, exist_ok=True)
    dest = f"{d}/{slug(rec.get('title','chat'))}-{short}.{ext}"
    open(dest,"w",encoding="utf-8").write(body)
    return {"ok": True, "file": dest, "dir": d, "name": os.path.basename(dest),
            "messages": len(msgs), "kb": round(os.path.getsize(dest)/1024)}

def cmd_export(query, fmt="md"):
    hits = []
    for f in glob.glob(f"{SESS}/*/*/local_*.json"):
        rec = read_rec(f)
        if not rec: continue
        if query.lower() in (rec.get("title","")).lower() or query in rec.get("sessionId",""):
            hits.append((f, rec))
    if not hits: return print(f"no chat matching {query!r}")
    if len(hits) > 1:
        seen = {}
        for f, r in hits: seen[r["sessionId"]] = r["title"]
        if len(seen) > 1:
            print("matches more than one chat:")
            for sid, t in seen.items(): print(f"   {t}   [{sid[6:14]}]")
            return
    f, rec = hits[0]
    body, msgs = build_export(rec, fmt)
    ext = fmt if fmt in ("md","txt","json") else "md"
    short = rec["sessionId"][len("local_"):][:8]
    d = export_dir(); os.makedirs(d, exist_ok=True)
    dest = f"{d}/{slug(rec.get('title','chat'))}-{short}.{ext}"
    open(dest, "w", encoding="utf-8").write(body)
    print(f"{len(msgs)} messages -> {dest}  ({os.path.getsize(dest)/1024:.0f} KB)")

def cmd_list():
    st = scan()
    print(f"vault: {VAULT}   app running: {'YES (writes blocked)' if st['appRunning'] else 'no'}\n")
    for s in st["scopes"]:
        if s.get("kind") == "source":
            # chats Claude Code wrote outside the desktop app, owned by nobody
            print(f"{s['sourceName']}   (not in an account)")
            print(f"   {len(s['chats'])} chats - add one with: "
                  f"{os.path.basename(__file__)} import <text|id> <account>")
            for c in s["chats"][:5]:
                print(f"     - {c['title'][:58]:60} {ts(c['last'])}  [{c['sid'][:8]}]")
            print()
            continue
        who = s["profile"]["email"] if s["profile"] else (s["label"] or "unidentified")
        cur = "  <= CURRENT" if s["isCurrent"] else ""
        print(f"{s['acct'][:8]} / {s['org'][:8]}  {who}{cur}")
        print(f"   {len(s['chats'])} chats, {len(s['deleted'])} deleted")
        print(f"   connectors: {', '.join(list(s['connectors'])[:6]) or '-'}")
        for c in s["chats"][:5]:
            print(f"     - {c['title'][:58]:60} {ts(c['last'])}")
        print()

def cmd_import(query, who):
    """Add a CLI or VS Code chat to an account. The chat is named by title or
    session id, the account by email, nickname or the start of its uuid."""
    st = scan()
    hits = [(c, s) for s in st["scopes"] if s.get("kind") == "source"
                   for c in s["chats"]
                   if query.lower() in c["title"].lower() or c["sid"].startswith(query)]
    if not hits: return print(f"no chat outside an account matching {query!r}")
    if len({c["sid"] for c, _ in hits}) > 1:
        print("matches more than one chat:")
        for c, s in hits: print(f"   {c['title'][:58]:60} [{c['sid'][:8]}]  {s['sourceName']}")
        return
    chat, src = hits[0]

    w = who.lower()
    accounts = [s for s in st["scopes"] if s.get("kind") != "source"]
    want = [s for s in accounts
            if w in ((s["profile"] or {}).get("email","") or "").lower()
            or w in (s["label"] or "").lower() or s["acct"].lower().startswith(w)]
    if not want:
        print(f"no account matching {who!r}. Accounts on this machine:")
        for s in accounts:
            print(f"   {s['acct'][:8]}  {(s['profile'] or {}).get('email') or s['label'] or '-'}")
        return
    if len(want) > 1:
        print("matches more than one account:")
        for s in want: print(f"   {s['acct'][:8]}  {(s['profile'] or {}).get('email') or s['label'] or '-'}")
        return
    t = want[0]
    r = op_import(chat["path"], t["acct"], t["org"])
    name = (t["profile"] or {}).get("email") or t["label"] or t["acct"][:8]
    print(f"added {r['title'][:58]!r} ({r['turns']} turns) from {src['sourceName']} to {name}")
    print(f"  -> {r['wrote']}")

def cmd_folder(query, folder):
    """Point a chat at the folder it belongs to."""
    hits = []
    for f in glob.glob(f"{SESS}/*/*/local_*.json"):
        rec = read_rec(f)
        if not rec: continue
        if query.lower() in (rec.get("title","")).lower() or query in (rec.get("sessionId") or ""):
            hits.append((f, rec))
    if not hits: return print(f"no chat matching {query!r}")
    if len({r["sessionId"] for _, r in hits}) > 1:
        print("matches more than one chat:")
        for f, r in hits: print(f"   {r.get('title','')[:58]:60} [{r['sessionId'][6:14]}]")
        return
    f, rec = hits[0]
    r = op_set_folder(f, folder)
    if r.get("unchanged"): return print("already in that folder")
    print(f"{rec.get('title','(untitled)')[:58]}")
    print(f"  was: {r['was'] or '(no folder)'}")
    print(f"  now: {r['cwd']}")
    print(f"  transcripts: {r['linked']} linked, {r['copied']} copied")

# ---------- Cursor ----------
# Cursor is a separate application with a storage of its own: one SQLite file
# holding every conversation, not a folder of transcripts. A chat is a row in
# composerHeaders, an ordered list of bubble ids in composerData:<id>, and one
# bubbleId:<chat>:<bubble> row per message. Nothing about it resembles the
# JSONL Claude Code appends, so a chat cannot be moved between them - it has to
# be converted, and the conversion only goes one way. Writing into Cursor's
# database would mean inserting rows into a live 1 GB file that Cursor holds
# open, where a mistake costs every conversation in it, so Ferry never does.
# Every read here is mode=ro.

if sys.platform == "win32":
    CURSOR = os.path.join(os.environ.get("APPDATA", ""), "Cursor", "User")
elif sys.platform == "darwin":
    CURSOR = f"{HOME}/Library/Application Support/Cursor/User"
else:
    CURSOR = f"{HOME}/.config/Cursor/User"

def cursor_db():
    p = os.path.join(CURSOR, "globalStorage", "state.vscdb")
    return p if os.path.exists(p) else None

def cursor_ro(path):
    """Read-only, and never anything else."""
    import sqlite3
    return sqlite3.connect("file:%s?mode=ro" % path.replace("\\", "/"), uri=True)

def cursor_folders():
    """Which folder each Cursor workspace is: workspace.json names it as a URI."""
    import urllib.parse
    out = {}
    for f in glob.glob(os.path.join(CURSOR, "workspaceStorage", "*", "workspace.json")):
        try: j = json.load(open(f, encoding="utf-8"))
        except Exception: continue
        uri = j.get("folder") or ""
        if not uri.startswith("file:///"): continue
        p = urllib.parse.unquote(uri[len("file:///"):])
        p = p.replace("/", os.sep) if sys.platform == "win32" else "/" + p
        out[os.path.basename(os.path.dirname(f))] = p
    return out

def cursor_chats():
    """Every Cursor conversation that has anything in it. Most headers are empty
    shells left behind by windows that were opened and closed."""
    db = cursor_db()
    if not db: return []
    ws, out = cursor_folders(), []
    c = cursor_ro(db)
    try:
        rows = list(c.execute("""select composerId, workspaceId, createdAt, lastUpdatedAt,
                                        isArchived, isSubagent, value
                                 from composerHeaders order by lastUpdatedAt desc"""))
    except Exception:
        return []
    for cid, wid, created, updated, arch, sub, val in rows:
        if sub: continue                      # a subagent's own side conversation
        r = c.execute("select value from cursorDiskKV where key=?",
                      ("composerData:" + cid,)).fetchone()
        if not r: continue
        try: data = json.loads(r[0]) or {}
        except Exception: continue
        heads = data.get("fullConversationHeadersOnly") or []
        if not heads: continue
        # most bubbles are tool calls with nothing to read; each header says
        # whether its bubble has text, so how much was said can be counted
        # without opening three thousand rows
        said = sum(1 for h in heads if (h.get("grouping") or {}).get("hasText")) or len(heads)
        try: name = (json.loads(val) or {}).get("name") or ""
        except Exception: name = ""
        out.append({"id": cid, "title": name or "(unnamed)",
                    "folder": ws.get(str(wid), ""), "created": created,
                    "last": updated, "archived": bool(arch),
                    "bubbles": len(heads), "said": said})
    return out

def _tool_line(t):
    """One tool call, said in a line. Cursor keeps the arguments as raw JSON;
    the path or command in them is the part worth reading."""
    name = t.get("name") or t.get("tool") or "tool"
    hint = ""
    try:
        a = json.loads(t.get("rawArgs") or "{}")
        if isinstance(a, dict):
            for k in ("path", "target_file", "file", "command", "query", "pattern",
                      "globPattern", "targetDirectory", "toolName", "explanation"):
                if a.get(k): hint = str(a[k]); break
            if not hint:
                # whatever is left, minus Cursor's own bookkeeping - call ids say
                # nothing about what the tool did and carry newlines of their own
                rest = {k: v for k, v in a.items()
                        if k not in ("toolCallId", "modelCallId", "toolIndex", "toolCallBinary")}
                if rest: hint = json.dumps(rest)
    except Exception: pass
    hint = " ".join(str(hint).split())            # a tool call is one line
    return f"-> {name}({hint[:120]})" if hint else f"-> {name}()"

def cursor_messages(cid):
    """One Cursor conversation as plain turns, oldest first.

    Most bubbles carry no prose at all - in a 4,563 bubble chat only a couple of
    hundred do, and three thousand are tool calls. Dropping those would throw
    away what the conversation actually did, so a run of them is folded into the
    message before it as one line each. They are written as text, not as
    tool_use blocks: a tool_use has to be answered by a tool_result or the
    conversation is malformed, and there is nothing here to answer it with."""
    db = cursor_db()
    if not db: return []
    c = cursor_ro(db)
    r = c.execute("select value from cursorDiskKV where key=?", ("composerData:" + cid,)).fetchone()
    if not r: return []
    heads = (json.loads(r[0]) or {}).get("fullConversationHeadersOnly") or []
    out, pending = [], []

    def flush(ts):
        if not pending: return
        out.append({"role": "assistant", "text": "\n".join(pending), "t": ts})
        pending.clear()

    for h in heads:
        b = c.execute("select value from cursorDiskKV where key=?",
                      ("bubbleId:%s:%s" % (cid, h.get("bubbleId")),)).fetchone()
        if not b: continue
        try: d = json.loads(b[0]) or {}
        except Exception: continue
        ts = h.get("createdAt") or d.get("createdAt") or ""
        text = (d.get("text") or "").strip()
        role = "user" if d.get("type") == 1 else "assistant"
        if text:
            if role == "user": flush(ts)
            elif pending: text = "\n".join(pending) + "\n\n" + text; pending.clear()
            out.append({"role": role, "text": text, "t": ts})
        elif d.get("toolFormerData"):
            pending.append(_tool_line(d["toolFormerData"]))
    flush(out[-1]["t"] if out else "")
    return out

def write_transcript(path, msgs, cwd, sid):
    """A Claude Code transcript, written from turns that came from somewhere
    else. The shape is the one Claude Code appends: one JSON object a line,
    each linked to the one before it. entrypoint says where it really came from
    so nothing later mistakes it for a session Claude Code ran itself."""
    import uuid as _uuid
    os.makedirs(os.path.dirname(path), exist_ok=True)
    prev = None
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        for m in msgs:
            u = str(_uuid.uuid4())
            fh.write(json.dumps({
                "parentUuid": prev, "isSidechain": False, "userType": "external",
                "type": m["role"], "message": {"role": m["role"], "content": m["text"]},
                "uuid": u, "timestamp": m["t"], "cwd": cwd, "sessionId": sid,
                "gitBranch": "", "entrypoint": "cursor",
            }, ensure_ascii=False) + "\n")
            prev = u
    return path

def op_cursor_import(cid, acct, org, force=False):
    """Convert one Cursor conversation into a Claude chat.

    This is the one place Ferry writes a transcript rather than only the little
    record beside it, because there is no transcript to point at - Cursor keeps
    its conversations in a database. The file is named after the Cursor
    conversation, so converting the same chat twice rewrites the one file
    instead of leaving a second copy."""
    guard(force)
    hit = [x for x in cursor_chats() if x["id"] == cid or x["id"].startswith(cid)]
    if not hit: raise RuntimeError(f"no Cursor chat {cid!r}")
    chat = hit[0]
    if not chat["folder"]:
        raise RuntimeError("that Cursor chat has no folder on this machine")
    if not os.path.isdir(scope_dir(acct, org)):
        raise RuntimeError("that account has no folder on this machine")
    msgs = cursor_messages(chat["id"])
    if not msgs: raise RuntimeError("that Cursor chat has nothing readable in it")

    dst = os.path.join(project_dir(chat["folder"]), chat["id"] + ".jsonl")
    write_transcript(dst, msgs, chat["folder"], chat["id"])
    rec = op_import(dst, acct, org, force=force)
    # the record's title comes from the first prompt; Cursor already named it
    if chat["title"] and chat["title"] != "(unnamed)":
        op_rename(rec["wrote"], chat["title"], force=force)
        rec["title"] = chat["title"]
    rec["messages"] = len(msgs)
    rec["transcript"] = dst
    return rec

def cmd_cursor():
    if not cursor_db():
        return print(f"no Cursor storage here\n  looked in: {CURSOR}")
    chats = cursor_chats()
    if not chats: return print("Cursor is installed but has no conversations with anything in them")
    by = {}
    for x in chats: by.setdefault(x["folder"] or "(no folder)", []).append(x)
    print(f"{len(chats)} Cursor chats   ({cursor_db()})\n")
    for folder, xs in by.items():
        print(folder)
        for x in xs:
            print("  %-9s %-40s %4d turns of %5d bubbles  %s%s" %
                  (x["id"][:8], x["title"][:40], x["said"], x["bubbles"], ts(x["last"]),
                   "  (archived)" if x["archived"] else ""))
        print()
    print("add one to an account with:  ferry-cli.py cursor-import <id|text> <account>")

def cmd_cursor_import(query, who):
    chats = [x for x in cursor_chats()
             if x["id"].startswith(query) or query.lower() in x["title"].lower()]
    if not chats: return print(f"no Cursor chat matching {query!r}")
    if len(chats) > 1:
        print("matches more than one chat:")
        for x in chats: print("   %-9s %s" % (x["id"][:8], x["title"][:58]))
        return
    chat = chats[0]
    st = scan()
    w = who.lower()
    accounts = [s for s in st["scopes"] if s.get("kind") != "source"]
    want = [s for s in accounts
            if w in ((s["profile"] or {}).get("email", "") or "").lower()
            or w in (s["label"] or "").lower() or s["acct"].lower().startswith(w)]
    if len(want) != 1:
        print("no single account matching %r. Accounts here:" % who)
        for s in accounts:
            print("   %-9s %s" % (s["acct"][:8], (s["profile"] or {}).get("email") or s["label"] or "-"))
        return
    t = want[0]
    r = op_cursor_import(chat["id"], t["acct"], t["org"])
    name = (t["profile"] or {}).get("email") or t["label"] or t["acct"][:8]
    print(f"{r['title']!r}")
    print(f"  {r['messages']} turns from Cursor -> {name}")
    print(f"  folder     : {chat['folder']}")
    print(f"  transcript : {r['transcript']}")
    print(f"  record     : {r['wrote']}")

def cmd_ui():
    srv = HTTPServer(("127.0.0.1", PORT), H)
    url = f"http://127.0.0.1:{PORT}/"
    print(f"ferry -> {url}   (ctrl-c to stop)")
    threading.Timer(0.6, lambda: webbrowser.open(url)).start()
    try: srv.serve_forever()
    except KeyboardInterrupt: print("\nstopped")

if __name__ == "__main__":
    a = sys.argv[1] if len(sys.argv)>1 else "ui"
    if   a=="list":  cmd_list()
    elif a=="vault": print(json.dumps(op_vault(), indent=1))
    elif a=="export": cmd_export(sys.argv[2], sys.argv[3] if len(sys.argv)>3 else "md")
    elif a=="import":
        if len(sys.argv) < 4: print("usage: ferry-cli.py import <text|id> <account>")
        else: cmd_import(sys.argv[2], sys.argv[3])
    elif a=="folder":
        if len(sys.argv) < 4: print("usage: ferry-cli.py folder <text|id> <path>")
        else: cmd_folder(sys.argv[2], sys.argv[3])
    elif a=="cursor": cmd_cursor()
    elif a=="cursor-import":
        if len(sys.argv) < 4: print("usage: ferry-cli.py cursor-import <text|id> <account>")
        else: cmd_cursor_import(sys.argv[2], sys.argv[3])
    elif a=="ui":    cmd_ui()
    else: print(__doc__)
