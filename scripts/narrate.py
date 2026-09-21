"""Voice the README demo: one spoken line per step, offline, with Kokoro.

    python3 scripts/narrate.py            # writes renders/voice/<id>.wav and lines.json
    node scripts/record-demo.mjs          # then records, holding each step until its line is said

The same lines are the video's subtitles. Kokoro (kokoro-onnx) runs locally; point
KOKORO_MODEL / KOKORO_VOICES at its two files if they aren't in ~/.cache/hyperframes/tts.
KOKORO_VOICE picks the voice (af_heart by default).
"""
import json, os, sys, wave

import numpy as np
from kokoro_onnx import Kokoro

# (step id, what is said and shown). The ids are the ones record-demo.mjs uses.
LINES = [
    ("accounts", "Ferry lists every Claude account on your machine, even the ones you're signed out of."),
    ("read",     "Open a signed-out account. Its chats are all still here."),
    ("move",     "Move one into the account you use now, and Claude picks up the old context."),
    ("drag",     "Or drag a chat onto any account."),
    ("nickname", "Give an account a nickname."),
    ("search",   "Search your chats."),
    ("pruned",   "An orange dot marks a chat whose transcript Claude's cleanup removed."),
    ("rename",   "Click a chat's title to rename it."),
    ("folder",   "A chat started without a folder can be pointed at the one it belongs to."),
    ("download", "Download a whole conversation as Markdown, text or JSON."),
    ("restore",  "Delete a chat, and restore one from Ferry's archive."),
    ("cli",      "Chats from the CLI and VS Code show up too, though no account lists them."),
    ("cliadd",   "Add one to an account, and Claude lists it there."),
    ("cursor",   "So do your Cursor chats."),
    ("cursoradd","Add one, and it becomes a Claude chat."),
    ("convert",  "And a Claude chat converts into Cursor."),
    ("backup",   "One button backs up every chat and transcript, out of Claude's reach."),
    ("locked",   "While the Claude app is open, Ferry won't write."),
    ("quit",     "Quit Claude first, then confirm."),
    ("nearby",   "Nearby sends a chat to another computer on the same Wi-Fi."),
    ("code",     "Both screens show the same code before anything is sent."),
    ("receive",  "And it receives one the same way."),
    ("zoom",     "Zoom with Command plus and minus."),
    ("end",      "Ferry. Your Claude Code chats, in whichever account you use."),
]

HOME = os.path.expanduser("~/.cache/hyperframes/tts")
MODEL  = os.environ.get("KOKORO_MODEL",  os.path.join(HOME, "models", "kokoro-v1.0.onnx"))
VOICES = os.environ.get("KOKORO_VOICES", os.path.join(HOME, "voices", "voices-v1.0.bin"))
VOICE  = os.environ.get("KOKORO_VOICE", "af_heart")
OUT    = os.path.join(os.environ.get("OUT_DIR", "renders"), "voice")

def main():
    for p in (MODEL, VOICES):
        if not os.path.exists(p):
            sys.exit(f"missing {p}: set KOKORO_MODEL and KOKORO_VOICES")
    os.makedirs(OUT, exist_ok=True)
    tts = Kokoro(MODEL, VOICES)
    out = []
    for key, text in LINES:
        samples, rate = tts.create(text, voice=VOICE, speed=1.0, lang="en-us")
        pcm = (np.clip(samples, -1, 1) * 32767).astype("<i2")
        path = os.path.join(OUT, f"{key}.wav")
        with wave.open(path, "wb") as w:
            w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate); w.writeframes(pcm.tobytes())
        out.append({"id": key, "text": text, "file": path, "dur": round(len(pcm) / rate, 3)})
        print(f"{out[-1]['dur']:5.2f}s  {key}")
    with open(os.path.join(OUT, "lines.json"), "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=1)

if __name__ == "__main__":
    main()
