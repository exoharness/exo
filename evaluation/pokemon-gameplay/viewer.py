"""Live dashboard for following a Pokemon agent run.

  python3 viewer.py [--port 8778]     # then open http://127.0.0.1:8778

Reads only from runtime/ (screenshots, agent.log, playbook, todos, progress,
history) — it never talks to the emulator, so it cannot slow the agent down.
stdlib only.
"""

from __future__ import annotations

import argparse
import json
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent
RUNTIME = BASE_DIR / "runtime"
SCREENSHOTS = RUNTIME / "screenshots"
FRAME_RE = re.compile(r"^frame-\d{6}-t\d+\.png$")

PAGE = """<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>exo plays pokemon</title>
<style>
  body { background:#0d1117; color:#c9d1d9; font:14px/1.45 ui-monospace,Menlo,monospace;
         margin:0; padding:16px; }
  h1 { font-size:16px; margin:0 0 12px; color:#e6edf3; }
  h1 .live { color:#3fb950; }
  .grid { display:grid; grid-template-columns: minmax(320px,520px) 1fr; gap:16px; }
  .panel { background:#161b22; border:1px solid #30363d; border-radius:8px;
           padding:12px; margin-bottom:16px; }
  .panel h2 { font-size:12px; text-transform:uppercase; letter-spacing:.08em;
              color:#8b949e; margin:0 0 8px; }
  #frame { width:100%; image-rendering:pixelated; border-radius:4px; background:#000; }
  #statusline { color:#e6edf3; margin-top:8px; white-space:pre-wrap; }
  #milestones div { color:#d29922; }
  #milestones div:first-child { color:#e3b341; font-weight:bold; }
  #log { white-space:pre-wrap; max-height:320px; overflow-y:auto; color:#8b949e; }
  #log .turnline { color:#c9d1d9; }
  #log .improve { color:#3fb950; }
  #log .milestone { color:#e3b341; }
  #playbook { white-space:pre-wrap; max-height:420px; overflow-y:auto; }
  #todos div.done { color:#484f58; text-decoration:line-through; }
  #todos div.in_progress { color:#58a6ff; }
  #tools summary { color:#3fb950; cursor:pointer; }
  #memory summary { color:#bc8cff; cursor:pointer; }
  #tools pre, #memory pre { white-space:pre-wrap; max-height:260px; overflow-y:auto;
    background:#0d1117; border:1px solid #30363d; border-radius:4px;
    padding:8px; margin:6px 0; color:#8b949e; }
  .muted { color:#484f58; }
</style>
</head>
<body>
<h1>exo plays pokemon <span class="live">●</span>
    <span id="turncount" class="muted"></span></h1>
<div class="grid">
  <div>
    <div class="panel">
      <h2>Game</h2>
      <img id="frame" alt="game frame">
      <div id="statusline" class="muted">waiting for frames...</div>
    </div>
    <div class="panel"><h2>Milestones (from game RAM)</h2><div id="milestones" class="muted">none yet</div></div>
    <div class="panel"><h2>Todos (agent-maintained)</h2><div id="todos" class="muted">none yet</div></div>
    <div class="panel"><h2>Tools the agent built</h2><div id="tools" class="muted">none yet</div></div>
    <div class="panel"><h2>Memory (agent-authored)</h2><div id="memory" class="muted">none yet</div></div>
  </div>
  <div>
    <div class="panel"><h2>Run log</h2><div id="log"></div></div>
    <div class="panel"><h2>Playbook (the agent's self-edited prompt)</h2><div id="playbook" class="muted">seed</div></div>
  </div>
</div>
<script>
async function refresh() {
  try {
    const r = await fetch('/api/latest');
    const d = await r.json();
    if (d.frame) {
      document.getElementById('frame').src = '/shot/' + d.frame + '?v=' + d.frame;
    }
    document.getElementById('statusline').textContent = d.status || '';
    document.getElementById('turncount').textContent =
      d.turn ? ('turn ' + d.turn + ' · ' + d.frames + ' frames captured') : '';
    const ms = document.getElementById('milestones');
    ms.innerHTML = d.milestones.length
      ? d.milestones.map(m => '<div>\\u2605 ' + esc(m) + '</div>').join('')
      : '<span class="muted">none yet</span>';
    document.getElementById('todos').innerHTML = d.todos.length
      ? d.todos.map(t => '<div class="' + t.status + '">' +
          (t.status === 'done' ? '\\u2713 ' : t.status === 'in_progress' ? '\\u25B8 ' : '\\u00B7 ') +
          esc(t.text) + '</div>').join('')
      : '<span class="muted">none yet</span>';
    document.getElementById('tools').innerHTML = d.tools.length
      ? d.tools.map(t => '<details><summary>\\u2699 ' + esc(t.name) + ' (' + t.lines +
          ' lines)</summary><pre>' + esc(t.source) + '</pre></details>').join('')
      : '<span class="muted">none yet</span>';
    document.getElementById('memory').innerHTML = d.memories.length
      ? d.memories.map(m => '<details><summary>\\u25C6 ' + esc(m.name) +
          '</summary><pre>' + esc(m.content) + '</pre></details>').join('')
      : '<span class="muted">none yet</span>';
    const log = document.getElementById('log');
    const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 8;
    log.innerHTML = d.log.map(line => {
      let cls = '';
      if (/^T\\d+/.test(line)) cls = 'turnline';
      if (line.includes('MILESTONE')) cls = 'milestone';
      if (/\\[(PLAYBOOK|NEW TOOL|MEMORY|TODOS|REWIND)/.test(line)) cls = 'improve';
      return '<div class="' + cls + '">' + esc(line) + '</div>';
    }).join('');
    if (atBottom) log.scrollTop = log.scrollHeight;
    document.getElementById('playbook').textContent = d.playbook || '(seed)';
  } catch (e) { /* server briefly away; keep polling */ }
}
function esc(s) {
  return s.replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));
}
refresh();
setInterval(refresh, 1000);
</script>
</body>
</html>
"""


def latest_payload() -> dict:
    frames = (
        sorted(f.name for f in SCREENSHOTS.glob("frame-*.png"))
        if SCREENSHOTS.is_dir()
        else []
    )
    frame = frames[-1] if frames else None

    log_lines: list[str] = []
    log_path = RUNTIME / "agent.log"
    if log_path.exists():
        log_lines = log_path.read_text(errors="replace").splitlines()[-80:]

    milestones: list[str] = []
    progress_path = RUNTIME / "progress.jsonl"
    if progress_path.exists():
        for line in progress_path.read_text().splitlines()[-12:]:
            try:
                entry = json.loads(line)
                milestones.append(f"turn {entry['turn']}: {entry['milestone']}")
            except (json.JSONDecodeError, KeyError):
                pass
    milestones.reverse()

    todos: list[dict] = []
    todos_path = RUNTIME / "todos.json"
    if todos_path.exists():
        try:
            todos = json.loads(todos_path.read_text())
        except json.JSONDecodeError:
            pass

    playbook = ""
    playbook_path = RUNTIME / "playbook.md"
    if playbook_path.exists():
        playbook = playbook_path.read_text(errors="replace")

    turn = None
    status = ""
    history_path = RUNTIME / "history.json"
    if history_path.exists():
        try:
            history = json.loads(history_path.read_text())
            if history:
                turn = history[-1]["turn"]
                status = history[-1]["summary"]
        except (json.JSONDecodeError, KeyError, IndexError):
            pass

    tools: list[dict] = []
    tools_dir = RUNTIME / "tools"
    if tools_dir.is_dir():
        for file in sorted(tools_dir.glob("*.mjs")):
            source = file.read_text(errors="replace")
            tools.append(
                {
                    "name": file.stem,
                    "lines": source.count("\n") + 1,
                    "source": source,
                }
            )

    memories: list[dict] = []
    memory_dir = RUNTIME / "memory"
    if memory_dir.is_dir():
        for file in sorted(memory_dir.glob("*.md")):
            memories.append(
                {"name": file.stem, "content": file.read_text(errors="replace")}
            )

    return {
        "frame": frame,
        "frames": len(frames),
        "turn": turn,
        "status": status,
        "log": log_lines,
        "milestones": milestones,
        "todos": todos,
        "playbook": playbook,
        "tools": tools,
        "memories": memories,
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args) -> None:
        pass

    def _send(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        path = self.path.split("?", 1)[0]
        if path == "/":
            self._send(200, "text/html; charset=utf-8", PAGE.encode())
        elif path == "/api/latest":
            self._send(200, "application/json", json.dumps(latest_payload()).encode())
        elif path.startswith("/shot/"):
            name = path[len("/shot/") :]
            file = SCREENSHOTS / name
            # FRAME_RE keeps this from serving anything outside screenshots/.
            if FRAME_RE.match(name) and file.is_file():
                self._send(200, "image/png", file.read_bytes())
            else:
                self._send(404, "text/plain", b"not found")
        else:
            self._send(404, "text/plain", b"not found")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8778)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"viewer on http://{args.host}:{args.port}")
    server.serve_forever()


if __name__ == "__main__":
    main()
