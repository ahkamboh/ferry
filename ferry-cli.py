#!/usr/bin/env python3
"""ferry - manage Claude Code chats across local accounts.

  ./ferry-cli.py ui                    serve the app UI at localhost:7777
  ./ferry-cli.py list                  print accounts + chat counts
  ./ferry-cli.py vault                 archive every chat + transcript into ~/.ferry
  ./ferry-cli.py export <text|id> [md|txt|json]   save a chat to ~/Downloads

Writes are refused while the Claude desktop app is running; every mutation
snapshots the affected file into the vault first.
"""
import json, os, re, shutil, sys, glob, subprocess, threading, webbrowser
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer

HOME  = os.path.expanduser("~")
if sys.platform == "win32":
    CLAUDE = os.path.join(os.environ.get("APPDATA",
                          os.path.join(HOME, "AppData", "Roaming")), "Claude")
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
    try:
        if sys.platform == "win32":
            # both the app and the CLI are claude.exe, so match the install path
            out = subprocess.run(["powershell","-NoProfile","-Command",
                "(Get-Process -Name Claude -ErrorAction SilentlyContinue | "
                "Where-Object { $_.Path -like '*WindowsApps*' }).Count"],
                capture_output=True, text=True).stdout.strip()
            return out.isdigit() and int(out) > 0
        out = subprocess.run(["pgrep","-f","Claude.app/Contents/MacOS/Claude"],
                             capture_output=True, text=True).stdout.strip()
        return bool(out)
    except Exception: return False

def current_account():
    try: return json.load(open(CFG)).get("lastKnownAccountUuid")
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
        rb"(?:.{0,12}?full_name.{0,4}?([A-Za-z0-9 ._-]{2,40}))?", re.S)
    for root,_,files in os.walk(IDB):
        for fn in files:
            try: blob = open(os.path.join(root,fn),"rb").read()
            except Exception: continue
            for m in pat.finditer(blob):
                uuid = m.group(1).decode()
                found[uuid] = {"email": m.group(2).decode(),
                               "name": (m.group(3) or b"").decode().strip()}
    return found

def read_rec(path):
    try: return json.load(open(path))
    except Exception: return None

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

def scan():
    """Full inventory: every account/org scope, its chats and tombstones."""
    cur, labs, profs = current_account(), labels(), profiles()
    scopes = {}
    for f in glob.glob(f"{SESS}/*/*/local_*.json"):
        acct, org = f.split("/")[-3], f.split("/")[-2]
        rec = read_rec(f)
        if not rec: continue
        key = f"{acct}|{org}"
        s = scopes.setdefault(key, {"acct":acct,"org":org,"chats":[],"deleted":[],
                                    "connectors":{}, "cwds":{}, "isCurrent":acct==cur,
                                    "label":labs.get(acct,""),
                                    "profile":profs.get(acct)})
        tr = transcripts_for(rec)
        s["chats"].append({
            "id": rec.get("sessionId"), "title": rec.get("title") or "(untitled)",
            "cwd": rec.get("cwd",""), "model": rec.get("model",""),
            "created": rec.get("createdAt"), "last": rec.get("lastActivityAt"),
            "turns": rec.get("completedTurns"), "archived": bool(rec.get("isArchived")),
            "forkedFrom": rec.get("forkedFromSessionId"),
            "files": len(tr), "bytes": sum(t["size"]+sum(x["size"] for x in t["subagents"]) for t in tr),
            "missing": sum(1 for t in tr if not t["exists"]), "path": f})
        for m in (rec.get("remoteMcpServersConfig") or []):
            if isinstance(m,dict) and m.get("name"):
                s["connectors"][m["name"]] = s["connectors"].get(m["name"],0)+1
        s["cwds"][rec.get("cwd","")] = s["cwds"].get(rec.get("cwd",""),0)+1
    for f in glob.glob(f"{SESS}/*/*/deleted_*"):
        acct, org = f.split("/")[-3], f.split("/")[-2]
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
    return {"exportDir": export_dir(), "scopes": sorted(scopes.values(), key=lambda s: -(max([c["last"] or 0 for c in s["chats"]], default=0)),),
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
    guard(force)
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

def op_rename(path, title, force=False):
    guard(force)
    rec = read_rec(path)
    if not rec: raise RuntimeError("chat unreadable")
    snapshot(path, "rename")
    rec["title"] = title
    json.dump(rec, open(path,"w"), indent=1)
    return {"ok": True, "title": title}

def op_delete(path, force=False):
    guard(force)
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
        acct = f.split("/")[-3]
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
        for key in ("path",):
            if args.get(key) and not str(args[key]).startswith(SESS):
                return self._send({"__error": "bad path"}, 400)
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
    short = rec["sessionId"][len("local_"):][:8]
    d = export_dir(); os.makedirs(d, exist_ok=True)
    dest = f"{d}/{slug(rec.get('title','chat'))}-{short}.{ext}"
    open(dest,"w").write(body)
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
    open(dest, "w").write(body)
    print(f"{len(msgs)} messages -> {dest}  ({os.path.getsize(dest)/1024:.0f} KB)")

def cmd_list():
    st = scan()
    print(f"vault: {VAULT}   app running: {'YES (writes blocked)' if st['appRunning'] else 'no'}\n")
    for s in st["scopes"]:
        who = s["profile"]["email"] if s["profile"] else (s["label"] or "unidentified")
        cur = "  <= CURRENT" if s["isCurrent"] else ""
        print(f"{s['acct'][:8]} / {s['org'][:8]}  {who}{cur}")
        print(f"   {len(s['chats'])} chats, {len(s['deleted'])} deleted")
        print(f"   connectors: {', '.join(list(s['connectors'])[:6]) or '-'}")
        for c in s["chats"][:5]:
            print(f"     - {c['title'][:58]:60} {ts(c['last'])}")
        print()

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
    elif a=="ui":    cmd_ui()
    else: print(__doc__)
