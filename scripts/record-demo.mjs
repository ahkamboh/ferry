/* Record the README demo.
 *
 * scrolltape (github.com/ahkamboh/scrolltape) draws the cursor and renders the
 * video; its automatic site tour can't drive this app, though — Ferry has no
 * <h1> to wait on, `body{overflow:hidden}` so window scrolling is a no-op, and
 * three independently scrolling columns that the tour engine never touches. So
 * the cursor injector and the ffmpeg pipeline are reused here and the path is
 * scripted instead.
 *
 *   python3 ferry-cli.py ui --demo          # in another shell
 *   node scripts/record-demo.mjs
 *
 * SCROLLTAPE=/path/to/scrolltape overrides where scrolltape lives.
 */
import { execFile } from "node:child_process"
import { promisify } from "node:util"
import { mkdir, rm, readdir, rename } from "node:fs/promises"
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

await rm(OUT, { recursive: true, force: true })
await mkdir(path.join(OUT, "_raw"), { recursive: true })

const browser = await chromium.launch()
const ctx = await browser.newContext({
  viewport: { width: W, height: H },
  colorScheme: "dark",                       // scrolltape forces light; Ferry is a dark app
  recordVideo: { dir: path.join(OUT, "_raw"), size: { width: W, height: H } },
})
const cursor = await getCursorMarkup({ cursor: process.env.CURSOR || "mac-hand" })
const page = await ctx.newPage()

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
  const b = await page.locator(sel).first().boundingBox()
  if (!b) throw new Error(`no box for ${sel}`)
  return { x: Math.round(b.x + b.width / 2), y: Math.round(b.y + b.height / 2) }
}

async function tap(sel, { ms = 900, after = 700 } = {}) {
  await glide(await centre(sel), ms)
  await put(at.x, at.y, true)
  await page.mouse.down(); await sleep(90); await page.mouse.up()
  await put(at.x, at.y, false)
  await sleep(after)
}

/* The three columns scroll themselves, so animate the pane's own scrollTop. */
async function scrollPane(sel, to, ms) {
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

const row = t => `.ch:has(.t:text-is("${t}"))`
const acct = t => `.acct:has-text("${t}")`

await page.goto(URL_, { waitUntil: "networkidle" })
await page.waitForSelector(".acct", { state: "visible" })
// after load, like scrolltape does — the injector needs document.body to exist
await page.evaluate(buildCursorInject(cursor, { safe: false }))
await page.mouse.move(at.x, at.y)
await put(at.x, at.y)
await sleep(1500)

// 1. three accounts on this machine — one of them signed out
await glide({ x: 150, y: 200 }, 1100); await sleep(700)
await glide({ x: 150, y: 300 }, 700);  await sleep(900)

// 2. open the account you are NOT signed into
await tap(acct("old work account"), { after: 1200 })

// 3. its chats are still here — read one
await tap(row("Stripe webhook retries"), { after: 1500 })

// 4. the conversation renders: tables, lists, a quote, code
await scrollPane("#read", 620, 2600); await sleep(1300)
await scrollPane("#read", 1240, 2200); await sleep(1300)
await scrollPane("#read", 0, 1400);   await sleep(700)

// 5. choose where it goes
await tap("#pick", { after: 900 })
await tap('#pmenu .mi:has-text("you")', { after: 1000 })

// 6. move it into the account you are signed into
await tap('.seg button:text-is("Move")', { ms: 700, after: 500 })
await page.waitForSelector("#ask.on")
await tap("#ago", { ms: 700, after: 300 })
// the card is display:none when closed, so wait on the class, not visibility
await page.waitForFunction(() => !document.getElementById("ask").className, null, { timeout: 25000 })
await sleep(1400)

// 7. it is in the current account now
await tap(acct("you@example.com"), { after: 1200 })
await glide(await centre(row("Stripe webhook retries")), 800)
await sleep(1800)

// 8. a deleted chat is only a tombstone — the archive still has the record
await tap(".ch.dead .btn", { after: 2400 })
await glide(await centre(row("Rewrite the billing cron")), 900)
await sleep(2200)

await page.close(); await ctx.close(); await browser.close()

const raw = (await readdir(path.join(OUT, "_raw"))).find(f => f.endsWith(".webm"))
await rename(path.join(OUT, "_raw", raw), path.join(OUT, "_raw-full.webm"))
const src = path.join(OUT, "_raw-full.webm")

// same encoder settings scrolltape uses
await execFileAsync("ffmpeg", ["-y", "-ss", "1.2", "-i", src, "-an",
  "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "20", path.join(OUT, "ferry-demo.mp4")])
console.log("wrote", path.join(OUT, "ferry-demo.mp4"))
