<div align="center">

<img src="assets/logo-160.png" width="112" alt="Ferry logo">

<h1>
  Ferry
  <a href="https://github.com/ahkamboh/ferry/releases/latest" title="Download the latest release">
    <img src="assets/download.png" width="24" alt="Download">
  </a>
</h1>

**Copy or move a chat from a Claude Code account you're signed out of into the one you're using now.**

Claude picks up the old context instead of you explaining the project again — all on your machine. A 4 MB native app for macOS and Windows, plus a zero-dependency CLI.

[![Download](https://img.shields.io/badge/%E2%86%93%20Download-macOS%20%7C%20Windows-d97757?style=for-the-badge)](https://github.com/ahkamboh/ferry/releases/latest)
[![Website](https://img.shields.io/badge/Website-ahkamboh.github.io%2Fferry-1f1e1d?style=for-the-badge)](https://ahkamboh.github.io/ferry/)

[![License: MIT](https://img.shields.io/badge/License-MIT-d97757.svg)](LICENSE)
![Platform: macOS and Windows](https://img.shields.io/badge/platform-macOS%2011%2B%20%7C%20Windows%2010%2B-1f1e1d)
![Size: 4 MB](https://img.shields.io/badge/app-4%20MB-1f1e1d)
![Built with Tauri 2](https://img.shields.io/badge/built%20with-Tauri%202%20%2B%20Rust-1f1e1d)

<img src="assets/screenshot.png" width="920" alt="Ferry showing accounts, chats and a rendered conversation">

</div>

---

## The problem

You sign into Claude Code with a second account — a work one, a new subscription, a client's — and your chat history is gone. Not deleted, just invisible: Claude Code keeps chat metadata **per account**, so every conversation you had is still on the disk, attached to an account you're no longer signed into.

So you start over and re-explain everything to the model.

Two things make it worse:

- **Transcripts expire.** Claude Code prunes old transcript files. Chats can outlive their own content — the title is listed, the conversation is gone.
- **Deletes can come back.** The desktop app reconciles its own state. A chat you restore by hand can be removed again later.

Ferry fixes both. It reads the files Claude Code already writes, lets you carry a chat from one account to another, and keeps an archive the app can't touch.

## What it does

| | |
|---|---|
| **See every account** | every Claude account you've signed into on this Mac, with its chats — even ones you're signed out of |
| **Identify them** | email for the account you're signed into; connectors, date range and project folders for the rest. Nickname any account and it sticks |
| **Read any chat** | full conversation with proper Markdown — tables, code blocks, lists, quotes — plus tool calls |
| **Copy or move** | drag a chat onto another account, or use the Copy / Move buttons |
| **Download** | save a whole conversation as Markdown, plain text or JSON, anywhere you choose |
| **Delete and undelete** | deletes are reversible; restore from another account or from the archive |
| **Back up** | one button archives every chat and every transcript, subagent transcripts included |

## Why moving a chat is instant

Claude Code stores your history in two separate layers:

| Layer | Location | Scope |
|---|---|---|
| Chat metadata — title, model, folder, timestamps | `~/Library/Application Support/Claude/claude-code-sessions/<account>/<org>/local_<id>.json` | **per account** |
| Deletion marker | same folder, `deleted_<id>` — a millisecond timestamp | per account |
| Transcripts — the actual conversation | `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` | **shared by every account** |
| Subagent transcripts | `~/.claude/projects/<encoded-cwd>/<sessionId>/subagents/*.jsonl` | shared |

Because transcripts are account-agnostic, moving a chat only moves about **10 KB of JSON**. A 45 MB conversation transfers in the same instant as a 40 KB one, and nothing is duplicated on disk.

## Install

### Download the app

From [Releases](https://github.com/ahkamboh/ferry/releases/latest):

| Platform | File | Notes |
|---|---|---|
| **macOS 11+** | `Ferry-<version>-macos-universal.zip` | universal — Apple Silicon and Intel |
| **Windows 10+** | `Ferry.exe` | single file, no installer, needs the WebView2 runtime |

Neither build is code-signed, so both operating systems will warn you once.

**macOS:** right-click the app → **Open** → **Open**. Or:

```bash
xattr -dr com.apple.quarantine /Applications/Ferry.app
```

**Windows:** SmartScreen shows "Windows protected your PC" → **More info** → **Run anyway**.
If it says **Smart App Control blocked an app** instead, there is no Run anyway: Smart App
Control only allows signed apps. Use the [CLI](#cli)'s `ui` command, which serves the same
interface, or turn Smart App Control off under Windows Security → App & browser control.
Windows 11 already has the WebView2 runtime; on Windows 10 install the
[Evergreen WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
if the window opens blank.

### Or build it yourself

Requires [Rust](https://rustup.rs). `build.sh` is macOS; on Windows run
`cargo build --release --manifest-path src-tauri/Cargo.toml`.

```bash
git clone https://github.com/ahkamboh/ferry.git
cd ferry
./build.sh
open ~/Applications/Ferry.app
```

`build.sh` compiles the Rust binary, bundles `Ferry.app`, and installs it to `~/Applications`.

**Windows installer.** With Node.js as well, this builds both `Ferry.exe` and an installer,
`src-tauri/target/release/bundle/nsis/Ferry_<version>_x64-setup.exe`:

```bash
npx @tauri-apps/cli@2 build --bundles nsis
```

The installer needs no admin rights: it installs Ferry for the current user under
`%LOCALAPPDATA%\Programs`, adds it to the Start menu, registers an uninstaller in
Settings → Apps, and fetches the WebView2 runtime if the machine lacks it. The **Windows
build** workflow in Actions produces both files as well.

## CLI

`ferry-cli.py` uses only the Python 3 standard library — no `pip install`, no virtualenv.

```bash
python3 ferry-cli.py list                     # every account and its chats
python3 ferry-cli.py vault                    # archive everything to ~/.ferry
python3 ferry-cli.py export "auth refactor"   # save a chat, Markdown by default
python3 ferry-cli.py export "auth refactor" json
python3 ferry-cli.py export 1191f0ec txt      # disambiguate by session id
python3 ferry-cli.py ui                       # serve the app UI at localhost:7777
```

`export` matches on chat title or session id. If a title matches more than one chat it lists
the candidates with their ids instead of guessing. Files go to the folder you last downloaded
to, `~/Downloads` until you pick another.

`ui` serves the **same interface as the desktop app** — the app's `dist/index.html` with a shim
that turns its `invoke()` calls into HTTP. Markdown rendering, drag-and-drop and the archive all
work there; the only difference is the browser can't open a native save panel, so downloads go
straight to your chosen folder.

Run `vault` from a launchd job or cron and your history is backed up nightly without opening anything.

## The archive

`Back up` copies every chat record and every transcript into `~/.ferry`.

```
~/.ferry/
  chats/<timestamp>/<account>/   chat records, one snapshot per run
  projects/                      every transcript, subagents included
  snapshots/                     an automatic copy before each change
  labels.json                    your account nicknames
  prefs.json                     last folder you downloaded to
```

The archive is yours, outside anything Claude Code manages. Once a chat is in it, deletion becomes cosmetic — restore it into whichever account you want.

## Safety

- **Writes are refused while the Claude app is running.** Quit Claude first; the title bar tells you when editing is off.
- **Every change is snapshotted** into `~/.ferry/snapshots/` before it happens.
- **Transcripts are never moved or edited.** Only the small metadata record moves.
- **Connector settings are stripped on copy.** MCP connector IDs belong to the account that created them and don't resolve elsewhere.
- **Deleting writes a tombstone**, the same marker Claude Code uses. The conversation stays on disk.

## FAQ

**Where does Claude Code store chat history on macOS?**
Two places. Metadata per account in `~/Library/Application Support/Claude/claude-code-sessions/`, and the conversations themselves as JSONL in `~/.claude/projects/`.

**I switched Claude accounts and my chats disappeared. Are they gone?**
No. The transcripts are still on disk — only the per-account metadata changed. Ferry lists every account it finds and can copy a chat into the one you're using now.

**Can I recover a deleted Claude Code chat?**
Usually. Deletion writes a small tombstone rather than erasing the conversation, so if the transcript hasn't been pruned, Ferry can restore it from another account or from `~/.ferry`.

**Why does a chat show a warning dot?**
Its transcript was pruned by Claude Code's cleanup. The record survives, the conversation doesn't. Backing up prevents this.

**Does this send anything anywhere?**
No. Ferry has no network code. It reads and writes local files only.

**Does it work on Windows or Linux?**
macOS and Windows are both built and tested. On Windows the layout is
`%APPDATA%\Claude\claude-code-sessions` with transcripts in `%USERPROFILE%\.claude\projects`,
and a working directory like `D:\Claude` maps to the folder `D--Claude`. Linux has no Claude
Code desktop app, so there are no per-account chat records to move — only the archive half
would apply.

**Ferry shows no accounts on Windows, but Claude has chats.**
The Microsoft Store build of Claude runs in a container that redirects `%APPDATA%\Claude` to
`%LOCALAPPDATA%\Packages\Claude_<id>\LocalCache\Roaming\Claude`. Only Claude sees the redirect,
so anything outside it — Ferry, or a terminal you opened yourself — finds `%APPDATA%\Claude`
empty. Ferry checks both and uses the one with the newest chats. If it still comes back empty,
the window lists every path it looked at; include that in an issue.

**Is it affiliated with Anthropic?**
No. Ferry is an independent tool that reads local files written by Claude Code.

## How it's built

Rust + [Tauri 2](https://tauri.app) with a plain HTML/CSS/JS front end — no framework, no npm, no build step for the UI. The Markdown renderer is about 60 lines, written for this app, so nothing is fetched at runtime.

On Windows, the loading mascot is the Ferry logo come alive: one head per account, joined by
the bar, with a chat that rides across while a copy or move runs. It is pixel art in the style of
[mascot-maker](https://github.com/ahkamboh/mascot-maker), drawn as SVG in the theme's colours.

## Contributing

Issues and pull requests welcome. If you hit a layout Ferry doesn't understand — a different Claude Code version, an org setup it misreads — open an issue with the shape of your `claude-code-sessions` folder (no file contents needed).

## Licence

[MIT](LICENSE) © Ali Hamza Kamboh
