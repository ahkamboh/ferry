#!/usr/bin/env python3
"""ferry - manage Claude Code chats across local accounts.

  ./ferry.py ui              open the web UI (default)
  ./ferry.py list            print accounts + chat counts
  ./ferry.py vault           archive every chat + transcript into ~/.ferry

Writes are refused while the Claude desktop app is running; every mutation
snapshots the affected file into the vault first.
"""
import json, os, re, shutil, sys, glob, subprocess, threading, webbrowser
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer

HOME  = os.path.expanduser("~")
SESS  = f"{HOME}/Library/Application Support/Claude/claude-code-sessions"
PROJ  = f"{HOME}/.claude/projects"
CFG   = f"{HOME}/Library/Application Support/Claude/config.json"
IDB   = f"{HOME}/Library/Application Support/Claude/IndexedDB"
VAULT = f"{HOME}/.ferry"
LABELS= f"{VAULT}/labels.json"
PORT  = 7777

# account-scoped fields that must NOT follow a chat into another account
ACCOUNT_SCOPED = ("remoteMcpServersConfig", "enabledMcpTools")

def enc_cwd(p): return re.sub(r"[/._]", "-", p)
def ts(ms):
    try: return datetime.fromtimestamp(ms/1000).strftime("%Y-%m-%d %H:%M")
    except Exception: return "?"

def app_running():
    try:
        out = subprocess.run(["pgrep","-f","Claude.app/Contents/MacOS/Claude"],
                             capture_output=True, text=True).stdout.strip()
        return bool(out)
    except Exception: return False

def current_account():
    try: return json.load(open(CFG)).get("lastKnownAccountUuid")
    except Exception: return None

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
    d   = f"{PROJ}/{enc_cwd(cwd)}"
    ids = [rec.get("cliSessionId")] + list(rec.get("priorCliSessionIds") or [])
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
    return {"scopes": sorted(scopes.values(), key=lambda s: -(max([c["last"] or 0 for c in s["chats"]], default=0)),),
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

PAGE = r"""<!doctype html><html><head><meta charset=utf-8>
<title>ferry</title><meta name=viewport content="width=device-width,initial-scale=1">
<style>
:root{--bg:#faf9f7;--panel:#fff;--ink:#1a1a19;--dim:#6b6b68;--line:#e3e1dd;
      --accent:#b8532a;--ok:#2f6b46;--warn:#8a5a00;--sel:#f0ede8}
@media(prefers-color-scheme:dark){:root{--bg:#161614;--panel:#1e1d1b;--ink:#eceae6;
      --dim:#9a978f;--line:#33312d;--accent:#e07a4a;--ok:#6bbf8e;--warn:#d9a441;--sel:#2a2825}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);
 font:13px/1.5 ui-sans-serif,-apple-system,"SF Pro Text",system-ui,sans-serif}
header{display:flex;align-items:center;gap:12px;padding:10px 16px;border-bottom:1px solid var(--line);
 background:var(--panel);position:sticky;top:0;z-index:5}
h1{font-size:14px;margin:0;letter-spacing:.02em}
h1 small{color:var(--dim);font-weight:400;margin-left:6px}
button{font:inherit;padding:5px 11px;border:1px solid var(--line);border-radius:6px;
 background:var(--panel);color:var(--ink);cursor:pointer}
button:hover{border-color:var(--accent)}
button.p{background:var(--accent);color:#fff;border-color:var(--accent)}
button:disabled{opacity:.4;cursor:not-allowed}
.warn{background:#fdf3e0;color:#7a4d00;border:1px solid #e8cf9a;padding:6px 12px;
 border-radius:6px;font-size:12px}
@media(prefers-color-scheme:dark){.warn{background:#3a2f18;color:#e8c583;border-color:#5c4a22}}
main{display:grid;grid-template-columns:280px 1fr 400px;gap:0;height:calc(100vh - 49px)}
section{overflow:auto;padding:12px}
#accts{border-right:1px solid var(--line)}
#detail{border-left:1px solid var(--line);background:var(--panel)}
.card{border:1px solid var(--line);border-radius:8px;padding:10px;margin-bottom:8px;
 cursor:pointer;background:var(--panel)}
.card:hover{border-color:var(--accent)}
.card.on{border-color:var(--accent);background:var(--sel)}
.card b{display:block;font-size:12.5px}
.card .m{color:var(--dim);font-size:11px;margin-top:3px}
.tag{display:inline-block;font-size:10px;padding:1px 6px;border:1px solid var(--line);
 border-radius:99px;color:var(--dim);margin:2px 3px 0 0}
.tag.cur{border-color:var(--ok);color:var(--ok)}
.row{display:flex;align-items:center;gap:8px;padding:7px 9px;border-radius:6px;cursor:pointer;
 border:1px solid transparent}
.row:hover{background:var(--sel)}
.row.on{background:var(--sel);border-color:var(--accent)}
.row .t{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.row .n{color:var(--dim);font-size:11px;white-space:nowrap}
.dead .t{text-decoration:line-through;color:var(--dim)}
input[type=search],input[type=text],select{font:inherit;width:100%;padding:6px 9px;
 border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--ink)}
.k{display:grid;grid-template-columns:88px 1fr;gap:4px 10px;font-size:12px;margin:10px 0}
.k div:nth-child(odd){color:var(--dim)}
.msg{border-left:2px solid var(--line);padding:4px 0 4px 9px;margin:7px 0;font-size:12px;
 white-space:pre-wrap;word-break:break-word}
.msg.user{border-color:var(--accent)}
.msg .h{color:var(--dim);font-size:10.5px;margin-bottom:2px}
.acts{display:flex;flex-wrap:wrap;gap:6px;margin:12px 0}
hr{border:0;border-top:1px solid var(--line);margin:14px 0}
.hd{font-size:11px;text-transform:uppercase;letter-spacing:.07em;color:var(--dim);margin:14px 0 6px}
#toast{position:fixed;bottom:16px;left:50%;transform:translateX(-50%);background:var(--ink);
 color:var(--bg);padding:9px 16px;border-radius:8px;font-size:12px;opacity:0;transition:.2s;
 pointer-events:none;max-width:70vw}
#toast.on{opacity:1}
</style></head><body>
<header>
  <h1>ferry<small id="sub"></small></h1>
  <span style="flex:1"></span>
  <span id="lock"></span>
  <button onclick="vault()">Archive to vault</button>
  <button onclick="load()">Refresh</button>
</header>
<main>
  <section id="accts"></section>
  <section id="chats"></section>
  <section id="detail"><p style="color:var(--dim)">Select a chat.</p></section>
</main>
<div id="toast"></div>
<script>
let S=null, curScope=null, curChat=null, q="";
const $=s=>document.querySelector(s);
const fmt=b=>b>1048576?(b/1048576).toFixed(1)+" MB":(b/1024).toFixed(0)+" KB";
const day=ms=>ms?new Date(ms).toISOString().slice(0,10):"?";
function toast(m,ms=2600){const t=$("#toast");t.textContent=m;t.className="on";
  clearTimeout(t._x);t._x=setTimeout(()=>t.className="",ms);}

async function load(){
  S=await (await fetch("/api/state")).json();
  $("#lock").innerHTML = S.appRunning
    ? '<span class="warn">Claude app is running - edits disabled</span>' : '';
  const n=S.scopes.reduce((a,s)=>a+s.chats.length,0);
  $("#sub").textContent=` ${S.scopes.length} accounts / ${n} chats`;
  if(!curScope||!S.scopes.find(s=>s.acct+"|"+s.org===curScope))
     curScope=S.scopes.length?S.scopes[0].acct+"|"+S.scopes[0].org:null;
  drawAccts(); drawChats();
}
function scope(){return S.scopes.find(s=>s.acct+"|"+s.org===curScope);}

function drawAccts(){
  $("#accts").innerHTML=S.scopes.map(s=>{
    const key=s.acct+"|"+s.org;
    const who=s.profile?s.profile.email:(s.label||"unidentified account");
    const con=Object.keys(s.connectors).slice(0,4).map(c=>`<span class=tag>${c}</span>`).join("");
    const dates=s.chats.length?`${day(Math.min(...s.chats.map(c=>c.created||Infinity)))} - ${day(Math.max(...s.chats.map(c=>c.last||0)))}`:"";
    return `<div class="card ${key===curScope?'on':''}" onclick="pick('${key}')">
      <b>${who}</b>
      <div class=m>${s.acct.slice(0,8)} / ${s.org.slice(0,8)}</div>
      <div class=m>${s.chats.length} chats${s.deleted.length?` &middot; ${s.deleted.length} deleted`:""} &middot; ${dates}</div>
      <div>${s.isCurrent?'<span class="tag cur">current</span>':''}${con}</div>
      <div class=m style="margin-top:6px">
        <input type=text placeholder="nickname this account" value="${s.label||''}"
         onclick="event.stopPropagation()"
         onchange="label('${s.acct}',this.value)"></div>
    </div>`;}).join("");
}
function pick(k){curScope=k;curChat=null;drawAccts();drawChats();
  $("#detail").innerHTML='<p style="color:var(--dim)">Select a chat.</p>';}

function drawChats(){
  const s=scope(); if(!s){$("#chats").innerHTML="";return;}
  const list=s.chats.filter(c=>!q||c.title.toLowerCase().includes(q));
  $("#chats").innerHTML=
   `<input type=search placeholder="filter ${s.chats.length} chats" value="${q}"
      oninput="q=this.value.toLowerCase();drawChats()">
    <div class=hd>chats</div>`+
   list.map(c=>`<div class="row ${curChat===c.path?'on':''}" onclick="open_('${c.path.replace(/'/g,"\\'")}')">
      <span class=t>${c.title}</span>
      <span class=n>${c.turns||0}t &middot; ${fmt(c.bytes)}${c.missing?' &middot; <span style="color:var(--warn)">'+c.missing+' gone</span>':''}</span>
    </div>`).join("")+
   (s.deleted.length?`<div class=hd>deleted (${s.deleted.length})</div>`+
     s.deleted.map(d=>`<div class="row dead">
        <span class=t>${d.id.slice(6,14)}</span>
        <span class=n>${day(d.when)}</span>
        <button onclick="undel('${d.id}')">Undelete</button></div>`).join(""):"");
}

async function open_(p){
  curChat=p; drawChats();
  const d=await (await fetch("/api/chat?path="+encodeURIComponent(p))).json();
  const s=scope();
  const targets=S.scopes.filter(x=>x.acct+"|"+x.org!==curScope);
  const opts=targets.map(x=>`<option value="${x.acct}|${x.org}">${x.profile?x.profile.email:(x.label||x.acct.slice(0,8))}</option>`).join("");
  $("#detail").innerHTML=`
    <input type=text id=ttl value="${d.rec.title.replace(/"/g,'&quot;')}">
    <div class=acts>
      <button onclick="rename()">Rename</button>
      <button onclick="del()">Delete</button>
    </div>
    <div class=k>
      <div>id</div><div>${d.rec.sessionId.slice(6,20)}</div>
      <div>model</div><div>${d.rec.model||"?"}</div>
      <div>folder</div><div>${d.rec.cwd.replace(/^\/Users\/[^/]+/,"~")}</div>
      <div>turns</div><div>${d.rec.completedTurns||0}</div>
      <div>active</div><div>${day(d.rec.createdAt)} - ${day(d.rec.lastActivityAt)}</div>
      <div>files</div><div>${d.files.length} transcripts &middot; ${fmt(d.bytes)}${d.subs?` &middot; ${d.subs} subagent files`:""}</div>
      ${d.rec.forkedFromSessionId?`<div>fork of</div><div>${d.rec.forkedFromSessionId.slice(6,20)}</div>`:""}
    </div>
    ${targets.length?`<hr><div class=hd>send to another account</div>
    <select id=tgt>${opts}</select>
    <div class=acts>
      <button class=p onclick="send(false)">Copy &rarr;</button>
      <button onclick="send(true)">Move &rarr;</button>
    </div>
    <div style="color:var(--dim);font-size:11px">Connector settings are stripped - they belong to the source account.</div>`:""}
    <hr><div class=hd>conversation</div>
    ${d.msgs.length?d.msgs.map(m=>`<div class="msg ${m.role}"><div class=h>${m.role} &middot; ${m.t}</div>${
       m.text.replace(/[<>&]/g,c=>({'<':'&lt;','>':'&gt;','&':'&amp;'}[c]))}</div>`).join("")
     :'<p style="color:var(--warn)">Transcript file is gone - pruned by retention. The vault prevents this.</p>'}`;
}

async function op(body,okmsg){
  const r=await (await fetch("/api/op",{method:"POST",headers:{"content-type":"application/json"},
    body:JSON.stringify(body)})).json();
  if(r.error) toast("x "+r.error,5000); else { toast(okmsg); await load();
    if(curChat&&body.op!=="delete"&&body.op!=="move") open_(curChat); else $("#detail").innerHTML=""; }
}
const rename=()=>op({op:"rename",path:curChat,title:$("#ttl").value},"Renamed");
const del=()=>confirm("Delete this chat from this account? The transcript stays on disk.")
  &&op({op:"delete",path:curChat},"Deleted");
function send(move){const [a,o]=$("#tgt").value.split("|");
  op({op:"copy",path:curChat,acct:a,org:o,move:move},move?"Moved":"Copied");}
const undel=id=>{const s=scope();op({op:"undelete",acct:s.acct,org:s.org,id:id},"Restored");};
const label=(a,v)=>op({op:"label",acct:a,name:v},"Saved");
const vault=async()=>{toast("Archiving...",9000);
  const r=await (await fetch("/api/op",{method:"POST",headers:{"content-type":"application/json"},
    body:JSON.stringify({op:"vault"})})).json();
  toast(r.error?("x "+r.error):`Vaulted ${r.records} chats, ${r.transcripts} new transcripts (${r.mb} MB)`,6000);}
load();
</script></body></html>"""

class H(BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def _send(self, obj, code=200, ctype="application/json"):
        b = obj if isinstance(obj,bytes) else json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type",ctype)
        self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        from urllib.parse import urlparse, parse_qs, unquote
        u = urlparse(self.path)
        if u.path == "/": return self._send(PAGE.encode(),200,"text/html; charset=utf-8")
        if u.path == "/api/state": return self._send(scan())
        if u.path == "/api/chat":
            p = unquote(parse_qs(u.query).get("path",[""])[0])
            if not p.startswith(SESS): return self._send({"error":"bad path"},400)
            rec = read_rec(p) or {}
            tr  = transcripts_for(rec)
            msgs = []
            for t in tr:
                if t["exists"] and not msgs: msgs = transcript_head(t["path"])
            return self._send({"rec":rec,"files":tr,
                               "bytes":sum(t["size"] for t in tr),
                               "subs":sum(len(t["subagents"]) for t in tr),"msgs":msgs})
        self._send({"error":"not found"},404)
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        try: b = json.loads(self.rfile.read(n) or b"{}")
        except Exception: return self._send({"error":"bad json"},400)
        op = b.get("op")
        try:
            for k in ("path",):
                if b.get(k) and not str(b[k]).startswith(SESS): raise RuntimeError("bad path")
            if   op=="copy":     r=op_copy(b["path"],b["acct"],b["org"],b.get("move",False))
            elif op=="rename":   r=op_rename(b["path"],b.get("title","").strip() or "(untitled)")
            elif op=="delete":   r=op_delete(b["path"])
            elif op=="undelete": r=op_undelete(b["acct"],b["org"],b["id"])
            elif op=="vault":    r=op_vault()
            elif op=="label":    set_label(b["acct"],b.get("name","").strip()); r={"ok":True}
            else: raise RuntimeError("unknown op")
            return self._send(r)
        except Exception as e:
            return self._send({"error":str(e)},400)


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
    msgs = full_msgs(rec)
    title = rec.get("title", "chat")
    short = rec["sessionId"][len("local_"):][:8]
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
    dest = os.path.expanduser(f"~/Downloads/{slug(title)}-{short}.{fmt}")
    os.makedirs(os.path.dirname(dest), exist_ok=True)
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
