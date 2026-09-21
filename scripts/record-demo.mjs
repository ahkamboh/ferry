/* Record the README and website demo: every feature, once, on example data.
 *
 * scrolltape (github.com/ahkamboh/scrolltape) draws the cursor and renders the
 * video; its automatic site tour can't drive this app, though — Ferry has no
 * <h1> to wait on, `body{overflow:hidden}` so window scrolling is a no-op, and
 * three independently scrolling columns that the tour engine never touches. So
 * the cursor injector and the ffmpeg pipeline are reused here and the path is
 * scripted instead.
 *
 *   BROWSER=true TMPDIR=/tmp python3 ferry-cli.py ui --demo    # in another shell
 *   python3 scripts/narrate.py        # the voice-over, offline with Kokoro (optional)
 *   node scripts/record-demo.mjs
 *
 * With the voice-over in renders/voice, each step is held until its line has
 * been said, the lines play at the moment their step starts, and the same
 * words run as subtitles in a strip under the app, where they can't cover
 * its own toasts. Without it the video is silent, with no subtitles.
 *
 * The demo is the CLI's `ui` against an invented Claude, Cursor and archive
 * tree, so copy, move, add, convert, set folder, rename, download, delete,
 * restore and back up run their real code. Two things a browser can't do are
 * stood in by the demo server and cued from here: the Claude app being open
 * (POST /api/demo/nearby {action:"claude_open"}) and the other computer on
 * the Wi-Fi, "Sam's PC", which answers Nearby like a second Ferry would.
 *
 * SCROLLTAPE=/path/to/scrolltape overrides where scrolltape lives.
 */
import { execFile } from "node:child_process"
import { promisify } from "node:util"
import { mkdir, rm, readdir, rename, readFile, writeFile } from "node:fs/promises"
import { existsSync } from "node:fs"
import path from "node:path"
import os from "node:os"

const execFileAsync = promisify(execFile)
const ST   = process.env.SCROLLTAPE || path.join(os.homedir(), "Documents/GitHub/scrolltape")
const URL_ = process.env.FERRY_URL  || "http://127.0.0.1:7777/"
const OUT  = path.resolve(process.env.OUT_DIR || "renders")
const W = 1600, H = 1000, FPS = 60

const { chromium }         = await import(path.join(ST, "node_modules/playwright/index.mjs"))
const { getCursorMarkup, buildCursorInject } = await import(path.join(ST, "lib/cursors.mjs"))

const sleep = ms => new Promise(r => setTimeout(r, ms))
const ease  = t => (t < 0.5 ? 4 * t ** 3 : 1 - (-2 * t + 2) ** 3 / 2)

// clear only what this script makes: renders/voice belongs to narrate.py
for (const f of ["_raw", "_raw-full.webm", "_subs", "ferry-demo.mp4", "timeline.json"])
  await rm(path.join(OUT, f), { recursive: true, force: true })
await mkdir(path.join(OUT, "_raw"), { recursive: true })
const LINES_JSON = path.join(OUT, "voice", "lines.json")
const LINES = existsSync(LINES_JSON) ? JSON.parse(await readFile(LINES_JSON, "utf8")) : []
const line = id => LINES.find(l => l.id === id)
const SPEED = Number(process.env.SPEED || 1.2)   // playback runs this much faster than the recording

const browser = await chromium.launch()
const ctx = await browser.newContext({
  viewport: { width: W, height: H },
  colorScheme: "dark",                       // scrolltape forces light; Ferry is a dark app
  recordVideo: { dir: path.join(OUT, "_raw"), size: { width: W, height: H } },
})
const cursor = await getCursorMarkup({ cursor: process.env.CURSOR || "mac-hand" })
const page = await ctx.newPage()
const T0 = Date.now()                            // the recording starts with the page

let at = { x: W * 0.5, y: H * 0.62 }
const put = (x, y, down = false) =>
  page.evaluate(([x, y, d]) => window.__stMoveCursor?.(x, y, d), [x, y, down])

async function glide(to, ms = 900) {
  const steps = Math.max(18, Math.round((ms / 1000) * FPS)), from = at
  for (let i = 0; i <= steps; i++) {
    const e = ease(i / steps)
    const x = from.x + (to.x - from.x) * e, y = from.y + (to.y - from.y) * e
    await page.mouse.move(x, y)
    await put(x, y)
    await sleep(ms / steps)
  }
  at = to
}

const centre = async sel => {
  const el = page.locator(sel).first()
  await el.waitFor({ state: "visible", timeout: 15000 })
  const b = await el.boundingBox()
  if (!b) throw new Error(`no box for ${sel}`)
  return { x: Math.round(b.x + b.width / 2), y: Math.round(b.y + b.height / 2) }
}

async function tap(sel, { ms = 800, after = 600 } = {}) {
  await glide(await centre(sel), ms)
  await put(at.x, at.y, true)
  await page.mouse.down(); await sleep(90); await page.mouse.up()
  await put(at.x, at.y, false)
  await sleep(after)
}

/* HTML5 drag and drop, the way a hand does it: press, travel, let go. A
   recorded headless browser draws no drag image, so the script draws one: a
   copy of the row that rides under the hand. */
async function drag(fromSel, toSel) {
  await glide(await centre(fromSel), 800)
  await page.locator(fromSel).first().evaluate(src => {
    const b = src.getBoundingClientRect()
    const g = src.cloneNode(true); g.id = "__ghost"
    g.style.cssText = `position:fixed;left:0;top:0;width:${b.width}px;z-index:2147483645;pointer-events:none;` +
      "opacity:.9;background:var(--raise,#2b2a27);border-radius:8px;box-shadow:0 10px 30px rgba(0,0,0,.45);" +
      "transform:translate(-9999px,0)"
    g.dataset.w = b.width; document.documentElement.appendChild(g)
  })
  const ghost = (x, y) => page.evaluate(([x, y]) => {
    const g = document.getElementById("__ghost")
    if (g) g.style.transform = `translate(${x - g.dataset.w / 2}px,${y - 14}px) rotate(-2deg)`
  }, [x, y])
  await put(at.x, at.y, true); await page.mouse.down(); await ghost(at.x, at.y); await sleep(200)
  const to = await centre(toSel), from = at, steps = 45
  for (let i = 1; i <= steps; i++) {
    const e = ease(i / steps)
    const x = from.x + (to.x - from.x) * e, y = from.y + (to.y - from.y) * e
    await page.mouse.move(x, y); await put(x, y, true); await ghost(x, y); await sleep(24)
  }
  at = to; await sleep(450)
  await page.evaluate(() => document.getElementById("__ghost")?.remove())
  await page.mouse.up(); await put(at.x, at.y, false)
}

/* A pulsing ring around something small the viewer should look at. */
async function spot(sel, ms = 1800) {
  await page.locator(sel).first().evaluate((el, ms) => {
    const b = el.getBoundingClientRect(), r = document.createElement("div")
    r.style.cssText = `position:fixed;left:${b.left - 7}px;top:${b.top - 7}px;width:${b.width + 14}px;` +
      `height:${b.height + 14}px;border:2.5px solid #d97757;border-radius:10px;z-index:2147483644;` +
      "pointer-events:none;animation:__pulse .9s ease-in-out infinite"
    if (!document.getElementById("__pulsekf")) {
      const st = document.createElement("style"); st.id = "__pulsekf"
      st.textContent = "@keyframes __pulse{0%,100%{opacity:1;transform:scale(1)}50%{opacity:.35;transform:scale(1.08)}}"
      document.head.appendChild(st)
    }
    document.documentElement.appendChild(r); setTimeout(() => r.remove(), ms)
  }, ms)
}

/* Press keys and show them, since a video can't show a key being pressed. */
async function keys(combo, label) {
  await page.evaluate(t => {
    const k = document.createElement("div")
    k.textContent = t
    k.style.cssText = "position:fixed;right:28px;bottom:28px;z-index:2147483646;" +
      "pointer-events:none;padding:7px 14px;border-radius:9px;font:600 15px/1 -apple-system,system-ui,sans-serif;" +
      "color:#f4f1ea;background:rgba(31,30,29,.92);border:1px solid rgba(255,255,255,.18);" +
      "box-shadow:0 2px 0 rgba(255,255,255,.12) inset,0 8px 24px rgba(0,0,0,.4)"
    document.documentElement.appendChild(k); setTimeout(() => k.remove(), 1100)
  }, label)
  await sleep(250); await page.keyboard.press(combo)
}

/* Park the hand somewhere neutral once a click has done its job, so it
   isn't left resting on Cancel while a card moves under it. */
const rest = (ms = 600) => glide({ x: W * 0.78, y: H * 0.86 }, ms)

async function typeInto(sel, text, { select = true, commit = null } = {}) {
  await tap(sel, { after: 250 })
  if (select) { await page.keyboard.press("Meta+A"); await sleep(150) }
  await page.keyboard.type(text, { delay: 55 })
  await sleep(400)
  if (commit) await page.keyboard.press(commit)
}

/* The three columns scroll themselves, so animate the pane's own scrollTop. */
async function scrollPane(sel, to, ms) {
  const b = await page.locator(sel).boundingBox()
  if (b && !(at.x > b.x && at.x < b.x + b.width && at.y > b.y && at.y < b.y + b.height))
    await glide({ x: Math.round(b.x + b.width * 0.72), y: Math.round(b.y + b.height * 0.55) }, 500)
  await page.evaluate(async ([sel, to, ms]) => {
    const el = document.querySelector(sel), from = el.scrollTop
    const end = to === "end" ? el.scrollHeight - el.clientHeight : to
    const t0 = performance.now()
    await new Promise(done => {
      const step = now => {
        const t = Math.min(1, (now - t0) / ms)
        const e = t < 0.5 ? 4 * t ** 3 : 1 - (-2 * t + 2) ** 3 / 2
        el.scrollTop = from + (end - from) * e
        t < 1 ? requestAnimationFrame(step) : done()
      }
      requestAnimationFrame(step)
    })
  }, [sel, to, ms])
}

/* A step starts: the previous one is first held until its line has been said
   (plus a breath), measured in playback time, then this one's start is noted
   for the voice track and the subtitles. */
const TIMELINE = []
async function holdLine() {
  const prev = TIMELINE[TIMELINE.length - 1], l = prev && line(prev.id)
  if (!l) return
  const need = (l.dur + 0.55) * SPEED * 1000, spent = Date.now() - prev.at
  if (spent < need) await sleep(need - spent)
}
async function chapter(id) {
  await holdLine()
  TIMELINE.push({ id, at: Date.now() })
  console.log(`${((Date.now() - T0) / 1000).toFixed(1).padStart(6)}s  ${id}`)
  await sleep(300)
}

const demo = action => page.evaluate(a =>
  fetch("/api/demo/nearby", { method: "POST", body: JSON.stringify({ action: a }) }).then(r => r.json()), action)
const cardClosed = (timeout = 30000) =>
  page.waitForFunction(() => !document.getElementById("ask").className, null, { timeout })
const row  = t => `.ch:has(.t:text-is("${t}"))`
const rowL = t => `.ch:has(.t:has-text("${t}"))`
const acct = k => `.acct[data-k^="${k}"]`
const pickItem = k => `#pmenu .mi[data-k^="${k}"]`
const YOU = "a1c4f2e8", OLD = "b7d9e3a1", SIDE = "c9d8e7f6"

await page.goto(URL_, { waitUntil: "networkidle" })
await page.waitForSelector(".acct", { state: "visible" })
// after load, like scrolltape does — the injector needs document.body to exist
await page.evaluate(buildCursorInject(cursor, { safe: false }))
// the hand sits on <html>, so the demo's zoom (which scales <body>) can't move it off-frame
await page.evaluate(() => document.documentElement.appendChild(document.getElementById("st-cursor")))
const ROOT = await page.evaluate(() => S.exportDir.replace(/\/Downloads$/, ""))
// the browser's own dialogs stand in for the app's native panels, and a video
// can't see them: answer Set folder's with a folder that exists, confirm Delete
page.on("dialog", d => d.type() === "prompt" ? d.accept(ROOT + "/code/brand-site") : d.accept())
await page.mouse.move(at.x, at.y)
await put(at.x, at.y)
await sleep(1200)

// 1. every account on this machine, signed-out ones included, and the chats no account lists
await chapter("accounts")
await glide(await centre(acct(YOU)), 1000); await sleep(600)
await glide(await centre(acct(OLD)), 600); await sleep(400)
await glide(await centre(acct(SIDE)), 500); await sleep(500)
await glide(await centre(acct("source:cli")), 700); await sleep(300)
await glide(await centre(acct("source:cursor")), 800); await sleep(900)

// 2. a signed-out account's chats are still there: read one
await chapter("read")
await tap(acct(OLD), { after: 900 })
await tap(row("Stripe webhook retries"), { after: 1300 })
await scrollPane("#read", 620, 2200); await sleep(1100)
await scrollPane("#read", 1240, 1900); await sleep(1000)
await scrollPane("#read", 0, 1200);   await sleep(500)

// 3. move it into the account you're signed into
await chapter("move")
await tap("#pick", { after: 800 })
await tap(pickItem(YOU), { after: 800 })
await tap('.seg button:text-is("Move")', { ms: 700, after: 400 })
await page.waitForSelector("#ask.on")
await sleep(500)
await tap("#ago", { ms: 700, after: 150 })
await rest(); await cardClosed(); await sleep(900)

// 4. or drag one onto an account
await chapter("drag")
await drag(row("Flaky integration tests"), acct(SIDE))
await page.waitForSelector("#ask.on #ac")
await sleep(700)
await tap("#ac", { ms: 600, after: 150 })
await rest(); await cardClosed(); await sleep(800)

// 5. name an account that has only an id
await chapter("nickname")
await tap(acct(SIDE), { after: 700 })
await typeInto("#nick", "side project", { commit: "Tab" })
await sleep(1300)

// 6. the account you use now: search, a pruned chat, rename
await chapter("search")
await tap(acct(YOU), { after: 700 })
await typeInto("#q", "post", { select: false })
await sleep(900)
await tap(row("Postgres index tuning"), { after: 1200 })
await tap("#q", { after: 150 })
await page.keyboard.press("Meta+A"); await page.keyboard.press("Backspace"); await sleep(600)
await chapter("pruned")
await tap(row("Kubernetes rollout debugging"), { after: 1500 })
await chapter("rename")
await tap(row("Refactor auth middleware"), { after: 900 })
await typeInto("#ttl", "Auth middleware: rotate JWT keys", { commit: "Tab" })
await sleep(1400)

// 7. a chat started without a folder
await chapter("folder")
await tap(row("Landing page hero copy"), { after: 900 })
await spot(".fol.none", 1600); await sleep(900)
await glide(await centre(".fol.none"), 700); await sleep(500)
await tap(".fol.none", { ms: 300, after: 300 })
await page.waitForSelector(".fol:not(.none)")
await sleep(1600)

// 8. download
await chapter("download")
await tap(row("Postgres index tuning"), { after: 800 })
await glide(await centre("#fmt"), 700)
await spot("#fmt", 3600)
await page.selectOption("#fmt", "json"); await sleep(1300)
await page.selectOption("#fmt", "txt");  await sleep(1300)
await page.selectOption("#fmt", "md");   await sleep(900)
await tap('.fops .btn:text-is("Download")', { ms: 500, after: 2200 })

// 9. delete, and bring back one Claude deleted, from the archive
await chapter("restore")
await tap(row("Migrate build to esbuild"), { after: 1400 })
await glide(await centre(".fops .btn.d"), 700); await sleep(600)
await tap(".fops .btn.d", { ms: 200, after: 1600 })
await glide(await centre('.ch.dead:has-text("Rewrite the billing cron")'), 800); await sleep(700)
await tap('.ch.dead:has-text("Rewrite the billing cron") .btn', { ms: 400, after: 1000 })
await glide(await centre(row("Rewrite the billing cron")), 700)
await sleep(1500)

// 10. chats the CLI and VS Code wrote, that no account lists
await chapter("cli")
await tap(acct("source:cli"), { after: 700 })
await tap(rowL("Add zsh and fish"), { after: 1200 })
await scrollPane("#read", 500, 1400); await sleep(700)
await scrollPane("#read", 0, 900)
await chapter("cliadd")
await tap("#pick", { after: 700 })
await tap(pickItem(YOU), { after: 700 })
await tap('.sendg .btn.p:text-is("Add to account")', { after: 400 })
await page.waitForSelector("#ask.on"); await sleep(600)
await tap("#ago", { ms: 600, after: 150 })
await rest(); await cardClosed(); await sleep(1100)
await glide(await centre(acct("source:vscode")), 700); await sleep(900)

// 11. Cursor chats, into Claude
await chapter("cursor")
await tap(acct("source:cursor"), { after: 700 })
await tap(row("Debounce the search box"), { after: 1200 })
await scrollPane("#read", 700, 1800); await sleep(900)
await scrollPane("#read", 0, 1000)
await chapter("cursoradd")
await tap("#pick", { after: 700 })
await tap(pickItem(YOU), { after: 700 })
await tap('.sendg .btn.p:text-is("Add to account")', { after: 400 })
await page.waitForSelector("#ask.on"); await sleep(600)
await tap("#ago", { ms: 600, after: 150 })
await rest(); await cardClosed(); await sleep(1100)

// 12. and a Claude chat into Cursor
await tap(row("Rust CLI argument parsing"), { after: 800 })
await chapter("convert")
await tap("#pick", { after: 700 })
await tap(pickItem("source:cursor"), { after: 700 })
await tap('.sendg .btn.p:text-is("Convert")', { after: 400 })
await page.waitForSelector("#ask.on"); await sleep(600)
await tap("#ago", { ms: 600, after: 150 })
await rest(); await cardClosed(); await sleep(1100)

// 13. back up
await chapter("backup")
await tap('#bar .btn:text-is("Back up")', { after: 2600 })

// 14. while Claude is open, nothing is written
await chapter("locked")
await demo("claude_open")
await page.evaluate(() => document.activeElement?.blur())
await keys("F5", "F5  re-read"); await sleep(900)
await glide(await centre("#lock .appst:first-child"), 700)
await spot("#lock .appst:first-child", 2200); await sleep(2300)
await tap(row("Postgres index tuning"), { after: 700 })
await tap("#pick", { after: 600 })                // the last target was Cursor
await tap(pickItem(OLD), { after: 600 })
await tap('.seg button:text-is("Copy")', { ms: 600, after: 300 })
await page.waitForSelector("#ask.on"); await rest(500)
await chapter("quit"); await sleep(1800)
await keys("Escape", "esc  cancel"); await sleep(700)
await demo("claude_quit")
await page.evaluate(() => document.activeElement?.blur())
await keys("F5", "F5  re-read"); await sleep(600)
await spot("#lock .appst:first-child", 1500); await sleep(1600)

// 15. Nearby: send a chat to another computer on the same Wi-Fi
await chapter("nearby")
await tap("#nb", { after: 1000 })
await tap("#nbtog", { after: 2200 })
// the menu again, now with Sam's PC in it
if (!(await page.locator("#pmenu").isVisible())) await tap("#nb", { after: 300 })
await page.waitForSelector("#pmenu .nbsec", { state: "visible" }).catch(() => {})
await sleep(3400)
await page.keyboard.press("Escape"); await sleep(400)
await tap(row("Auth middleware: rotate JWT keys"), { after: 900 })
await tap("#pick", { after: 1300 })
await tap('#pmenu .mi[data-k^="nb:"]', { after: 700 })
await chapter("code")
await tap('.sendg .btn.p:text-is("Send")', { after: 300 })
await page.waitForSelector("#nbyes", { state: "visible" })
await sleep(2200)                                 // both screens show the same six digits
await tap("#nbyes", { ms: 600, after: 0 })
await rest(250); await cardClosed(); await sleep(1000)

// 16. and receive one
await chapter("receive")
await demo("incoming")
await page.waitForSelector("#nbacct", { state: "visible", timeout: 20000 })
await rest(500); await sleep(1600)
await tap("#nbpick", { after: 900 })
await tap("#ago", { ms: 600, after: 150 })
await rest(); await cardClosed(); await sleep(2000)

// 17. zoom, then done
await chapter("zoom")
await glide({ x: 820, y: 470 }, 600)
await keys("Meta+Equal", "⌘ +"); await sleep(1000)
await keys("Meta+Equal", "⌘ +"); await sleep(1300)
await keys("Meta+Minus", "⌘ −"); await sleep(1000)
await keys("Meta+0", "⌘ 0");     await sleep(1100)
await chapter("end")
await glide(await centre(".xbtn"), 700)
await keys("Meta+W", "⌘ W  close"); await sleep(500)
await glide({ x: 1080, y: 520 }, 700); await sleep(500)
await holdLine(); await sleep(1400)

// the strip under the app takes the page's own background, so it reads as part of the frame
const BG = await page.evaluate(() => getComputedStyle(document.body).backgroundColor)
const hex = BG.match(/\d+/g).slice(0, 3).map(n => (+n).toString(16).padStart(2, "0")).join("")
await page.close(); await ctx.close()

const raw = (await readdir(path.join(OUT, "_raw"))).find(f => f.endsWith(".webm"))
await rename(path.join(OUT, "_raw", raw), path.join(OUT, "_raw-full.webm"))
const src = path.join(OUT, "_raw-full.webm")

// Where each step lands in the finished video: the first TRIM seconds (the
// page loading) are cut, and playback runs SPEED times faster than recording.
const TRIM = 1.0
const final = at => Math.max(0, ((at - T0) / 1000 - TRIM) / SPEED)
const spoken = TIMELINE.map((c, i) => {
  const l = line(c.id), start = final(c.at) + 0.15
  const next = TIMELINE[i + 1] ? final(TIMELINE[i + 1].at) - 0.05 : Infinity
  return l && { ...l, start, end: Math.min(start + l.dur + 0.9, next) }
}).filter(Boolean)
await writeFile(path.join(OUT, "timeline.json"), JSON.stringify(spoken, null, 1))

// one image per subtitle, drawn by the browser, since this ffmpeg has no text filter
const SUB_H = spoken.length ? 84 : 0
const subs = []
if (spoken.length) {
  await mkdir(path.join(OUT, "_subs"), { recursive: true })
  const sctx = await browser.newContext({ viewport: { width: W, height: SUB_H } })
  const sp = await sctx.newPage()
  for (const l of spoken) {
    await sp.setContent(`<body style="margin:0;height:${SUB_H}px;display:grid;place-items:center;background:#${hex}">
      <div style="max-width:1400px;text-align:center;color:#f1eee7;font:500 25px/1.3 -apple-system,system-ui,sans-serif;
        letter-spacing:-.005em">${l.text.replace(/&/g, "&amp;").replace(/</g, "&lt;")}</div></body>`)
    const f = path.join(OUT, "_subs", `${l.id}.png`)
    await sp.screenshot({ path: f }); subs.push({ ...l, png: f })
  }
  await sctx.close()
}
await browser.close()

// One pass: the recording sped up, the strip under it, each subtitle in its
// window, each spoken line at its step's start, and faststart for the website.
const args = ["-y", "-ss", String(TRIM), "-i", src]
for (const s of subs) args.push("-i", s.png)
for (const s of subs) args.push("-i", s.file)
const fx = [`[0:v]setpts=PTS/${SPEED},fps=25,pad=${W}:${H + SUB_H}:0:0:color=0x${hex}[v0]`]
subs.forEach((s, k) => fx.push(
  `[v${k}][${1 + k}:v]overlay=0:${H}:enable='between(t,${s.start.toFixed(2)},${s.end.toFixed(2)})'[v${k + 1}]`))
subs.forEach((s, k) => fx.push(
  `[${1 + subs.length + k}:a]adelay=delays=${Math.round(s.start * 1000)}:all=1,aresample=48000[a${k}]`))
if (subs.length) fx.push(subs.map((_, k) => `[a${k}]`).join("") +
  `amix=inputs=${subs.length}:normalize=0:dropout_transition=0,apad[aout]`)
args.push("-filter_complex", fx.join(";"), "-map", `[v${subs.length}]`)
if (subs.length) args.push("-map", "[aout]", "-c:a", "aac", "-b:a", "128k", "-shortest")
else args.push("-an")
args.push("-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", process.env.CRF || "24", "-preset", "slow",
  "-movflags", "+faststart", path.join(OUT, "ferry-demo.mp4"))
await execFileAsync("ffmpeg", args, { maxBuffer: 1 << 26 })
console.log("wrote", path.join(OUT, "ferry-demo.mp4"))
