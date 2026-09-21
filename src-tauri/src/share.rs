//! Nearby: send a chat to another Ferry on the same network.
//!
//! Off until the person turns it on. While on, this machine announces itself
//! over mDNS as `_ferry._tcp` and listens on one TCP port. Nothing is sent
//! anywhere else, and an address outside the local network is refused, so a
//! chat never crosses the router.
//!
//! A transfer, and why it holds up on a shared Wi-Fi:
//!  1. The two sides swap fresh X25519 keys, commit-then-reveal: the sender
//!     sends a hash of its key first and shows the key only after it has the
//!     receiver's. Everything after that is sealed with ChaCha20-Poly1305, one
//!     key per direction.
//!  2. Both screens show a six-digit code derived from that exchange. A device
//!     sitting in between has to run two exchanges, and the commitment stops it
//!     choosing a key that makes the two codes agree: its odds are one in a
//!     million, once, per attempt.
//!  3. The sender confirms the code matches the one on the other screen before
//!     it says anything about the chat. A device calling itself by a friend's
//!     name learns only the sender's machine name, because the friend never saw
//!     its code. Then the chat is described, and nothing moves until the person
//!     on the other side accepts it and picks the account it goes into.
//!  4. The receiver trusts nothing it is told. Ids and file names are checked
//!     before they become paths, sizes are capped, every file is staged and
//!     moved in only once all of them arrived, and a conversation already on
//!     this machine is never replaced by a different one.
//!
//! Receiving does not wait for Claude to be quit, unlike Ferry's other writes.
//! Those change records Claude already holds in memory, and Claude can write
//! its own copy back over them. A received chat is new to the account: its
//! transcript goes where Claude Code writes transcripts all the time, and its
//! record is one Claude has never loaded, so there is nothing to write back.
//! With Claude open it simply lists the chat the next time it starts.

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
const PROTO: u64 = 2;
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
    confirm: Option<mpsc::Sender<bool>>,
    cancel: Option<TcpStream>,
    incoming_sock: Option<TcpStream>,
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

/// The local network, and nothing past it: private IPv4 ranges, link-local,
/// and loopback for testing on one machine. The carrier-grade range
/// (100.64/10) is left out: it is shared across an ISP, and Tailscale uses it
/// for peers that may be anywhere. IPv6 is left out too: the listener is
/// IPv4, and Tailscale's IPv6 range sits inside the private fc00::/7.
fn lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_link_local() || v.is_loopback(),
        IpAddr::V6(v) => v.is_loopback(),
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
        && !windows_device(s)
}

/// CON, NUL, COM1 and the rest name devices on Windows, with or without an
/// extension, so COM1.jsonl is a serial port rather than a file.
fn windows_device(s: &str) -> bool {
    let u = s.to_ascii_uppercase();
    matches!(u.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((u.starts_with("COM") || u.starts_with("LPT")) && u.len() == 4 && u.as_bytes()[3].is_ascii_digit())
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

fn key_commit(pk: &[u8; 32]) -> String {
    hex(&Sha256::new().chain_update(b"ferry nearby v2 commit").chain_update(pk).finalize())
}

/// Swap keys, derive the two directions' keys and the code both people compare.
///
/// Commit-then-reveal. Without it the side that answers second sees the other
/// key before choosing its own, and a device in the middle could try keys until
/// its code with one person matched the code the other person already saw:
/// about a million tries, seconds of work. Here the sender (the initiator)
/// sends only a hash of its key, the receiver answers with its key, and the
/// sender reveals the key, which has to match the hash. Neither side can pick
/// its key after seeing the other's.
fn handshake(mut s: TcpStream, initiator: bool) -> Result<(Chan, Value, String), String> {
    let sk = x25519_dalek::EphemeralSecret::random_from_rng(OsRng);
    let pk = x25519_dalek::PublicKey::from(&sk);
    let msg = |b: Vec<u8>| serde_json::from_slice::<Value>(&b)
        .map_err(|_| "that isn't Ferry answering".to_string());
    let check = |p: &Value| -> Result<(), String> {
        if p["app"].as_str() != Some("ferry") { return Err("that isn't Ferry answering".into()); }
        if p["v"].as_u64() != Some(PROTO) {
            return Err("the other Ferry is a different version. Update both to the same one.".into());
        }
        Ok(())
    };
    let (peer, theirs) = if initiator {
        put(&mut s, json!({ "app": "ferry", "v": PROTO, "commit": key_commit(pk.as_bytes()),
                             "name": my_name(), "os": os_name() }).to_string().as_bytes())?;
        let p = msg(get(&mut s, 4096)?)?;
        check(&p)?;
        let theirs = unhex32(p["pk"].as_str().unwrap_or("")).ok_or("the other side sent a bad key")?;
        put(&mut s, json!({ "pk": hex(pk.as_bytes()) }).to_string().as_bytes())?;
        (p, theirs)
    } else {
        let p = msg(get(&mut s, 4096)?)?;
        check(&p)?;
        let commit = p["commit"].as_str().unwrap_or("").to_string();
        if commit.len() != 64 { return Err("the other side didn't commit to a key".into()); }
        put(&mut s, json!({ "app": "ferry", "v": PROTO, "pk": hex(pk.as_bytes()),
                             "name": my_name(), "os": os_name() }).to_string().as_bytes())?;
        let reveal = msg(get(&mut s, 4096)?)?;
        let theirs = unhex32(reveal["pk"].as_str().unwrap_or("")).ok_or("the other side sent a bad key")?;
        if key_commit(&theirs) != commit {
            return Err("the other side's key didn't match the one it committed to, so the connection was cut".into());
        }
        (p, theirs)
    };
    let shared = sk.diffie_hellman(&x25519_dalek::PublicKey::from(theirs));
    if !shared.was_contributory() { return Err("the key exchange was refused".into()); }

    let (pi, pr) = if initiator { (*pk.as_bytes(), theirs) } else { (theirs, *pk.as_bytes()) };
    let base: [u8; 32] = Sha256::new().chain_update(b"ferry nearby v2").chain_update(shared.as_bytes())
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
        // 100, so "local_" + id stays within safe_seg's 128
        if !safe_seg(id) || id.len() > 100 {
            return Err("the offer has a transcript id that isn't safe to use".into());
        }
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
    fn pdir(&self, cwd: &str) -> String { project_dir_in(&self.proj, cwd) }
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

/// A transcript Ferry wrote from a Cursor conversation says so on every line.
fn cursor_made(p: &str) -> bool {
    let Ok(f) = fs::File::open(p) else { return false };
    let mut line = String::new();
    if std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut line).is_err() { return false; }
    serde_json::from_str::<Value>(&line).map(|v| v["entrypoint"].as_str() == Some("cursor")).unwrap_or(false)
}

fn lines(p: &str) -> usize {
    fs::read(p).map(|b| b.iter().filter(|&&c| c == b'\n').count()).unwrap_or(0)
}

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

/// Put a received chat into an account. Everything is decided before anything
/// is written, and a failure part-way puts back what was there.
///
/// A chat this account already has is updated where it lives: its own folder,
/// its own record. The sender's folder and id only apply to a chat that is new
/// here. Otherwise a continued chat would land in a second folder while the
/// record kept pointing at the first, and a different conversation could slip
/// in under a known id through a folder the check never looked at.
fn commit(r: &Roots, stage: &str, offer: &Value, files: &[FileSpec], rec: &Value, a: &Answer)
          -> Result<Value, String> {
    if !safe_seg(&a.acct) || !safe_seg(&a.org) { return Err("pick an account on this machine".into()); }
    let scope = format!("{}/{}/{}", r.sess, a.acct, a.org);
    if !Path::new(&scope).is_dir() { return Err("that account has no folder on this machine".into()); }
    let main_id = &files[0].id;
    let claude_open = r.real && app_running();

    // Which record this is. The sender picks the id, so an id already here has
    // to belong to this same conversation, and an id this account deleted
    // isn't the sender's to reuse: the chat gets one of its own instead, and
    // the deleted chat stays deleted.
    let own = format!("local_{main_id}");
    let mut sid = rec["sessionId"].as_str()
        .filter(|s| s.starts_with("local_") && safe_seg(s))
        .map(String::from)
        .unwrap_or_else(|| own.clone());
    let rec_at = |id: &str| format!("{}/{}.json", scope, id);
    let tomb_at = |id: &str| format!("{}/deleted_{}", scope, id.trim_start_matches("local_"));
    if sid != own && !Path::new(&rec_at(&sid)).exists() && Path::new(&tomb_at(&sid)).exists() {
        sid = own.clone();
    }
    if !safe_seg(&sid) { return Err("the chat's id isn't safe to use".into()); }
    let dst = rec_at(&sid);
    let tomb = tomb_at(&sid);
    let existing = if Path::new(&dst).exists() {
        Some(read_json(&dst).ok_or("the chat already in that account can't be read. Nothing was changed.")?)
    } else { None };
    if let Some(e) = &existing {
        let mut ids: Vec<&str> = e["cliSessionId"].as_str().into_iter().collect();
        for k in ["priorCliSessionIds", "bridgeSessionIds"] {
            if let Some(v) = e[k].as_array() { ids.extend(v.iter().filter_map(|x| x.as_str())); }
        }
        if !ids.contains(&main_id.as_str()) {
            return Err("this account already has a different chat under that id. Nothing was changed.".into());
        }
    }
    // A chat deleted from this account is one Claude holds as deleted: bringing
    // it back while Claude runs is exactly the write Claude undoes. (Only the
    // chat's own id reaches here, so it is the same conversation.)
    if claude_open && existing.is_none() && Path::new(&tomb).exists() {
        return Err("this chat was deleted from that account. Quit Claude and send it again, \
                    or Claude may delete it again. Nothing was changed.".into());
    }

    // Where its conversation goes: the folder this account already files it
    // under, or for a new chat the one picked here, else the sender's.
    let known = existing.as_ref().and_then(|e| e["cwd"].as_str()).filter(|c| !c.is_empty()).map(String::from);
    // A chat here with no folder on its record gets one now, like a new chat,
    // and its record is pointed at it. That is a record write, so not while
    // Claude holds the record.
    let repoint = existing.is_some() && known.is_none();
    if repoint && claude_open {
        return Err("that chat has no folder in this account yet. Quit Claude and send it again, \
                    so Ferry can give it one. Nothing was changed.".into());
    }
    let cwd = match known {
        Some(c) => c,
        None => {
            let theirs = clean(offer["cwd"].as_str().unwrap_or(""), 1024);
            match a.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
                Some(f) => {
                    if !Path::new(f).is_dir() { return Err(format!("no such folder: {f}")); }
                    f.to_string()
                }
                None => theirs,
            }
        }
    };
    if cwd.is_empty() {
        return Err("this chat has no folder. Choose one on this machine to put it in.".into());
    }
    // A real path, never a way out of ~/.claude/projects. ".." is checked on
    // the path and again on the folder it maps to, because the older encoding
    // keeps dots and would map ".." to projects/.. itself.
    if cwd.split(|c| c == '/' || c == '\\').any(|seg| seg == "." || seg == "..") {
        return Err("that folder path isn't usable. Choose a folder on this machine.".into());
    }
    let dir = r.pdir(&cwd);
    let escapes = Path::new(&dir).components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir));
    if escapes || !under(&r.proj, &dir) || Path::new(&dir) == Path::new(&r.proj) {
        return Err("that folder doesn't map inside ~/.claude/projects".into());
    }

    let (mut plan, mut kept) = (vec![], 0usize);
    for f in files {
        let src = format!("{}/{}", stage, f.stage_name());
        let to = match f.kind {
            Kind::Main => format!("{}/{}.jsonl", dir, f.id),
            Kind::Sub => format!("{}/{}/subagents/{}", dir, f.id, f.name),
        };
        if !under(&dir, &to) { return Err("a file would have landed outside the chat's folder".into()); }
        if !Path::new(&to).exists() { plan.push((src, to, false)); continue; }
        match compare(&to, &src)? {
            Cmp::Same | Cmp::Shorter => kept += 1,        // this machine already has all of it
            Cmp::Longer => plan.push((src, to, true)),    // the conversation went on since
            // Both written by Ferry from the same Cursor conversation: Cursor folds
            // a trailing tool run into the next reply once the chat goes on, so a
            // later rendering doesn't start with the earlier one. It is still the
            // same chat, and the one with at least as many lines is the newer.
            Cmp::Differs if f.kind == Kind::Main && cursor_made(&to) && cursor_made(&src)
                            && lines(&src) >= lines(&to) => plan.push((src, to, true)),
            Cmp::Differs => return Err(format!(
                "this machine already has a different conversation under the same id ({}). \
                 Nothing was changed.", f.id)),
        }
    }

    // The record. A chat already here keeps the one it has: its title, its
    // sessions and its folder are this machine's, and Claude may hold it in
    // memory. With Claude closed, only its turn count and last activity move
    // forward. A new chat gets a record built from a fixed set of fields, never
    // copied over: the sender's came off the network, and a session id list
    // such as bridgeSessionIds is followed as a path by actions like set_folder.
    let info = read_session(&format!("{}/{}", stage, files[0].stage_name()))
        .ok_or("the conversation arrived but can't be read")?;
    let num = |k: &str, fb: &Value| if rec[k].as_u64().map(|n| n > 0).unwrap_or(false) { rec[k].clone() } else { fb.clone() };
    let out: Option<Value> = match &existing {
        Some(_) if claude_open => None,
        Some(e) => {
            let mut e = e.clone();
            for (k, v) in [("completedTurns", &info["turns"]), ("lastActivityAt", &info["last"])] {
                let (have, got) = (e[k].as_u64().unwrap_or(0), v.as_u64().unwrap_or(0));
                if got > have { e[k] = json!(got); }
            }
            if repoint { e["cwd"] = json!(cwd); e["originCwd"] = json!(cwd); }
            Some(e)
        }
        None => {
            let tpl = template_record(&scope);
            let title = rec["title"].as_str().map(|t| clean(t, 300)).filter(|t| !t.is_empty())
                .unwrap_or_else(|| clean(info["title"].as_str().unwrap_or("(untitled)"), 300));
            let mut o = json!({
                "sessionId": sid, "cliSessionId": main_id,
                "title": title,
                "titleSource": if rec["titleSource"].as_str() == Some("user") { "user" } else { "auto" },
                "createdAt": num("createdAt", &info["created"]),
                "lastActivityAt": num("lastActivityAt", &info["last"]),
                "lastFocusedAt": num("lastFocusedAt", &info["last"]),
                "completedTurns": num("completedTurns", &info["turns"]),
                "isArchived": false,
            });
            let model = rec["model"].as_str().or(info["model"].as_str()).map(|m| clean(m, 100)).unwrap_or_default();
            if !model.is_empty() { o["model"] = json!(model); }
            // earlier sessions of the same chat, but only ones whose file came with it
            let arrived: HashSet<&str> = files.iter().filter(|f| f.kind == Kind::Main).map(|f| f.id.as_str()).collect();
            for k in ["priorCliSessionIds", "bridgeSessionIds"] {
                let ids: Vec<&str> = rec[k].as_array().map(|v| v.iter().filter_map(|x| x.as_str())
                    .filter(|i| *i != main_id.as_str() && arrived.contains(i)).collect()).unwrap_or_default();
                if !ids.is_empty() { o[k] = json!(ids); }
            }
            // fields that only resolve on the machine that wrote them come from
            // a record this account already has, or are left out
            for k in INHERIT {
                match tpl.as_ref().map(|t| t[k].clone()).filter(|v| !v.is_null()) {
                    Some(v) => o[k] = v,
                    None => { if let Some(m) = o.as_object_mut() { m.remove(k); } }
                }
            }
            o["cwd"] = json!(cwd);
            o["originCwd"] = json!(cwd);
            Some(o)
        }
    };

    // The writes. Any failure takes back the files it added and restores the
    // ones it replaced, from copies kept in the staging folder.
    let backup = format!("{stage}/.replaced");
    let (mut added, mut replaced): (Vec<String>, Vec<(String, String)>) = (vec![], vec![]);
    let wrote = (|| -> Result<(), String> {
        for (i, (src, to, replace)) in plan.iter().enumerate() {
            if let Some(p) = Path::new(to).parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
            if *replace {
                fs::create_dir_all(&backup).map_err(|e| e.to_string())?;
                let keep = format!("{backup}/{i}");
                fs::copy(to, &keep).map_err(|e| e.to_string())?;
                replaced.push((keep, to.clone()));
                r.snap(to, "nearby-replaced");
            }
            if !*replace { added.push(to.clone()); }    // before the copy: a partial one goes too
            fs::copy(src, to).map_err(|e| e.to_string())?;
        }
        if let Some(o) = &out {
            r.snap(&dst, "nearby");
            fs::write(&dst, serde_json::to_string_pretty(o).unwrap()).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if let Err(e) = wrote {
        for f in &added { let _ = fs::remove_file(f); }
        for (keep, to) in &replaced { let _ = fs::copy(keep, to); }
        return Err(e);
    }
    if out.is_some() && existing.is_none() && Path::new(&tomb).exists() {
        r.snap(&tomb, "undelete");
        let _ = fs::remove_file(&tomb);
    }

    Ok(json!({ "ok": true, "wrote": dst, "cwd": cwd,
               "title": out.as_ref().or(existing.as_ref()).map(|o| o["title"].clone()).unwrap_or(Value::Null),
               "claudeOpen": claude_open, "known": existing.is_some(), "recordKept": out.is_none(),
               "folderHere": Path::new(&cwd).is_dir(), "written": plan.len(), "kept": kept,
               "acct": a.acct, "org": a.org }))
}

/* ---------- receiving ---------- */

type Decide = dyn Fn(&Value, &Value, &str) -> Option<Answer> + Send + Sync;
/// Put the code in front of the person before anything about the chat is
/// known. False means busy with another transfer: hang up.
type Pair = dyn Fn(&Value, &str, Option<TcpStream>) -> bool + Send + Sync;

struct Ctx { roots: Roots, pair: Box<Pair>, decide: Box<Decide> }

fn set_incoming(k: &str, v: Value) {
    if let Some(i) = st().incoming.as_mut() { i[k] = v; }
}

/// Change the state only if it is still the one expected, so a hang-up the
/// person chose (declined) isn't reported as the sender leaving (cancelled).
fn set_incoming_from(was: &str, now: &str) {
    if let Some(i) = st().incoming.as_mut() {
        if i["state"].as_str() == Some(was) { i["state"] = json!(now); }
    }
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
    // The code goes up on this screen now, while nothing about the chat has
    // been said. The sender compares it with this screen and confirms, and only
    // then describes the chat. Declining here hangs up.
    if !(ctx.pair)(&peer, &code, ch.s.try_clone().ok()) { return Ok(json!({ "busy": true })); }
    let res = after_pairing(&mut ch, ctx, &peer, &code);
    let mut g = st();
    g.incoming_sock = None;
    settle(&mut g.incoming, &res);
    drop(g);
    res
}

/// Every way out of a transfer ends here, so none is left looking live. One
/// left in pairing or asking would turn every later sender away as busy, and
/// the card would never clear.
fn settle(inc: &mut Option<Value>, res: &Result<Value, String>) {
    if let Some(i) = inc.as_mut() {
        if matches!(i["state"].as_str(), Some("pairing" | "asking" | "receiving")) {
            match res {
                Ok(_) => i["state"] = json!("cancelled"),
                Err(e) => { i["state"] = json!("error"); i["msg"] = json!(e); }
            }
        }
    }
}

fn after_pairing(ch: &mut Chan, ctx: &Ctx, peer: &Value, code: &str) -> Result<Value, String> {
    let _ = ch.s.set_read_timeout(Some(ASK_FOR + Duration::from_secs(30)));
    // The sender hanging up here, or cancelling, is theirs to do: not a fault.
    let first = match ch.recv_json() {
        Ok(v) => v,
        Err(_) => { set_incoming_from("pairing", "cancelled"); return Ok(json!({ "cancelled": true })); }
    };
    if first["t"].as_str() != Some("offer") {
        set_incoming_from("pairing", "cancelled");
        return Ok(json!({ "cancelled": true }));
    }
    let _ = ch.s.set_read_timeout(Some(Duration::from_secs(30)));
    let offer = first;
    let files = match check_offer(&offer) {
        Ok(f) => f,
        Err(e) => { let _ = ch.send_json(&json!({ "t": "decline", "msg": e })); return Err(e); }
    };
    let Some(ans) = (ctx.decide)(peer, &offer, code) else {
        let _ = ch.send_json(&json!({ "t": "decline" }));
        return Ok(json!({ "declined": true }));
    };
    ch.send_json(&json!({ "t": "accept" }))?;
    set_incoming("state", json!("receiving"));

    let stage = format!("{}/incoming/{}", ctx.roots.vault, rand_hex(6));
    let res = (|| {
        fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let mut got = 0u64;
        for f in &files { receive_file(ch, &stage, f, &mut got)?; }
        let rec = ch.recv_json()?;
        commit(&ctx.roots, &stage, &offer, &files, &rec, &ans)
    })();
    let _ = fs::remove_dir_all(&stage);
    let _ = ch.send_json(&match &res {
        Ok(v) => json!({ "t": "done", "ok": true, "title": v["title"] }),
        Err(e) => json!({ "t": "done", "ok": false, "msg": e }),
    });
    if let Ok(v) = &res {
        set_incoming("result", v.clone());
        set_incoming("state", json!("done"));
    }
    res
}

/// The app's way of pairing: show who is connecting and the code.
fn pair_person(peer: &Value, code: &str, sock: Option<TcpStream>) -> bool {
    let mut g = st();
    let busy = g.incoming.as_ref()
        .map(|i| matches!(i["state"].as_str(), Some("pairing" | "asking" | "receiving"))).unwrap_or(false);
    if !g.on || busy { return false; }
    g.incoming = Some(json!({
        "xfer": rand_hex(4),
        "from": clean(peer["name"].as_str().unwrap_or("Someone"), 60),
        "os": clean(peer["os"].as_str().unwrap_or(""), 12),
        "code": code, "state": "pairing", "got": 0,
    }));
    g.incoming_sock = sock;
    true
}

/// The app's way of deciding: put the offer in front of the person and wait.
fn ask_person(peer: &Value, offer: &Value, code: &str) -> Option<Answer> {
    let (tx, rx) = mpsc::channel();
    {
        let mut g = st();
        if !g.on { return None; }
        let files = offer["files"].as_array().map(|a| a.len()).unwrap_or(0);
        let xfer = g.incoming.as_ref().and_then(|i| i["xfer"].as_str().map(String::from))
            .unwrap_or_else(|| rand_hex(4));
        g.incoming = Some(json!({
            "xfer": xfer,
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
                    let ctx = Ctx { roots: Roots::real(), pair: Box::new(pair_person), decide: Box::new(ask_person) };
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
        if let Some(c) = g.confirm.take() { let _ = c.send(false); }
        if let Some(sk) = g.incoming_sock.take() { let _ = sk.shutdown(std::net::Shutdown::Both); }
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
            set: &mut dyn FnMut(&str, Value), confirm: &dyn Fn() -> bool) -> Result<&'static str, String> {
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

    // Before a word about the chat goes out, this side checks the code against
    // the other screen. A device posing under a friend's name gets no further:
    // it has learned this machine's name, which mDNS already announced.
    set("state", json!("confirm"));
    if !confirm() {
        let _ = ch.send_json(&json!({ "t": "abort" }));
        return Ok("cancelled");
    }
    set("state", json!("waiting"));

    let total: u64 = files.iter().map(|f| f.0.size).sum();
    let offer = json!({
        "t": "offer",
        "title": clean(rec["title"].as_str().unwrap_or("(untitled)"), 200),
        "turns": rec["completedTurns"], "bytes": total,
        "cwd": rec["cwd"].as_str().unwrap_or(""),
        "files": files.iter().map(|(f, _)| f.json()).collect::<Vec<_>>(),
    });
    // They may have hung up while this side was checking the code. That is a
    // no, not a fault.
    let _ = ch.s.set_read_timeout(Some(ASK_FOR + Duration::from_secs(30)));
    let r = match ch.send_json(&offer).and_then(|_| ch.recv_json()) {
        Ok(r) => r,
        Err(e) if e == "the connection closed" => {
            set("msg", json!("They declined, or closed Ferry."));
            return Ok("declined");
        }
        Err(e) => return Err(e),
    };
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

/// A Cursor chat has no transcript on disk: its turns live in Cursor's own
/// database. For a send it is written in Claude Code's transcript shape, the
/// lines "Add to account" writes, into a folder of its own that goes when the
/// send ends. The other machine receives an ordinary Claude chat, and this
/// machine's ~/.claude stays as it was. Cursor is only read, so it can stay open.
fn cursor_outgoing(cid: &str) -> Result<(Value, Vec<(FileSpec, String)>, Option<String>), String> {
    if !safe_seg(cid) || cid.len() > 100 { return Err("that Cursor chat has an id Ferry can't send".into()); }
    let chat = cursor_chat(cid).ok_or("no such Cursor chat")?;
    let body = cursor_transcript(&chat)?;
    let dir = format!("{}/outgoing/{}", vault(), rand_hex(6));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let p = format!("{dir}/{cid}.jsonl");
    if let Err(e) = fs::write(&p, &body) { let _ = fs::remove_dir_all(&dir); return Err(e.to_string()); }
    let info = read_session(&p).unwrap_or_else(|| json!({}));
    // what the other person sees on the card before accepting: Cursor's name,
    // else the first prompt, and how many turns
    let title = chat["title"].as_str().filter(|t| !t.is_empty() && *t != "(unnamed)")
        .or(info["title"].as_str()).unwrap_or("(untitled)");
    let mut rec = json!({ "sessionId": null, "cliSessionId": cid, "cwd": chat["folder"], "title": title,
                          "completedTurns": info["turns"] });
    for (k, v) in [("createdAt", &chat["created"]), ("lastActivityAt", &chat["last"])] {
        if v.as_u64().map(|n| n > 0).unwrap_or(false) { rec[k] = v.clone(); }   // a zero is no date
    }
    let spec = FileSpec { kind: Kind::Main, id: cid.into(), name: String::new(), size: body.len() as u64 };
    Ok((rec, vec![(spec, p)], Some(dir)))
}

pub fn send(path: String, to: String) -> Result<Value, String> {
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
    let live = st().outgoing.as_ref()
        .map(|o| matches!(o["state"].as_str(), Some("connecting" | "waiting" | "confirm" | "sending"))).unwrap_or(false);
    if live { return Err("already sending a chat".into()); }
    let (rec, files, tmp) = match path.strip_prefix("cursor:") {
        Some(cid) => cursor_outgoing(cid)?,
        None => { let (rec, tr) = chat_parts(&path)?; let f = outgoing_files(&tr); (rec, f, None) }
    };
    if files.is_empty() {
        return Err("this chat's transcript is gone from disk, so there is nothing to send".into());
    }
    let total: u64 = files.iter().map(|f| f.0.size).sum();
    {
        let mut g = st();
        // Checked again under the same lock that claims it: converting a Cursor
        // chat takes long enough for a second click to get in between.
        let live = g.outgoing.as_ref()
            .map(|o| matches!(o["state"].as_str(), Some("connecting" | "waiting" | "confirm" | "sending"))).unwrap_or(false);
        if live {
            drop(g);
            if let Some(t) = &tmp { let _ = fs::remove_dir_all(t); }
            return Err("already sending a chat".into());
        }
        g.outgoing = Some(json!({
            "state": "connecting", "to": if shown.is_empty() { to.clone() } else { shown },
            "title": rec["title"], "sent": 0, "total": total, "code": "",
        }));
    }
    std::thread::spawn(move || {
        let mut set = |k: &str, v: Value| { if let Some(o) = st().outgoing.as_mut() { o[k] = v; } };
        let confirm = || {
            let (tx, rx) = mpsc::channel();
            st().confirm = Some(tx);
            let yes = rx.recv_timeout(ASK_FOR).unwrap_or(false);
            st().confirm = None;
            yes
        };
        let r = run_send(&addrs, &rec, &files, &mut set, &confirm);
        if let Some(t) = &tmp { let _ = fs::remove_dir_all(t); }   // a Cursor chat's one-off transcript
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
        if !safe_seg(&acct) || !safe_seg(&org) || !Path::new(&format!("{}/{}/{}", sess(), acct, org)).is_dir() {
            return Err("pick an account on this machine".into());
        }
        if let Some(f) = folder.as_deref().filter(|f| !f.trim().is_empty()) {
            if !Path::new(f).is_dir() { return Err(format!("no such folder: {f}")); }
        }
    }
    let mut g = st();
    if let Some(tx) = g.answer.take() {
        drop(g);
        tx.send(Answer { accept, acct, org, folder }).map_err(|_| "that offer has already gone".to_string())?;
        return Ok(json!({ "ok": true }));
    }
    // Still pairing: there is no offer yet, so declining means hanging up.
    if !accept && g.incoming.as_ref().map(|i| i["state"] == "pairing").unwrap_or(false) {
        if let Some(sk) = g.incoming_sock.take() { let _ = sk.shutdown(std::net::Shutdown::Both); }
        if let Some(i) = g.incoming.as_mut() { i["state"] = json!("declined"); }
        return Ok(json!({ "ok": true }));
    }
    Err("that offer has already gone".into())
}

/// Clear the staging folders a transfer left behind when Ferry quit mid-way.
/// Only ones untouched for an hour: a live transfer writes to its own often,
/// and a second copy of Ferry may be running.
pub fn sweep() { sweep_in(&vault(), Duration::from_secs(3600)); }
fn sweep_in(vault: &str, age: Duration) {
    for d in ["outgoing", "incoming"] {
        let Ok(rd) = fs::read_dir(format!("{vault}/{d}")) else { continue };
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok()
                .and_then(|t| t.elapsed().ok()).map(|el| el >= age).unwrap_or(false);
            if old { let _ = fs::remove_dir_all(e.path()); }
        }
    }
}

/// The sender's half of the code check.
pub fn confirm_send(yes: bool) -> Result<Value, String> {
    let tx = st().confirm.take().ok_or("nothing is waiting on that")?;
    let _ = tx.send(yes);
    Ok(state())
}

pub fn cancel() -> Value {
    let mut g = st();
    if let Some(tx) = g.confirm.take() { let _ = tx.send(false); }
    if let Some(s) = g.cancel.take() { let _ = s.shutdown(std::net::Shutdown::Both); }
    if let Some(o) = g.outgoing.as_mut() {
        if matches!(o["state"].as_str(), Some("connecting" | "waiting" | "confirm" | "sending")) {
            o["state"] = json!("cancelled");
        }
    }
    drop(g);
    state()
}

/// Clear a finished transfer off the screen. One still asking or moving stays.
pub fn dismiss() -> Value {
    let mut g = st();
    if g.incoming.as_ref().map(|i| !matches!(i["state"].as_str(), Some("pairing" | "asking" | "receiving"))).unwrap_or(false) {
        g.incoming = None;
    }
    if g.outgoing.as_ref().map(|o| !matches!(o["state"].as_str(), Some("connecting" | "waiting" | "confirm" | "sending"))).unwrap_or(false) {
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
            json!({ "bytes": 3, "files": [{ "kind": "main", "id": "a".repeat(101), "size": 3 }] }),
            json!({ "bytes": 3, "files": [{ "kind": "main", "id": "COM1", "size": 3 }] }),
            json!({ "bytes": 3, "files": [{ "kind": "main", "id": "nul", "size": 3 }] }),
            json!({ "bytes": 6, "files": [{ "kind": "main", "id": "a", "size": 3 },
                                          { "kind": "sub", "id": "a", "name": "lpt9.jsonl", "size": 3 }] }),
        ] {
            assert!(check_offer(&bad).is_err(), "should refuse {bad}");
        }
    }

    #[test]
    fn nothing_past_the_router() {
        for ok in ["192.168.1.20:1", "10.0.0.2:1", "172.16.4.4:1", "169.254.1.1:1", "127.0.0.1:1"] {
            assert!(parse_addr(ok).is_ok(), "{ok}");
        }
        for bad in ["8.8.8.8:53", "1.1.1.1", "142.250.1.1:443", "100.101.1.1:1", "100.128.0.1:1", "nonsense"] {
            assert!(parse_addr(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_addr("192.168.1.9").unwrap()[0].port(), PORT);
        assert!(parse_addr("[fd7a:115c:a1e0::1]:53711").is_err(), "Tailscale's IPv6 range");
        assert!(parse_addr("[fe80::1]:53711").is_err());
        assert!(parse_addr("[::1]:53711").is_ok());
    }

    /// Sends one chat from a sender's tree into a receiver's tree over a real
    /// socket, the way two machines would. Returns the receiver's answer.
    fn transfer(recv_root: &str, rec: &Value, files: Vec<(FileSpec, String)>, folder: Option<String>)
                -> (Result<&'static str, String>, Result<Value, String>) {
        transfer_as(recv_root, rec, files, folder, true)
    }

    fn transfer_as(recv_root: &str, rec: &Value, files: Vec<(FileSpec, String)>, folder: Option<String>,
                   sender_confirms: bool) -> (Result<&'static str, String>, Result<Value, String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let root = recv_root.to_string();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let ctx = Ctx {
                roots: Roots { sess: format!("{root}/sess"), proj: format!("{root}/proj"),
                               vault: format!("{root}/vault"), real: false },
                pair: Box::new(|_: &Value, _: &str, _: Option<TcpStream>| true),
                decide: Box::new(move |_: &Value, _: &Value, _: &str| Some(Answer {
                    accept: true, acct: "acct-b".into(), org: "org-b".into(), folder: folder.clone() })),
            };
            serve(s, &ctx)
        });
        let sent = run_send(&[addr], rec, &files, &mut |_, _| {}, &move || sender_confirms);
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

        // 5. a chat this account already has stays in its own folder, even when
        //    a different one is picked: a second copy would split the chat
        let here = tmp("here");
        fs::write(&main, &longer).unwrap();
        let (sent, got) = transfer(&recv_root, &rec, files(&main, &sub), Some(here.clone()));
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        assert_eq!(got["cwd"], cwd);
        assert_eq!(got["known"], true);
        assert!(!Path::new(&format!("{recv_root}/proj/{}/{sid}.jsonl", enc_cwd(&here))).exists());

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

    #[test]
    fn nothing_moves_until_the_sender_confirms_the_code() {
        let send_root = tmp("send-no");
        let recv_root = tmp("recv-no");
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        let sid = "cccccccc-dddd-4eee-8fff-000000000000";
        let main = format!("{send_root}/{sid}.jsonl");
        fs::write(&main, json!({ "type": "user", "cwd": "/Users/a/p", "timestamp": "2026-09-19T08:00:00.000Z",
                                 "message": { "role": "user", "content": "hi" } }).to_string() + "\n").unwrap();
        let rec = json!({ "sessionId": format!("local_{sid}"), "cliSessionId": sid, "cwd": "/Users/a/p" });
        let files = vec![(FileSpec { kind: Kind::Main, id: sid.into(), name: String::new(),
                                     size: fs::metadata(&main).unwrap().len() }, main.clone())];
        let (sent, got) = transfer_as(&recv_root, &rec, files, None, false);
        assert_eq!(sent.unwrap(), "cancelled");
        assert_eq!(got.unwrap()["cancelled"], true);
        assert!(!Path::new(&format!("{recv_root}/proj")).exists(), "not one file was written");
        assert_eq!(fs::read_dir(format!("{recv_root}/sess/acct-b/org-b")).unwrap().count(), 0);
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_crafted_record_cannot_add_fields_or_point_at_other_files() {
        let send_root = tmp("send-evil");
        let recv_root = tmp("recv-evil");
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        let sid = "dddddddd-eeee-4fff-8000-111111111111";
        let main = format!("{send_root}/{sid}.jsonl");
        fs::write(&main, json!({ "type": "user", "cwd": "/Users/a/p", "timestamp": "2026-09-19T08:00:00.000Z",
                                 "message": { "role": "user", "content": "hi" } }).to_string() + "\n").unwrap();
        let rec = json!({
            "sessionId": format!("local_{sid}"), "cliSessionId": "../../../../etc/passwd", "cwd": "/Users/a/p",
            "title": "normal title", "completedTurns": "lots",
            "bridgeSessionIds": ["../../../../.ssh/authorized_keys", sid, "never-sent"],
            "priorCliSessionIds": ["../../x"],
            "scratchPromptRecents": "/Users/victim", "someFieldClaudeNeverWrote": { "a": 1 },
        });
        let files = vec![(FileSpec { kind: Kind::Main, id: sid.into(), name: String::new(),
                                     size: fs::metadata(&main).unwrap().len() }, main.clone())];
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        assert_eq!(sent.unwrap(), "done");
        let r: Value = serde_json::from_str(&fs::read_to_string(got.unwrap()["wrote"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(r["cliSessionId"], sid, "the chat's own session is the file that arrived");
        assert!(r.get("bridgeSessionIds").is_none(), "no id that didn't arrive survives: {r}");
        assert!(r.get("priorCliSessionIds").is_none());
        assert!(r.get("scratchPromptRecents").is_none() && r.get("someFieldClaudeNeverWrote").is_none());
        assert!(r["completedTurns"].is_u64(), "a non-number falls back to the transcript's count");
        assert_eq!(r["title"], "normal title");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_key_that_differs_from_its_commitment_is_refused() {
        let (mut c, s) = pair();
        let t = std::thread::spawn(move || handshake(s, false).map(|_| ()));
        let promised = x25519_dalek::PublicKey::from(&x25519_dalek::EphemeralSecret::random_from_rng(OsRng));
        let swapped = x25519_dalek::PublicKey::from(&x25519_dalek::EphemeralSecret::random_from_rng(OsRng));
        put(&mut c, json!({ "app": "ferry", "v": PROTO, "commit": key_commit(promised.as_bytes()),
                            "name": "relay", "os": "x" }).to_string().as_bytes()).unwrap();
        let _responder_key = get(&mut c, 4096).unwrap();
        put(&mut c, json!({ "pk": hex(swapped.as_bytes()) }).to_string().as_bytes()).unwrap();
        let err = t.join().unwrap().unwrap_err();
        assert!(err.contains("committed"), "{err}");
    }

    fn one_file(root: &str, sid: &str, cwd: &str) -> (String, Vec<(FileSpec, String)>) {
        let main = format!("{root}/{sid}.jsonl");
        fs::write(&main, json!({ "type": "user", "cwd": cwd, "timestamp": "2026-09-19T08:00:00.000Z",
                                 "message": { "role": "user", "content": "hello" } }).to_string() + "\n").unwrap();
        let spec = FileSpec { kind: Kind::Main, id: sid.into(), name: String::new(),
                              size: fs::metadata(&main).unwrap().len() };
        (main.clone(), vec![(spec, main)])
    }

    #[test]
    fn a_folder_of_dot_dot_goes_nowhere() {
        let send_root = tmp("send-dots");
        let recv_root = tmp("recv-dots");
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        let sid = "eeeeeeee-ffff-4000-8111-222222222222";
        for cwd in ["..", "/Users/a/../..", "C:\\Users\\..\\.."] {
            let (_, files) = one_file(&send_root, sid, cwd);
            let rec = json!({ "sessionId": format!("local_{sid}"), "cliSessionId": sid, "cwd": cwd });
            let (sent, got) = transfer(&recv_root, &rec, files, None);
            assert!(sent.is_err() && got.is_err(), "cwd {cwd} must be refused");
        }
        // and the same through the folder the receiver picks
        let (_, files) = one_file(&send_root, sid, "/Users/a/p");
        let rec = json!({ "sessionId": format!("local_{sid}"), "cliSessionId": sid, "cwd": "/Users/a/p" });
        let (_, got) = transfer(&recv_root, &rec, files, Some("..".into()));
        assert!(got.is_err());
        assert!(!Path::new(&format!("{recv_root}/{sid}.jsonl")).exists());
        assert!(!Path::new(&format!("{recv_root}/proj")).exists(), "nothing was written anywhere");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_chat_cannot_take_over_another_chats_record() {
        let send_root = tmp("send-take");
        let recv_root = tmp("recv-take");
        let scope = format!("{recv_root}/sess/acct-b/org-b");
        fs::create_dir_all(&scope).unwrap();
        let victim = json!({ "sessionId": "local_victim", "cliSessionId": "aaaaaaaa-0000-4000-8000-000000000001",
                             "title": "The receiver's own chat", "cwd": "/Users/b/p" });
        fs::write(format!("{scope}/local_victim.json"), victim.to_string()).unwrap();
        let sid = "bbbbbbbb-0000-4000-8000-000000000002";
        let (_, files) = one_file(&send_root, sid, "/Users/a/p");
        let rec = json!({ "sessionId": "local_victim", "cliSessionId": sid, "cwd": "/Users/a/p" });
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        assert!(sent.unwrap_err().contains("different chat"));
        assert!(got.is_err());
        let after: Value = serde_json::from_str(&fs::read_to_string(format!("{scope}/local_victim.json")).unwrap()).unwrap();
        assert_eq!(after, victim, "the other chat's record is untouched");
        assert!(!Path::new(&format!("{recv_root}/proj")).exists(), "and its transcript never landed");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn nothing_about_the_chat_is_said_before_the_code_is_confirmed() {
        let send_root = tmp("send-quiet");
        let sid = "ffffffff-0000-4000-8000-000000000003";
        let (_, files) = one_file(&send_root, sid, "/Users/ali/clients/acme-merger");
        let rec = json!({ "sessionId": format!("local_{sid}"), "cliSessionId": sid,
                          "cwd": "/Users/ali/clients/acme-merger", "title": "Draft layoff plan" });
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        // an impostor: completes the key exchange, then records whatever it is told
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let (mut ch, _, _) = handshake(s, false).unwrap();
            ch.recv_json().unwrap()
        });
        let sent = run_send(&[addr], &rec, &files, &mut |_, _| {}, &|| false);
        assert_eq!(sent.unwrap(), "cancelled");
        let heard = t.join().unwrap();
        assert_eq!(heard["t"], "abort");
        let said = heard.to_string();
        assert!(!said.contains("layoff") && !said.contains("acme"), "it learned: {said}");
        let _ = fs::remove_dir_all(send_root);
    }

    #[test]
    fn hanging_up_while_pairing_reads_as_declined() {
        let send_root = tmp("send-hangup");
        let sid = "abababab-0000-4000-8000-000000000004";
        let (_, files) = one_file(&send_root, sid, "/Users/a/p");
        let rec = json!({ "sessionId": format!("local_{sid}"), "cliSessionId": sid, "cwd": "/Users/a/p" });
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let (ch, _, _) = handshake(s, false).unwrap();
            let _ = ch.s.shutdown(std::net::Shutdown::Both);     // the person pressed Decline
        });
        let sent = run_send(&[addr], &rec, &files, &mut |_, _| {}, &|| true);
        t.join().unwrap();
        assert_eq!(sent.unwrap(), "declined");
        let _ = fs::remove_dir_all(send_root);
    }

    #[test]
    fn a_cursor_chat_arrives_as_a_claude_chat() {
        let send_root = tmp("send-cursor");
        let recv_root = tmp("recv-cursor");
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        let cid = "0c0c0c0c-1d1d-4e2e-8f3f-404040404040";
        let msgs = vec![
            json!({ "role": "user", "text": "make the sidebar collapsible", "t": "2026-09-20T09:00:00.000Z" }),
            json!({ "role": "assistant", "text": "Done: a toggle in the header.", "t": "2026-09-20T09:00:05.000Z" }),
        ];
        let body = transcript_lines(cid, "/Users/ali/code/site", &msgs);
        assert_eq!(body.lines().count(), 2);
        let main = format!("{send_root}/{cid}.jsonl");
        fs::write(&main, &body).unwrap();
        let files = vec![(FileSpec { kind: Kind::Main, id: cid.into(), name: String::new(), size: body.len() as u64 }, main)];
        // what cursor_outgoing sends: no account record, Cursor's title when it has one
        let rec = json!({ "sessionId": null, "cliSessionId": cid, "cwd": "/Users/ali/code/site", "title": "",
                          "createdAt": 1789900000000u64, "lastActivityAt": 1789900005000u64 });
        let (sent, got) = transfer(&recv_root, &rec, files.clone(), None);
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        let r: Value = serde_json::from_str(&fs::read_to_string(got["wrote"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(r["sessionId"], format!("local_{cid}"));
        assert_eq!(r["title"], "make the sidebar collapsible", "named from the first prompt");
        assert_eq!(r["completedTurns"], 1);
        assert_eq!(r["lastActivityAt"], 1789900005000u64);
        let landed = fs::read_to_string(format!("{recv_root}/proj/{}/{cid}.jsonl", enc_cwd("/Users/ali/code/site"))).unwrap();
        assert!(landed.contains("\"entrypoint\":\"cursor\""), "it still says where it came from");

        // a Cursor chat with no workspace folder needs one picked on the other
        // side (a fresh machine: on this one the chat is known and keeps its own)
        let fresh = tmp("recv-cursor-2");
        fs::create_dir_all(format!("{fresh}/sess/acct-b/org-b")).unwrap();
        let rec = json!({ "sessionId": null, "cliSessionId": cid, "cwd": "", "title": "" });
        let (_, got) = transfer(&fresh, &rec, files.clone(), None);
        assert!(got.unwrap_err().contains("no folder"));
        let here = tmp("here-cursor");
        let (sent, got) = transfer(&fresh, &rec, files, Some(here.clone()));
        assert_eq!(sent.unwrap(), "done");
        assert_eq!(got.unwrap()["cwd"], here);
        for d in [send_root, recv_root, here, fresh] { let _ = fs::remove_dir_all(d); }
    }

    /// Against the Cursor on this machine, read only: the first conversation
    /// with anything in it goes out as a one-off transcript, and the folder it
    /// was written to is the only thing created. cargo test -- --ignored
    #[test]
    #[ignore]
    fn cursor_outgoing_reads_a_real_chat() {
        let Some(chat) = cursor::cursor_chats().into_iter().find(|c| c["bubbles"].as_i64().unwrap_or(0) > 1) else {
            eprintln!("no Cursor chats on this machine"); return;
        };
        let cid = chat["id"].as_str().unwrap().to_string();
        let before_own = Path::new(&format!("{}/{}.jsonl", project_dir(chat["folder"].as_str().unwrap_or("")), cid)).exists();
        let (rec, files, tmp) = cursor_outgoing(&cid).expect("a real Cursor chat converts");
        let dir = tmp.expect("a folder of its own");
        assert!(Path::new(&files[0].1).starts_with(&dir));
        let info = read_session(&files[0].1).expect("the lines read as a transcript");
        assert!(info["turns"].as_u64().unwrap() >= 1);
        assert_eq!(rec["cliSessionId"], cid);
        // cursor_outgoing already ran above: its folder is the only thing it made
        let own = format!("{}/{}.jsonl", project_dir(rec["cwd"].as_str().unwrap_or("")), cid);
        let had = before_own;
        assert_eq!(Path::new(&own).exists(), had, "a send wrote nothing into ~/.claude/projects");
        let _ = fs::remove_dir_all(&dir);
        eprintln!("ok: {} turns, {} bytes", info["turns"], files[0].0.size);
    }

    #[test]
    fn no_way_out_of_a_transfer_leaves_it_looking_live() {
        let run = |state: &str, res: Result<Value, String>| {
            let mut inc = Some(json!({ "state": state }));
            settle(&mut inc, &res);
            inc.unwrap()
        };
        let e = run("pairing", Err("the offer has a transcript id that isn't safe to use".into()));
        assert_eq!(e["state"], "error");
        assert!(e["msg"].as_str().unwrap().contains("isn't safe"));
        assert_eq!(run("asking", Ok(json!({})))["state"], "cancelled");
        assert_eq!(run("receiving", Err("the connection closed".into()))["state"], "error");
        for done in ["done", "declined", "expired", "cancelled", "error"] {
            assert_eq!(run(done, Err("x".into()))["state"], done, "{done} is left as it was");
        }
    }

    /// A receiver's account with one chat already in it, filed under `cwd`.
    fn known_chat(recv_root: &str, sid: &str, main_id: &str, cwd: &str, body: &str, extra: Value) -> String {
        let scope = format!("{recv_root}/sess/acct-b/org-b");
        fs::create_dir_all(&scope).unwrap();
        let d = format!("{recv_root}/proj/{}", enc_cwd(cwd));
        fs::create_dir_all(&d).unwrap();
        fs::write(format!("{d}/{main_id}.jsonl"), body).unwrap();
        let mut r = json!({ "sessionId": sid, "cliSessionId": main_id, "cwd": cwd, "originCwd": cwd,
                            "title": "Renamed by receiver", "completedTurns": 1 });
        if let (Some(o), Some(x)) = (r.as_object_mut(), extra.as_object()) { for (k, v) in x { o.insert(k.clone(), v.clone()); } }
        let p = format!("{scope}/{sid}.json");
        fs::write(&p, r.to_string()).unwrap();
        p
    }

    fn user_line(cwd: &str, text: &str) -> String {
        json!({ "type": "user", "cwd": cwd, "timestamp": "2026-09-20T10:00:00.000Z",
                "message": { "role": "user", "content": text } }).to_string() + "\n"
    }

    #[test]
    fn a_known_chat_is_updated_where_it_lives() {
        let send_root = tmp("send-known");
        let recv_root = tmp("recv-known");
        let id = "12121212-3434-4565-8787-909090909090";
        let mine = "/Users/b/work/app";             // where the receiver filed it
        let first = user_line(mine, "first question");
        let rp = known_chat(&recv_root, "local_k1", id, mine, &first, json!({}));
        // the sender continued it, in a folder of its own
        let theirs = "/Users/a/elsewhere";
        let longer = first.clone() + &user_line(theirs, "second question");
        let main = format!("{send_root}/{id}.jsonl");
        fs::write(&main, &longer).unwrap();
        let files = vec![(FileSpec { kind: Kind::Main, id: id.into(), name: String::new(), size: longer.len() as u64 }, main.clone())];
        let rec = json!({ "sessionId": "local_k1", "cliSessionId": id, "cwd": theirs, "title": "Sender title" });
        let (sent, got) = transfer(&recv_root, &rec, files.clone(), None);
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        assert_eq!(got["cwd"], mine);
        let at = |c: &str| format!("{recv_root}/proj/{}/{id}.jsonl", enc_cwd(c));
        assert_eq!(fs::read_to_string(at(mine)).unwrap(), longer, "the new turns are where the record points");
        assert!(!Path::new(&at(theirs)).exists(), "no second copy");
        let r: Value = serde_json::from_str(&fs::read_to_string(&rp).unwrap()).unwrap();
        assert_eq!(r["title"], "Renamed by receiver", "the receiver's rename survives");
        assert_eq!(r["cwd"], mine);

        // a different conversation claiming that id, from yet another folder, is refused
        let other = user_line("/Users/a/third", "something else entirely");
        fs::write(&main, &other).unwrap();
        let files = vec![(FileSpec { kind: Kind::Main, id: id.into(), name: String::new(), size: other.len() as u64 }, main)];
        let rec = json!({ "sessionId": "local_k1", "cliSessionId": id, "cwd": "/Users/a/third" });
        let (sent, _) = transfer(&recv_root, &rec, files, None);
        assert!(sent.unwrap_err().contains("different conversation"));
        assert_eq!(fs::read_to_string(at(mine)).unwrap(), longer, "untouched");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn re_receiving_keeps_the_receivers_own_record() {
        let send_root = tmp("send-keep");
        let recv_root = tmp("recv-keep");
        let (r1, r2) = ("r1r1r1r1-0000-4000-8000-000000000001", "r2r2r2r2-0000-4000-8000-000000000002");
        let cwd = "/Users/b/app";
        let body = user_line(cwd, "hello");
        let rp = known_chat(&recv_root, "local_c", r2, cwd, "",
                            json!({ "bridgeSessionIds": [r1], "completedTurns": 9, "lastActivityAt": 1799999999999u64 }));
        fs::write(format!("{recv_root}/proj/{}/{r1}.jsonl", enc_cwd(cwd)), &body).unwrap();
        let before: Value = serde_json::from_str(&fs::read_to_string(&rp).unwrap()).unwrap();
        let main = format!("{send_root}/{r1}.jsonl");
        fs::write(&main, &body).unwrap();
        let files = vec![(FileSpec { kind: Kind::Main, id: r1.into(), name: String::new(), size: body.len() as u64 }, main)];
        let rec = json!({ "sessionId": "local_c", "cliSessionId": r1, "cwd": cwd, "title": "Sender title", "completedTurns": 1 });
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        assert_eq!(sent.unwrap(), "done");
        assert_eq!(got.unwrap()["written"], 0);
        let after: Value = serde_json::from_str(&fs::read_to_string(&rp).unwrap()).unwrap();
        assert_eq!(after, before, "its own session, bridge ids, title and turns are all still there");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_deleted_chats_id_is_not_the_senders_to_reuse() {
        let send_root = tmp("send-tomb");
        let recv_root = tmp("recv-tomb");
        let scope = format!("{recv_root}/sess/acct-b/org-b");
        fs::create_dir_all(&scope).unwrap();
        fs::write(format!("{scope}/deleted_victim"), "1789900000000").unwrap();
        let id = "56565656-0000-4000-8000-000000000005";
        let (_, files) = one_file(&send_root, id, "/Users/a/p");
        let rec = json!({ "sessionId": "local_victim", "cliSessionId": id, "cwd": "/Users/a/p" });
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        assert_eq!(sent.unwrap(), "done");
        let got = got.unwrap();
        assert!(got["wrote"].as_str().unwrap().ends_with(&format!("local_{id}.json")), "it got an id of its own");
        assert!(Path::new(&format!("{scope}/deleted_victim")).exists(), "the deleted chat stays deleted");
        assert!(!Path::new(&format!("{scope}/local_victim.json")).exists());
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_record_write_takes_the_transcripts_back() {
        use std::os::unix::fs::PermissionsExt;
        let send_root = tmp("send-rollback");
        let recv_root = tmp("recv-rollback");
        let scope = format!("{recv_root}/sess/acct-b/org-b");
        fs::create_dir_all(&scope).unwrap();
        fs::set_permissions(&scope, fs::Permissions::from_mode(0o555)).unwrap();   // the record can't be written
        let id = "78787878-0000-4000-8000-000000000006";
        let (_, files) = one_file(&send_root, id, "/Users/a/p");
        let rec = json!({ "sessionId": format!("local_{id}"), "cliSessionId": id, "cwd": "/Users/a/p" });
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        fs::set_permissions(&scope, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(sent.is_err() && got.is_err());
        assert!(!Path::new(&format!("{recv_root}/proj/{}/{id}.jsonl", enc_cwd("/Users/a/p"))).exists(),
                "the transcript went back out with the failed record");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_known_chat_with_no_folder_is_given_one() {
        let send_root = tmp("send-nofolder");
        let recv_root = tmp("recv-nofolder");
        let id = "9a9a9a9a-0000-4000-8000-000000000009";
        let scope = format!("{recv_root}/sess/acct-b/org-b");
        fs::create_dir_all(&scope).unwrap();
        let rp = format!("{scope}/local_nf.json");
        fs::write(&rp, json!({ "sessionId": "local_nf", "cliSessionId": id, "cwd": "", "title": "Mine" }).to_string()).unwrap();
        let (_, files) = one_file(&send_root, id, "/Users/a/landing");
        let rec = json!({ "sessionId": "local_nf", "cliSessionId": id, "cwd": "/Users/a/landing" });
        let (sent, got) = transfer(&recv_root, &rec, files, None);
        assert_eq!(sent.unwrap(), "done");
        assert_eq!(got.unwrap()["cwd"], "/Users/a/landing");
        let r: Value = serde_json::from_str(&fs::read_to_string(&rp).unwrap()).unwrap();
        assert_eq!(r["cwd"], "/Users/a/landing", "the record now points where the transcript went");
        assert_eq!(r["title"], "Mine");
        assert!(Path::new(&format!("{recv_root}/proj/{}/{id}.jsonl", enc_cwd("/Users/a/landing"))).exists());
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_newer_rendering_of_a_cursor_chat_replaces_the_older() {
        let send_root = tmp("send-rerender");
        let recv_root = tmp("recv-rerender");
        let cid = "0d0d0d0d-1e1e-4f2f-8a3a-505050505050";
        let cwd = "/Users/ali/code/site";
        let m = |r: &str, t: &str, ts: &str| json!({ "role": r, "text": t, "t": ts });
        // sent while the chat ended on a tool run, then again once it went on
        let before = transcript_lines(cid, cwd, &[m("user", "fix the bug", "2026-09-20T09:00:00.000Z"),
                                                 m("assistant", "(read_file main.rs)", "2026-09-20T09:00:00.000Z")]);
        let after = transcript_lines(cid, cwd, &[m("user", "fix the bug", "2026-09-20T09:00:00.000Z"),
                                                m("assistant", "(read_file main.rs)\n\nFixed.", "2026-09-20T09:00:09.000Z"),
                                                m("user", "thanks", "2026-09-20T09:01:00.000Z")]);
        let send = |body: &str| {
            let main = format!("{send_root}/{cid}.jsonl");
            fs::write(&main, body).unwrap();
            let files = vec![(FileSpec { kind: Kind::Main, id: cid.into(), name: String::new(), size: body.len() as u64 }, main)];
            transfer(&recv_root, &json!({ "sessionId": null, "cliSessionId": cid, "cwd": cwd, "title": "" }), files, None)
        };
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        assert_eq!(send(&before).0.unwrap(), "done");
        let (sent, got) = send(&after);
        assert_eq!(sent.unwrap(), "done", "not refused as a different conversation");
        assert_eq!(got.unwrap()["written"], 1);
        assert_eq!(fs::read_to_string(format!("{recv_root}/proj/{}/{cid}.jsonl", enc_cwd(cwd))).unwrap(), after);
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn a_zero_date_is_no_date() {
        let send_root = tmp("send-zero");
        let recv_root = tmp("recv-zero");
        fs::create_dir_all(format!("{recv_root}/sess/acct-b/org-b")).unwrap();
        let id = "0e0e0e0e-0000-4000-8000-000000000010";
        let (_, files) = one_file(&send_root, id, "/Users/a/p");
        let rec = json!({ "sessionId": null, "cliSessionId": id, "cwd": "/Users/a/p", "lastActivityAt": 0, "createdAt": 0 });
        let (_, got) = transfer(&recv_root, &rec, files, None);
        let r: Value = serde_json::from_str(&fs::read_to_string(got.unwrap()["wrote"].as_str().unwrap()).unwrap()).unwrap();
        assert!(r["lastActivityAt"].as_u64().unwrap() > 1_000_000_000_000, "the transcript's own time, not 1970");
        for d in [send_root, recv_root] { let _ = fs::remove_dir_all(d); }
    }

    #[test]
    fn stale_staging_is_swept_and_fresh_staging_kept() {
        let v = tmp("sweep");
        fs::create_dir_all(format!("{v}/outgoing/aaa")).unwrap();
        fs::create_dir_all(format!("{v}/incoming/bbb")).unwrap();
        sweep_in(&v, Duration::from_secs(3600));
        assert!(Path::new(&format!("{v}/outgoing/aaa")).exists(), "a transfer that may be live is left alone");
        sweep_in(&v, Duration::ZERO);
        assert!(!Path::new(&format!("{v}/outgoing/aaa")).exists() && !Path::new(&format!("{v}/incoming/bbb")).exists());
        let _ = fs::remove_dir_all(v);
    }
}
