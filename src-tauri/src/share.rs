//! Nearby: send a chat to another Ferry on the same network.
//!
//! Off until the person turns it on. While on, this machine announces itself
//! over mDNS as `_ferry._tcp` and listens on one TCP port. Nothing is sent
//! anywhere else, and an address outside the local network is refused, so a
//! chat never crosses the router.
//!
//! A transfer, and why it holds up on a shared Wi-Fi:
//!  1. The two sides swap fresh X25519 keys. Everything after that is sealed
//!     with ChaCha20-Poly1305, one key per direction.
//!  2. Both screens show a six-digit code derived from that exchange. A device
//!     sitting in between would have to run two exchanges, and the two people
//!     would see two different codes.
//!  3. The sender describes the chat. Nothing else moves until the person on
//!     the other side accepts it and picks the account it goes into.
//!  4. The receiver trusts nothing it is told. Ids and file names are checked
//!     before they become paths, sizes are capped, every file is staged and
//!     moved in only once all of them arrived, and a conversation already on
//!     this machine is never replaced by a different one.

use super::*;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const SERVICE: &str = "_ferry._tcp.local.";
/// Tried first, so an address typed by hand stays the same between launches.
const PORT: u16 = 53711;
/// Bumped whenever the messages change shape; two versions refuse each other.
const PROTO: u64 = 1;
const CHUNK: usize = 1 << 20;
const MAX_JSON: usize = 4 << 20;
/// The largest real chat seen was 45 MB with its subagents. This is headroom,
/// not a target: it stops a peer from filling the disk.
const MAX_TOTAL: u64 = 2 << 30;
const MAX_FILES: usize = 5000;
/// How long the person on the receiving side has to decide.
const ASK_FOR: Duration = Duration::from_secs(150);
/// Connections being handled at once. A transfer is one connection; this only
/// stops something on the network from opening hundreds.
const MAX_ACTIVE: usize = 4;

/* ---------- state ---------- */

pub struct Answer {
    pub accept: bool,
    pub acct: String,
    pub org: String,
    pub folder: Option<String>,
}

#[derive(Default)]
struct St {
    on: bool,
    id: String,
    name: String,
    port: u16,
    stop: Option<Arc<AtomicBool>>,
    mdns: Option<ServiceDaemon>,
    mdns_err: String,
    fullname: String,
    peers: HashMap<String, Value>,
    incoming: Option<Value>,
    answer: Option<mpsc::Sender<Answer>>,
    outgoing: Option<Value>,
    cancel: Option<TcpStream>,
}

fn st() -> MutexGuard<'static, St> {
    static S: OnceLock<Mutex<St>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(St::default())).lock().unwrap_or_else(|e| e.into_inner())
}

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/* ---------- names, addresses, small helpers ---------- */

fn os_name() -> &'static str {
    if cfg!(target_os = "windows") { "Windows" } else if cfg!(target_os = "macos") { "macOS" } else { "Linux" }
}

/// What the other person sees this machine called.
fn my_name() -> String {
    static N: OnceLock<String> = OnceLock::new();
    N.get_or_init(|| {
        if let Ok(n) = std::env::var("FERRY_NEARBY_NAME") {
            if !n.trim().is_empty() { return clean(&n, 60); }
        }
        #[cfg(target_os = "macos")]
        if let Ok(o) = Command::new("scutil").args(["--get", "ComputerName"]).output() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() { return clean(&s, 60); }
        }
        for k in ["COMPUTERNAME", "HOSTNAME"] {
            if let Ok(v) = std::env::var(k) {
                if !v.trim().is_empty() { return clean(&v, 60); }
            }
        }
        "Ferry".to_string()
    }).clone()
}

/// Text from the other side, made safe to show: no control characters, bounded.
fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{:02x}", x)).collect() }

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() { return None; }
    let mut o = [0u8; 32];
    for (i, b) in o.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(o)
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    OsRng.fill_bytes(&mut b);
    hex(&b)
}

/// The local network, and nothing past it: private ranges, link-local, the
/// carrier-grade range Tailscale uses, and loopback for testing on one machine.
fn lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_link_local() || v.is_loopback()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v) => {
            v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn local_ips() -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = if_addrs::get_if_addrs().unwrap_or_default().into_iter()
        .map(|i| i.ip())
        .filter(|ip| ip.is_ipv4() && !ip.is_loopback() && lan(*ip))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// One path segment: an account, an org, a session id. Never a separator, never
/// `..`, never empty.
fn safe_seg(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn safe_file(s: &str) -> bool { s.strip_suffix(".jsonl").map(safe_seg).unwrap_or(false) }

fn net_err(e: std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        TimedOut | WouldBlock => "the other side stopped answering".into(),
        UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe => "the connection closed".into(),
        _ => e.to_string(),
    }
}

/* ---------- frames and the sealed channel ---------- */

fn put(s: &mut TcpStream, b: &[u8]) -> Result<(), String> {
    s.write_all(&(b.len() as u32).to_be_bytes()).and_then(|_| s.write_all(b)).map_err(net_err)
}

fn get(s: &mut TcpStream, max: usize) -> Result<Vec<u8>, String> {
    let mut n = [0u8; 4];
    s.read_exact(&mut n).map_err(net_err)?;
    let n = u32::from_be_bytes(n) as usize;
    if n > max {
        return Err(format!("the other side sent {n} bytes where at most {max} were expected"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).map_err(net_err)?;
    Ok(b)
}

struct Chan { s: TcpStream, tx: ChaCha20Poly1305, rx: ChaCha20Poly1305, ntx: u64, nrx: u64 }

/// A counter, never reused: each direction has its own key, so the two sides
/// can both start at zero.
fn nonce(n: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_be_bytes());
    b
}

impl Chan {
    fn send(&mut self, b: &[u8]) -> Result<(), String> {
        let ct = self.tx.encrypt(Nonce::from_slice(&nonce(self.ntx)), b)
            .map_err(|_| "could not seal a message".to_string())?;
        self.ntx += 1;
        put(&mut self.s, &ct)
    }
    fn recv(&mut self, max: usize) -> Result<Vec<u8>, String> {
        let ct = get(&mut self.s, max + 16)?;
        let pt = self.rx.decrypt(Nonce::from_slice(&nonce(self.nrx)), ct.as_ref())
            .map_err(|_| "a message failed its check, so the connection was cut".to_string())?;
        self.nrx += 1;
        Ok(pt)
    }
    fn send_json(&mut self, v: &Value) -> Result<(), String> { self.send(v.to_string().as_bytes()) }
    fn recv_json(&mut self) -> Result<Value, String> {
        serde_json::from_slice(&self.recv(MAX_JSON)?).map_err(|_| "the other side sent something garbled".to_string())
    }
}

/// Swap keys, derive the two directions' keys and the code both people compare.
fn handshake(mut s: TcpStream, initiator: bool) -> Result<(Chan, Value, String), String> {
    let sk = x25519_dalek::EphemeralSecret::random_from_rng(OsRng);
    let pk = x25519_dalek::PublicKey::from(&sk);
    let me = json!({ "app": "ferry", "v": PROTO, "pk": hex(pk.as_bytes()),
                     "name": my_name(), "os": os_name() });
    let hello = |b: Vec<u8>| serde_json::from_slice::<Value>(&b)
        .map_err(|_| "that isn't Ferry answering".to_string());
    let peer = if initiator {
        put(&mut s, me.to_string().as_bytes())?;
        hello(get(&mut s, 4096)?)?
    } else {
        let p = hello(get(&mut s, 4096)?)?;
        put(&mut s, me.to_string().as_bytes())?;
        p
    };
    if peer["app"].as_str() != Some("ferry") { return Err("that isn't Ferry answering".into()); }
    if peer["v"].as_u64() != Some(PROTO) {
        return Err("the other Ferry is a different version. Update both to the same one.".into());
    }
    let theirs = unhex32(peer["pk"].as_str().unwrap_or("")).ok_or("the other side sent a bad key")?;
    let shared = sk.diffie_hellman(&x25519_dalek::PublicKey::from(theirs));
    if !shared.was_contributory() { return Err("the key exchange was refused".into()); }

    let (pi, pr) = if initiator { (*pk.as_bytes(), theirs) } else { (theirs, *pk.as_bytes()) };
    let base: [u8; 32] = Sha256::new().chain_update(b"ferry nearby v1").chain_update(shared.as_bytes())
        .chain_update(pi).chain_update(pr).finalize().into();
    let k = |label: &[u8]| -> [u8; 32] { Sha256::new().chain_update(base).chain_update(label).finalize().into() };
    let (ki, kr, c) = (k(b"initiator"), k(b"responder"), k(b"code"));
    let n = u32::from_be_bytes([c[0], c[1], c[2], c[3]]) % 1_000_000;
    let code = format!("{:03} {:03}", n / 1000, n % 1000);
    let (txk, rxk) = if initiator { (ki, kr) } else { (kr, ki) };
    Ok((Chan { s, tx: ChaCha20Poly1305::new(Key::from_slice(&txk)),
               rx: ChaCha20Poly1305::new(Key::from_slice(&rxk)), ntx: 0, nrx: 0 }, peer, code))
}

/* ---------- what is sent ---------- */

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind { Main, Sub }

#[derive(Clone, Debug)]
struct FileSpec { kind: Kind, id: String, name: String, size: u64 }

impl FileSpec {
    fn stage_name(&self) -> String {
        match self.kind {
            Kind::Main => format!("{}.jsonl", self.id),
            Kind::Sub => format!("{}__{}", self.id, self.name),
        }
    }
    fn json(&self) -> Value {
        json!({ "kind": if self.kind == Kind::Main { "main" } else { "sub" },
                "id": self.id, "name": self.name, "size": self.size })
    }
}

/// Every transcript the chat is made of, with its subagents, as they are on disk
/// now. The chat's own transcript comes first.
fn outgoing_files(tr: &[Value]) -> Vec<(FileSpec, String)> {
    let mut out = vec![];
    for t in tr {
        let (Some(p), Some(id)) = (t["path"].as_str(), t["id"].as_str()) else { continue };
        if !safe_seg(id) { continue; }
        let Ok(md) = fs::metadata(p) else { continue };
        out.push((FileSpec { kind: Kind::Main, id: id.into(), name: String::new(), size: md.len() }, p.into()));
        let dir = Path::new(p).parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
        if let Ok(g) = glob::glob(&format!("{}/{}/subagents/*.jsonl", dir, id)) {
            for s in g.flatten() {
                let name = s.file_name().unwrap_or_default().to_string_lossy().to_string();
                if !safe_file(&name) { continue; }
                if let Ok(m) = fs::metadata(&s) {
                    out.push((FileSpec { kind: Kind::Sub, id: id.into(), name, size: m.len() },
                              s.to_string_lossy().to_string()));
                }
            }
        }
    }
    out
}

/// The receiver's reading of an offer. Everything in it came off the network,
/// so it is checked here, once, before any of it is used.
fn check_offer(o: &Value) -> Result<Vec<FileSpec>, String> {
    let arr = o["files"].as_array().ok_or("the offer lists no files")?;
    if arr.is_empty() { return Err("the offer lists no files".into()); }
    if arr.len() > MAX_FILES { return Err("the offer lists too many files".into()); }
    let (mut total, mut mains, mut seen) = (0u64, 0usize, HashSet::new());
    let mut out = vec![];
    for f in arr {
        let id = f["id"].as_str().unwrap_or("");
        if !safe_seg(id) { return Err("the offer has a transcript id that isn't safe to use".into()); }
        let size = f["size"].as_u64().ok_or("the offer is missing a size")?;
        total = total.checked_add(size).ok_or("the offer's sizes overflow")?;
        if total > MAX_TOTAL { return Err("that chat is larger than Ferry will take over the network".into()); }
        let spec = match f["kind"].as_str() {
            Some("main") => { mains += 1; FileSpec { kind: Kind::Main, id: id.into(), name: String::new(), size } }
            Some("sub") => {
                let name = f["name"].as_str().unwrap_or("");
                if !safe_file(name) { return Err("the offer has a file name that isn't safe to use".into()); }
                FileSpec { kind: Kind::Sub, id: id.into(), name: name.into(), size }
            }
            _ => return Err("the offer has a file of an unknown kind".into()),
        };
        if !seen.insert(spec.stage_name()) { return Err("the offer lists the same file twice".into()); }
        out.push(spec);
    }
    if mains == 0 { return Err("the offer has no conversation in it".into()); }
    if out[0].kind != Kind::Main { return Err("the offer doesn't start with the conversation".into()); }
    if o["bytes"].as_u64() != Some(total) { return Err("the offer's sizes don't add up".into()); }
    Ok(out)
}

/* ---------- where things land ---------- */

/// The folders a receive writes into. The app uses the real ones; the tests
/// point these at a temporary tree.
#[derive(Clone)]
struct Roots { sess: String, proj: String, vault: String, real: bool }

impl Roots {
    fn real() -> Self { Roots { sess: sess(), proj: proj(), vault: vault(), real: true } }
    fn pdir(&self, cwd: &str) -> String {
        if self.real { project_dir(cwd) } else { format!("{}/{}", self.proj, enc_cwd(cwd)) }
    }
    fn snap(&self, p: &str, tag: &str) {
        if self.real { return snapshot(p, tag); }
        if !Path::new(p).exists() { return; }
        let name = Path::new(p).file_name().unwrap_or_default().to_string_lossy().to_string();
        let dir = format!("{}/snapshots", self.vault);
        let _ = fs::create_dir_all(&dir);
        let _ = fs::copy(p, format!("{}/{}-{}-{}", dir, stamp(), tag, name));
    }
}

#[derive(Debug, PartialEq)]
enum Cmp { Same, Longer, Shorter, Differs }

/// Two copies of one transcript. A transcript only ever grows, so when one is
/// the start of the other they are the same conversation, one further along.
fn compare(ours: &str, theirs: &str) -> Result<Cmp, String> {
    let (a, b) = (fs::metadata(ours).map_err(|e| e.to_string())?.len(),
                  fs::metadata(theirs).map_err(|e| e.to_string())?.len());
    let (mut fa, mut fb) = (fs::File::open(ours).map_err(|e| e.to_string())?,
                            fs::File::open(theirs).map_err(|e| e.to_string())?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    let mut left = a.min(b);
    while left > 0 {
        let n = (left as usize).min(ba.len());
        fa.read_exact(&mut ba[..n]).map_err(|e| e.to_string())?;
        fb.read_exact(&mut bb[..n]).map_err(|e| e.to_string())?;
        if ba[..n] != bb[..n] { return Ok(Cmp::Differs); }
        left -= n as u64;
    }
    Ok(if a == b { Cmp::Same } else if b > a { Cmp::Longer } else { Cmp::Shorter })
}

/// Put a received chat into an account. Nothing is written until every file
/// has been judged, so a refusal leaves the machine exactly as it was.
fn commit(r: &Roots, stage: &str, offer: &Value, files: &[FileSpec], rec: &Value, a: &Answer)
          -> Result<Value, String> {
    if !safe_seg(&a.acct) || !safe_seg(&a.org) { return Err("pick an account on this machine".into()); }
    let scope = format!("{}/{}/{}", r.sess, a.acct, a.org);
    if !Path::new(&scope).is_dir() { return Err("that account has no folder on this machine".into()); }

    let theirs = clean(offer["cwd"].as_str().unwrap_or(""), 1024);
    let cwd = match a.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => {
            if !Path::new(f).is_dir() { return Err(format!("no such folder: {f}")); }
            f.to_string()
        }
        None => theirs,
    };
    if cwd.is_empty() {
        return Err("this chat has no folder. Choose one on this machine to put it in.".into());
    }
    let dir = r.pdir(&cwd);
    if !under(&r.proj, &dir) || Path::new(&dir) == Path::new(&r.proj) {
        return Err("that folder doesn't map inside ~/.claude/projects".into());
    }

    let (mut plan, mut kept) = (vec![], 0usize);
    for f in files {
        let src = format!("{}/{}", stage, f.stage_name());
        let dst = match f.kind {
            Kind::Main => format!("{}/{}.jsonl", dir, f.id),
            Kind::Sub => format!("{}/{}/subagents/{}", dir, f.id, f.name),
        };
        if !under(&dir, &dst) { return Err("a file would have landed outside the chat's folder".into()); }
        if !Path::new(&dst).exists() { plan.push((src, dst, false)); continue; }
        match compare(&dst, &src)? {
            Cmp::Same | Cmp::Shorter => kept += 1,        // this machine already has all of it
            Cmp::Longer => plan.push((src, dst, true)),   // the conversation went on since
            Cmp::Differs => return Err(format!(
                "this machine already has a different conversation under the same id ({}). \
                 Nothing was changed.", f.id)),
        }
    }
    for (src, dst, replace) in &plan {
        if let Some(p) = Path::new(dst).parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
        if *replace { r.snap(dst, "nearby-replaced"); }
        fs::copy(src, dst).map_err(|e| e.to_string())?;
    }

    let main_id = &files[0].id;
    let tpl = template_record(&scope);
    let claimed = rec.is_object()
        && rec["sessionId"].as_str().map(|s| s.starts_with("local_") && safe_seg(s)).unwrap_or(false);
    let mut out = if claimed {
        let mut o = rec.clone();
        if let Some(m) = o.as_object_mut() { for k in ACCOUNT_SCOPED { m.remove(k); } }
        o
    } else {
        // A session the sender's CLI or VS Code ran: no account ever claimed it,
        // so it gets the record it never had, the way an import does.
        let info = read_session(&format!("{}/{}.jsonl", dir, main_id))
            .ok_or("the conversation arrived but can't be read")?;
        let mut o = json!({
            "sessionId": format!("local_{main_id}"), "cliSessionId": main_id,
            "title": info["title"], "titleSource": "auto",
            "createdAt": info["created"], "lastActivityAt": info["last"],
            "lastFocusedAt": info["last"], "completedTurns": info["turns"], "isArchived": false,
        });
        if let Some(m) = info["model"].as_str() { if !m.is_empty() { o["model"] = json!(m); } }
        o
    };
    // Fields that only resolve on the machine that wrote them come from a record
    // this account already has, or are left out.
    for k in INHERIT {
        match tpl.as_ref().map(|t| t[k].clone()).filter(|v| !v.is_null()) {
            Some(v) => out[k] = v,
            None => { if let Some(m) = out.as_object_mut() { m.remove(k); } }
        }
    }
    out["cwd"] = json!(cwd);
    out["originCwd"] = json!(cwd);

    let sid = out["sessionId"].as_str().unwrap_or("").to_string();
    if !safe_seg(&sid) { return Err("the chat's id isn't safe to use".into()); }
    let dst = format!("{}/{}.json", scope, sid);
    r.snap(&dst, "nearby");
    fs::write(&dst, serde_json::to_string_pretty(&out).unwrap()).map_err(|e| e.to_string())?;
    let tomb = format!("{}/deleted_{}", scope, sid.trim_start_matches("local_"));
    if Path::new(&tomb).exists() { r.snap(&tomb, "undelete"); let _ = fs::remove_file(&tomb); }

    Ok(json!({ "ok": true, "wrote": dst, "title": out["title"], "cwd": cwd,
               "folderHere": Path::new(&cwd).is_dir(), "written": plan.len(), "kept": kept,
               "acct": a.acct, "org": a.org }))
}

/* ---------- receiving ---------- */

type Decide = dyn Fn(&Value, &Value, &str) -> Option<Answer> + Send + Sync;

struct Ctx { roots: Roots, decide: Box<Decide>, guard: fn() -> Result<(), String> }

fn set_incoming(k: &str, v: Value) {
    if let Some(i) = st().incoming.as_mut() { i[k] = v; }
}

fn receive_file(ch: &mut Chan, stage: &str, f: &FileSpec, got: &mut u64) -> Result<(), String> {
    let path = format!("{}/{}", stage, f.stage_name());
    let mut w = std::io::BufWriter::new(fs::File::create(&path).map_err(|e| e.to_string())?);
    let mut left = f.size;
    while left > 0 {
        let b = ch.recv(CHUNK)?;
        if b.is_empty() || b.len() as u64 > left {
            return Err("the other side sent more than it said it would".into());
        }
        w.write_all(&b).map_err(|e| e.to_string())?;
        left -= b.len() as u64;
        *got += b.len() as u64;
        set_incoming("got", json!(*got));
    }
    w.flush().map_err(|e| e.to_string())
}

/// One incoming connection, start to finish.
fn serve(s: TcpStream, ctx: &Ctx) -> Result<Value, String> {
    let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let (mut ch, peer, code) = handshake(s, false)?;
    let offer = ch.recv_json()?;
    let files = match check_offer(&offer) {
        Ok(f) => f,
        Err(e) => { let _ = ch.send_json(&json!({ "t": "decline", "msg": e })); return Err(e); }
    };
    let Some(ans) = (ctx.decide)(&peer, &offer, &code) else {
        let _ = ch.send_json(&json!({ "t": "decline" }));
        return Ok(json!({ "declined": true }));
    };
    ch.send_json(&json!({ "t": "accept" }))?;

    let stage = format!("{}/incoming/{}", ctx.roots.vault, rand_hex(6));
    let res = (|| {
        fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let mut got = 0u64;
        for f in &files { receive_file(&mut ch, &stage, f, &mut got)?; }
        let rec = ch.recv_json()?;
        (ctx.guard)()?;          // Claude may have been opened while the files came in
        commit(&ctx.roots, &stage, &offer, &files, &rec, &ans)
    })();
    let _ = fs::remove_dir_all(&stage);
    let _ = ch.send_json(&match &res {
        Ok(v) => json!({ "t": "done", "ok": true, "title": v["title"] }),
        Err(e) => json!({ "t": "done", "ok": false, "msg": e }),
    });
    match &res {
        Ok(v) => {
            set_incoming("result", v.clone());
            set_incoming("state", json!("done"));
        }
        Err(e) => {
            set_incoming("msg", json!(e));
            set_incoming("state", json!("error"));
        }
    }
    res
}

/// The app's way of deciding: put the offer in front of the person and wait.
fn ask_person(peer: &Value, offer: &Value, code: &str) -> Option<Answer> {
    let (tx, rx) = mpsc::channel();
    {
        let mut g = st();
        let busy = g.incoming.as_ref()
            .map(|i| matches!(i["state"].as_str(), Some("asking" | "receiving"))).unwrap_or(false);
        if !g.on || busy { return None; }
        let files = offer["files"].as_array().map(|a| a.len()).unwrap_or(0);
        g.incoming = Some(json!({
            "xfer": rand_hex(4),
            "from": clean(peer["name"].as_str().unwrap_or("Someone"), 60),
            "os": clean(peer["os"].as_str().unwrap_or(""), 12),
            "title": clean(offer["title"].as_str().unwrap_or("(untitled)"), 200),
            "turns": offer["turns"].as_u64().unwrap_or(0),
            "bytes": offer["bytes"].as_u64().unwrap_or(0),
            "files": files,
            "cwd": clean(offer["cwd"].as_str().unwrap_or(""), 1024),
            "cwdHere": offer["cwd"].as_str().map(|c| !c.is_empty() && Path::new(c).is_dir()).unwrap_or(false),
            "code": code, "state": "asking", "got": 0,
        }));
        g.answer = Some(tx);
    }
    let a = rx.recv_timeout(ASK_FOR).ok();
    let mut g = st();
    g.answer = None;
    let yes = a.as_ref().map(|x| x.accept).unwrap_or(false);
    if let Some(i) = g.incoming.as_mut() {
        i["state"] = json!(if yes { "receiving" } else if a.is_some() { "declined" } else { "expired" });
    }
    a.filter(|x| x.accept)
}

fn accept_loop(l: TcpListener, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match l.accept() {
            Ok((s, from)) => {
                if !lan(from.ip()) || ACTIVE.load(Ordering::SeqCst) >= MAX_ACTIVE { continue; }
                let _ = s.set_nonblocking(false);
                ACTIVE.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let ctx = Ctx { roots: Roots::real(), decide: Box::new(ask_person), guard };
                    let _ = serve(s, &ctx);
                    ACTIVE.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(150)),
            Err(_) => std::thread::sleep(Duration::from_millis(400)),
        }
    }
}

/* ---------- finding each other ---------- */

fn announce_and_browse(id: &str, name: &str, port: u16, stop: Arc<AtomicBool>)
                       -> Result<(ServiceDaemon, String), String> {
    let d = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let label = format!("{} {}", clean(name, 40), &id[..4]);
    let host = format!("ferry-{id}.local.");
    let shown = clean(name, 60);
    let props = [("id", id), ("name", shown.as_str()), ("os", os_name()), ("v", "1")];
    let info = ServiceInfo::new(SERVICE, &label, &host, "", port, &props[..])
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
    let full = info.get_fullname().to_string();
    d.register(info).map_err(|e| e.to_string())?;
    let rx = d.browse(SERVICE).map_err(|e| e.to_string())?;
    let me = id.to_string();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(ServiceEvent::ServiceResolved(s)) => {
                    let pid = s.get_property_val_str("id").unwrap_or("").to_string();
                    if pid.is_empty() || pid == me { continue; }
                    let mut addrs: Vec<String> = s.get_addresses_v4().into_iter()
                        .filter(|ip| lan(IpAddr::V4(*ip)))
                        .map(|ip| format!("{}:{}", ip, s.get_port())).collect();
                    addrs.sort();
                    if addrs.is_empty() { continue; }
                    let key = s.get_fullname().to_string();
                    st().peers.insert(key.clone(), json!({
                        "key": key, "id": pid, "addrs": addrs,
                        "name": clean(s.get_property_val_str("name").unwrap_or("Ferry"), 60),
                        "os": clean(s.get_property_val_str("os").unwrap_or(""), 12),
                    }));
                }
                Ok(ServiceEvent::ServiceRemoved(_, full)) => { st().peers.remove(&full); }
                Ok(_) => {}
                Err(mdns_sd::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
    });
    Ok((d, full))
}

/* ---------- the commands ---------- */

pub fn start() -> Result<Value, String> {
    if st().on { return Ok(state()); }
    let listener = TcpListener::bind(("0.0.0.0", PORT))
        .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))
        .map_err(|e| format!("Nearby couldn't open a port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let id = rand_hex(6);
    let name = my_name();
    { let stop = stop.clone(); std::thread::spawn(move || accept_loop(listener, stop)); }

    // A network that drops multicast still works by typing the address, so a
    // failure here leaves the port open and says so instead of giving up.
    let (mdns, fullname, err) = match announce_and_browse(&id, &name, port, stop.clone()) {
        Ok((d, f)) => (Some(d), f, String::new()),
        Err(e) => (None, String::new(), e),
    };
    {
        let mut g = st();
        g.on = true; g.id = id; g.name = name; g.port = port;
        g.stop = Some(stop); g.mdns = mdns; g.fullname = fullname; g.mdns_err = err;
        g.peers.clear();
    }
    Ok(state())
}

pub fn stop() -> Value {
    let (d, full, stop, tx) = {
        let mut g = st();
        g.on = false;
        g.peers.clear();
        (g.mdns.take(), std::mem::take(&mut g.fullname), g.stop.take(), g.answer.take())
    };
    if let Some(s) = stop { s.store(true, Ordering::Relaxed); }
    if let Some(tx) = tx {
        let _ = tx.send(Answer { accept: false, acct: String::new(), org: String::new(), folder: None });
    }
    if let Some(d) = d {
        let _ = d.unregister(&full);
        let _ = d.shutdown();
    }
    state()
}

pub fn state() -> Value {
    let ips = local_ips();
    let g = st();
    let mut peers: Vec<Value> = g.peers.values().cloned().collect();
    peers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({
        "on": g.on, "name": if g.name.is_empty() { my_name() } else { g.name.clone() },
        "port": g.port, "os": os_name(),
        "addrs": if g.on { ips.iter().map(|ip| format!("{ip}:{}", g.port)).collect::<Vec<_>>() } else { vec![] },
        "discovery": g.mdns.is_some(), "discoveryError": g.mdns_err,
        "peers": peers, "incoming": g.incoming, "outgoing": g.outgoing,
    })
}

fn parse_addr(s: &str) -> Result<Vec<SocketAddr>, String> {
    let s = s.trim();
    let full = if s.contains(':') { s.to_string() } else { format!("{s}:{PORT}") };
    let a: SocketAddr = full.parse()
        .map_err(|_| format!("\"{s}\" isn't an address. It looks like 192.168.1.20:{PORT}."))?;
    if !lan(a.ip()) { return Err("that address isn't on your local network, so Ferry won't send to it".into()); }
    Ok(vec![a])
}

fn run_send(addrs: &[SocketAddr], rec: &Value, files: &[(FileSpec, String)],
            set: &mut dyn FnMut(&str, Value)) -> Result<&'static str, String> {
    let mut last = String::from("no address to try");
    let mut sock = None;
    for a in addrs {
        if !lan(a.ip()) { last = "that address isn't on your local network".into(); continue; }
        match TcpStream::connect_timeout(a, Duration::from_secs(4)) {
            Ok(x) => { sock = Some(x); break; }
            Err(e) => last = e.to_string(),
        }
    }
    let s = sock.ok_or_else(|| format!(
        "couldn't reach the other Ferry ({last}). Check that Nearby is on there, \
         and that a firewall isn't blocking Ferry."))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    if let Ok(c) = s.try_clone() { st().cancel = Some(c); }

    let (mut ch, peer, code) = handshake(s, true)?;
    set("code", json!(code));
    set("to", json!(clean(peer["name"].as_str().unwrap_or("the other Ferry"), 60)));
    set("os", json!(clean(peer["os"].as_str().unwrap_or(""), 12)));
    set("state", json!("waiting"));

    let total: u64 = files.iter().map(|f| f.0.size).sum();
    let offer = json!({
        "title": clean(rec["title"].as_str().unwrap_or("(untitled)"), 200),
        "turns": rec["completedTurns"], "bytes": total,
        "cwd": rec["cwd"].as_str().unwrap_or(""),
        "files": files.iter().map(|(f, _)| f.json()).collect::<Vec<_>>(),
    });
    ch.send_json(&offer)?;
    let _ = ch.s.set_read_timeout(Some(ASK_FOR + Duration::from_secs(30)));
    let r = ch.recv_json()?;
    match r["t"].as_str() {
        Some("accept") => {}
        _ => {
            if let Some(m) = r["msg"].as_str() { set("msg", json!(clean(m, 300))); }
            return Ok("declined");
        }
    }
    let _ = ch.s.set_read_timeout(Some(Duration::from_secs(90)));
    set("state", json!("sending"));

    let (mut sent, mut buf) = (0u64, vec![0u8; CHUNK]);
    for (f, p) in files {
        let mut fh = fs::File::open(p).map_err(|e| e.to_string())?;
        let mut left = f.size;
        // A transcript Claude is still writing can grow after it was measured.
        // The size in the offer is what the other side expects, so reading
        // stops there.
        while left > 0 {
            let want = (left as usize).min(CHUNK);
            let n = fh.read(&mut buf[..want]).map_err(|e| e.to_string())?;
            if n == 0 { return Err(format!("{} got shorter while it was being sent", f.stage_name())); }
            ch.send(&buf[..n])?;
            left -= n as u64;
            sent += n as u64;
            set("sent", json!(sent));
        }
    }
    let mut out = rec.clone();
    if let Some(m) = out.as_object_mut() {
        for k in ACCOUNT_SCOPED { m.remove(k); }
        for k in INHERIT { m.remove(k); }
    }
    ch.send_json(&out)?;
    let done = ch.recv_json()?;
    if done["ok"].as_bool() == Some(true) { Ok("done") }
    else { Err(clean(done["msg"].as_str().unwrap_or("the other side couldn't take it"), 400)) }
}

pub fn send(path: String, to: String) -> Result<Value, String> {
    if path.starts_with("cursor:") {
        return Err("add this Cursor chat to a Claude account first, then send it".into());
    }
    let (rec, tr) = chat_parts(&path)?;
    let files = outgoing_files(&tr);
    if files.is_empty() {
        return Err("this chat's transcript is gone from disk, so there is nothing to send".into());
    }
    let total: u64 = files.iter().map(|f| f.0.size).sum();
    let (addrs, shown) = {
        let g = st();
        match g.peers.get(&to) {
            Some(p) => (p["addrs"].as_array().map(|a| a.iter()
                           .filter_map(|x| x.as_str()?.parse::<SocketAddr>().ok()).collect::<Vec<_>>())
                           .unwrap_or_default(),
                        p["name"].as_str().unwrap_or("").to_string()),
            None => (vec![], String::new()),
        }
    };
    let addrs = if addrs.is_empty() { parse_addr(&to)? } else { addrs };
    {
        let mut g = st();
        let live = g.outgoing.as_ref()
            .map(|o| matches!(o["state"].as_str(), Some("connecting" | "waiting" | "sending"))).unwrap_or(false);
        if live { return Err("already sending a chat".into()); }
        g.outgoing = Some(json!({
            "state": "connecting", "to": if shown.is_empty() { to.clone() } else { shown },
            "title": rec["title"], "sent": 0, "total": total, "code": "",
        }));
    }
    std::thread::spawn(move || {
        let mut set = |k: &str, v: Value| { if let Some(o) = st().outgoing.as_mut() { o[k] = v; } };
        let r = run_send(&addrs, &rec, &files, &mut set);
        let mut g = st();
        g.cancel = None;
        if let Some(o) = g.outgoing.as_mut() {
            if o["state"] == "cancelled" { return; }
            match r {
                Ok(s) => o["state"] = json!(s),
                Err(e) => { o["state"] = json!("error"); o["msg"] = json!(e); }
            }
        }
    });
    Ok(json!({ "ok": true }))
}

pub fn answer(accept: bool, acct: String, org: String, folder: Option<String>) -> Result<Value, String> {
    if accept {
        guard()?;
        if !safe_seg(&acct) || !safe_seg(&org) || !Path::new(&format!("{}/{}/{}", sess(), acct, org)).is_dir() {
            return Err("pick an account on this machine".into());
        }
        if let Some(f) = folder.as_deref().filter(|f| !f.trim().is_empty()) {
            if !Path::new(f).is_dir() { return Err(format!("no such folder: {f}")); }
        }
    }
    let tx = st().answer.take().ok_or("that offer has already gone")?;
    tx.send(Answer { accept, acct, org, folder }).map_err(|_| "that offer has already gone".to_string())?;
    Ok(json!({ "ok": true }))
}

pub fn cancel() -> Value {
    let mut g = st();
    if let Some(s) = g.cancel.take() { let _ = s.shutdown(std::net::Shutdown::Both); }
    if let Some(o) = g.outgoing.as_mut() {
        if matches!(o["state"].as_str(), Some("connecting" | "waiting" | "sending")) {
            o["state"] = json!("cancelled");
        }
    }
    drop(g);
    state()
}

/// Clear a finished transfer off the screen. One still asking or moving stays.
pub fn dismiss() -> Value {
    let mut g = st();
    if g.incoming.as_ref().map(|i| !matches!(i["state"].as_str(), Some("asking" | "receiving"))).unwrap_or(false) {
        g.incoming = None;
    }
    if g.outgoing.as_ref().map(|o| !matches!(o["state"].as_str(), Some("connecting" | "waiting" | "sending"))).unwrap_or(false) {
        g.outgoing = None;
    }
    drop(g);
    state()
}

/* ---------- tests ---------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("ferry-nearby-{}-{}", tag, rand_hex(4)));
        fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().to_string()
    }

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        let c = TcpStream::connect(a).unwrap();
        (c, l.accept().unwrap().0)
    }

    #[test]
    fn both_sides_derive_the_same_code_and_talk() {
        let (c, s) = pair();
        let t = std::thread::spawn(move || {
            let (mut ch, _, code) = handshake(s, false).unwrap();
            let m = ch.recv_json().unwrap();
            ch.send_json(&json!({ "echo": m["hi"] })).unwrap();
            code
        });
        let (mut ch, peer, code) = handshake(c, true).unwrap();
        assert_eq!(peer["app"], "ferry");
        ch.send_json(&json!({ "hi": "there" })).unwrap();
        assert_eq!(ch.recv_json().unwrap()["echo"], "there");
        let theirs = t.join().unwrap();
        assert_eq!(code, theirs, "both screens must show one code");
        assert_eq!(code.len(), 7, "six digits and a space: {code}");
    }

    #[test]
    fn a_changed_byte_on_the_wire_is_caught() {
        let (c, s) = pair();
        let t = std::thread::spawn(move || {
            let (mut ch, _, _) = handshake(s, false).unwrap();
            ch.recv(1024)
        });
        let (mut ch, _, _) = handshake(c, true).unwrap();
        let mut ct = ch.tx.encrypt(Nonce::from_slice(&nonce(0)), &b"hello"[..]).unwrap();
        ct[0] ^= 1;
        put(&mut ch.s, &ct).unwrap();
        assert!(t.join().unwrap().is_err(), "a tampered frame must not decrypt");
    }

    #[test]
    fn an_offer_cannot_name_a_path_outside_the_chat() {
        let ok = json!({ "bytes": 3, "files": [{ "kind": "main", "id": "abc-123", "size": 3 }] });
        assert!(check_offer(&ok).is_ok());
        for bad in [
            json!({ "bytes": 3, "files": [{ "kind": "main", "id": "../../etc", "size": 3 }] }),
            json!({ "bytes": 3, "files": [{ "kind": "main", "id": "a/b", "size": 3 }] }),
            json!({ "bytes": 3, "files": [{ "kind": "main", "id": "", "size": 3 }] }),
            json!({ "bytes": 6, "files": [{ "kind": "main", "id": "a", "size": 3 },
                                          { "kind": "sub", "id": "a", "name": "../x.jsonl", "size": 3 }] }),
            json!({ "bytes": 6, "files": [{ "kind": "main", "id": "a", "size": 3 },
                                          { "kind": "sub", "id": "a", "name": "x.sh", "size": 3 }] }),
            json!({ "bytes": 4, "files": [{ "kind": "main", "id": "a", "size": 3 }] }),
            json!({ "bytes": 6, "files": [{ "kind": "main", "id": "a", "size": 3 },
                                          { "kind": "main", "id": "a", "size": 3 }] }),
            json!({ "bytes": 3, "files": [{ "kind": "sub", "id": "a", "name": "x.jsonl", "size": 3 }] }),
            json!({ "bytes": MAX_TOTAL + 1, "files": [{ "kind": "main", "id": "a", "size": MAX_TOTAL + 1 }] }),
        ] {
            assert!(check_offer(&bad).is_err(), "should refuse {bad}");
        }
    }

    #[test]
    fn nothing_past_the_router() {
        for ok in ["192.168.1.20:1", "10.0.0.2:1", "172.16.4.4:1", "169.254.1.1:1", "100.101.1.1:1", "127.0.0.1:1"] {
            assert!(parse_addr(ok).is_ok(), "{ok}");
        }
        for bad in ["8.8.8.8:53", "1.1.1.1", "142.250.1.1:443", "100.128.0.1:1", "nonsense"] {
            assert!(parse_addr(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_addr("192.168.1.9").unwrap()[0].port(), PORT);
    }

    /// Sends one chat from a sender's tree into a receiver's tree over a real
    /// socket, the way two machines would. Returns the receiver's answer.
    fn transfer(recv_root: &str, rec: &Value, files: Vec<(FileSpec, String)>, folder: Option<String>)
                -> (Result<&'static str, String>, Result<Value, String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let root = recv_root.to_string();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let ctx = Ctx {
                roots: Roots { sess: format!("{root}/sess"), proj: format!("{root}/proj"),
                               vault: format!("{root}/vault"), real: false },
                decide: Box::new(move |_, _, _| Some(Answer {
                    accept: true, acct: "acct-b".into(), org: "org-b".into(), folder: folder.clone() })),
                guard: || Ok(()),
            };
            serve(s, &ctx)
        });
        let sent = run_send(&[addr], rec, &files, &mut |_, _| {});
        (sent, t.join().unwrap())
    }

    #[test]
    fn a_chat_lands_whole_and_a_different_one_is_refused() {
        let send_root = tmp("send");
        let recv_root = tmp("recv");
        let scope = format!("{recv_root}/sess/acct-b/org-b");
        fs::create_dir_all(&scope).unwrap();
        // a record this account already has, to take its environment fields from
        fs::write(format!("{scope}/local_other.json"),
                  json!({ "sessionId": "local_other", "envScopeId": "env-B" }).to_string()).unwrap();

        let cwd = "/Users/sender/code/app";
        let sid = "11111111-2222-4333-8444-555555555555";
        let tdir = format!("{send_root}/{}", enc_cwd(cwd));
        fs::create_dir_all(format!("{tdir}/{sid}/subagents")).unwrap();
        let line = |t: &str| json!({ "type": "user", "cwd": cwd, "timestamp": "2026-09-20T10:00:00.000Z",
                                     "message": { "role": "user", "content": t } }).to_string() + "\n";
        let main = format!("{tdir}/{sid}.jsonl");
        let sub = format!("{tdir}/{sid}/subagents/agent-a1.jsonl");
        fs::write(&main, line("how does ferry move a chat")).unwrap();
        fs::write(&sub, line("subagent work")).unwrap();

        let rec = json!({ "sessionId": format!("local_{sid}"), "cliSessionId": sid, "cwd": cwd,
                          "title": "How Ferry moves a chat", "completedTurns": 1,
                          "envScopeId": "env-A", "remoteMcpServersConfig": [{ "name": "linear" }] });
        let files = |m: &str, s: &str| vec![
            (FileSpec { kind: Kind::Main, id: sid.into(), name: String::new(),
                        size: fs::metadata(m).unwrap().len() }, m.to_string()),
            (FileSpec { kind: Kind::Sub, id: sid.into(), name: "agent-a1.jsonl".into(),
                        size: fs::metadata(s).unwrap().len() }, s.to_string()),
        ];

        // 1. it arrives, whole, in the chosen account
        let (sent, got) = transfer(&recv_root, &rec, files(&main, &sub), None);
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        let dest = format!("{recv_root}/proj/{}", enc_cwd(cwd));
        assert_eq!(fs::read(format!("{dest}/{sid}.jsonl")).unwrap(), fs::read(&main).unwrap());
        assert!(Path::new(&format!("{dest}/{sid}/subagents/agent-a1.jsonl")).exists());
        let r: Value = serde_json::from_str(&fs::read_to_string(got["wrote"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(r["title"], "How Ferry moves a chat");
        assert_eq!(r["envScopeId"], "env-B", "environment fields come from the receiving account");
        assert!(r.get("remoteMcpServersConfig").is_none(), "connectors stay with the sender");
        assert!(fs::read_dir(format!("{recv_root}/vault/incoming")).unwrap().next().is_none(),
                "the staging folder is cleaned up");

        // 2. the same chat again changes nothing
        let (sent, got) = transfer(&recv_root, &rec, files(&main, &sub), None);
        assert_eq!(sent.unwrap(), "done");
        assert_eq!(got.unwrap()["written"], 0);

        // 3. the conversation went on: the longer copy replaces the shorter
        let mut longer = fs::read_to_string(&main).unwrap();
        longer.push_str(&line("and one more question"));
        fs::write(&main, &longer).unwrap();
        let (sent, got) = transfer(&recv_root, &rec, files(&main, &sub), None);
        assert_eq!(sent.unwrap(), "done");
        assert_eq!(got.unwrap()["written"], 1);
        assert_eq!(fs::read_to_string(format!("{dest}/{sid}.jsonl")).unwrap(), longer);

        // 4. a different conversation under the same id is refused, and nothing moves
        let before = fs::read(format!("{dest}/{sid}.jsonl")).unwrap();
        fs::write(&main, line("a completely different chat")).unwrap();
        let (sent, got) = transfer(&recv_root, &rec, files(&main, &sub), None);
        assert!(sent.unwrap_err().contains("different conversation"));
        assert!(got.is_err());
        assert_eq!(fs::read(format!("{dest}/{sid}.jsonl")).unwrap(), before, "untouched");

        // 5. into a folder that exists here instead of the sender's path
        let here = tmp("here");
        fs::write(&main, &longer).unwrap();
        let (sent, got) = transfer(&recv_root, &rec, files(&main, &sub), Some(here.clone()));
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        assert_eq!(got["cwd"], here);
        assert!(Path::new(&format!("{recv_root}/proj/{}/{sid}.jsonl", enc_cwd(&here))).exists());

        for d in [send_root, recv_root, here] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn an_unclaimed_cli_session_gets_a_record() {
        let send_root = tmp("send-cli");
        let recv_root = tmp("recv-cli");
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        let cwd = "C:\\Users\\shakaib\\code\\api";
        let sid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let main = format!("{send_root}/{sid}.jsonl");
        fs::write(&main, json!({ "type": "user", "cwd": cwd, "timestamp": "2026-09-19T08:00:00.000Z",
                                 "message": { "role": "user", "content": "fix the login bug" } }).to_string() + "\n").unwrap();
        let rec = json!({ "sessionId": null, "cliSessionId": sid, "cwd": cwd, "title": "fix the login bug" });
        let files = vec![(FileSpec { kind: Kind::Main, id: sid.into(), name: String::new(),
                                     size: fs::metadata(&main).unwrap().len() }, main.clone())];
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        let r: Value = serde_json::from_str(&fs::read_to_string(got["wrote"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(r["sessionId"], format!("local_{sid}"));
        assert_eq!(r["cwd"], cwd, "a Windows path is kept as the folder it ran in");
        assert_eq!(got["folderHere"], false);
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }
}
