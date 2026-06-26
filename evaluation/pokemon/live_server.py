#!/usr/bin/env python3
"""Tiny live web view for the Pokémon run — watch exo play in your browser.

Serves the current emulator screenshot + the agent's latest turn (buttons,
reasoning) + its durable memory, all auto-refreshing. Reads the files
pokemon_runner.py writes (/tmp/exo-pokemon/screen.png + state.json), so just run
this alongside a game:

    # terminal 1 (the box):
    python live_server.py --port 8080
    # terminal 2: start a game (OPENAI_API_KEY=... POKEMON_ROM=... ./run.sh --steps 300)
    # your laptop:
    ssh -L 8080:localhost:8080 <box>     # then open http://localhost:8080

No external deps (stdlib only).
"""
from __future__ import annotations

import argparse
import http.server
import json
import os

DIR = "/tmp/exo-pokemon"  # must match pokemon_runner.py / harness-pokemon.ts
GUIDANCE = os.path.join(DIR, "guidance.json")  # player "coach" channel; read by the harness

PAGE = """<!doctype html><html><head><meta charset="utf-8"><title>exo plays Pokemon — live</title>
<style>
 *{box-sizing:border-box} body{margin:0;background:#0c0d18;color:#e8e8f0;font:15px/1.5 system-ui,-apple-system,sans-serif}
 .wrap{max-width:1000px;margin:0 auto;padding:24px;display:grid;grid-template-columns:1fr 1fr;gap:22px}
 h1{grid-column:1/-1;margin:0 0 4px;font-size:22px} .sub{grid-column:1/-1;color:#9aa0c0;margin:-6px 0 6px;font-size:13px}
 .screen{display:flex;flex-direction:column;align-items:center}
 img{image-rendering:pixelated;width:100%;max-width:420px;border:9px solid #1c1d33;border-radius:12px;background:#000;box-shadow:0 16px 44px rgba(0,0,0,.5)}
 .bar{display:flex;gap:8px;align-items:center;margin-top:12px;flex-wrap:wrap;justify-content:center}
 .turn{font-size:18px;font-weight:700;color:#6cf;font-variant-numeric:tabular-nums}
 .chip{background:#2a2c4a;color:#fc6;border-radius:6px;padding:3px 10px;font-weight:600;font-size:13px}
 .panel{background:#13142a;border-radius:12px;padding:14px 16px}
 .lbl{color:#7e84a8;font-size:12px;text-transform:uppercase;letter-spacing:.05em;margin:0 0 4px}
 .reason{font-size:15px;min-height:42px}
 ul{margin:6px 0 0;padding:0;list-style:none} li{background:#191b34;border-radius:8px;padding:7px 10px;margin:5px 0;font-size:13px}
 .dot{display:inline-block;width:8px;height:8px;border-radius:50%;background:#3a3} .off{background:#a33}
 .live{font-size:12px;color:#9aa0c0}
 .coach{border:1px solid #2d2f55} input{width:100%;background:#0c0d18;border:1px solid #2d2f55;color:#e8e8f0;border-radius:7px;padding:9px 11px;font:14px system-ui}
 .btns{display:flex;gap:8px;margin-top:8px} button{background:#3550e0;color:#fff;border:0;border-radius:7px;padding:8px 16px;font-weight:600;cursor:pointer;font-size:13px} button.sec{background:#2a2c4a}
 .gcur{margin-top:9px;font-size:13px;color:#ffd479;min-height:18px} .gcur b{color:#7e84a8;font-weight:600}
 details{margin:5px 0;font-size:13px} summary{cursor:pointer;color:#8fd} pre{background:#0c0d18;border:1px solid #2d2f55;border-radius:7px;padding:8px;overflow:auto;font-size:11px;max-height:240px;white-space:pre-wrap}
</style></head><body>
<div class="wrap">
 <h1>🎮 exo plays Pok&eacute;mon &mdash; live</h1>
 <div class="sub"><span class="dot off" id="dot"></span> <span class="live" id="live">waiting for a game to start&hellip;</span></div>
 <div class="screen">
   <img id="screen" src="/screen.png" alt="game screen">
   <div class="bar"><span class="turn" id="turn">turn &mdash;</span> <span id="buttons"></span></div>
 </div>
 <div>
   <div class="panel coach"><p class="lbl">🎤 direct exo (live)</p>
     <input id="gin" placeholder="tell exo what to do — e.g. 'enter the building to your left'" autocomplete="off">
     <div class="btns"><button id="gsend">Send</button><button id="gclear" class="sec">Clear</button></div>
     <div class="gcur" id="gcur"></div>
   </div>
   <div class="panel" style="margin-top:14px"><p class="lbl">reasoning</p><div class="reason" id="reason">&mdash;</div></div>
   <div class="panel" style="margin-top:14px"><p class="lbl">durable memory</p><ul id="mem"></ul></div>
   <div class="panel" style="margin-top:14px"><p class="lbl">🛠️ self-improvement</p>
     <div class="gcur" id="selfedits"></div>
     <div id="tools"></div>
   </div>
 </div>
</div>
<script>
let last=-1;
async function tick(){
  try{
    const r=await fetch('/state',{cache:'no-store'}); const s=await r.json();
    document.getElementById('dot').className='dot';
    document.getElementById('live').textContent='live · '+(new Date().toLocaleTimeString());
    if(typeof s.turn==='number'){
      document.getElementById('turn').textContent='turn '+s.turn+(s.total?(' / '+s.total):'');
      document.getElementById('buttons').innerHTML=(s.buttons||[]).map(b=>'<span class="chip">'+b+'</span>').join(' ');
      document.getElementById('reason').textContent=s.reasoning||'—';
      document.getElementById('mem').innerHTML=(s.memory||[]).map(m=>'<li>'+(m.text||m).replace(/</g,'&lt;')+'</li>').join('');
      const se=document.getElementById('selfedits');
      if(se) se.innerHTML = (typeof s.self_edits==='number') ? ('<b>policy self-edits:</b> '+s.self_edits) : '<b>self-edits:</b> —';
      const td=document.getElementById('tools');
      if(td) td.innerHTML=(s.tools&&s.tools.length)
        ? ('<p class="lbl" style="margin-top:9px">agent-created tools ('+s.tools.length+')</p>'+s.tools.map(t=>'<details><summary>'+(t.name||'tool').replace(/</g,'&lt;')+'</summary><pre>'+(t.source||'').replace(/</g,'&lt;').slice(0,2000)+'</pre></details>').join(''))
        : '<div class="gcur" style="margin-top:6px"><b>created tools:</b> none yet</div>';
      if(s.turn!==last){ last=s.turn; document.getElementById('screen').src='/screen.png?t='+s.turn+'_'+Date.now(); }
    }
  }catch(e){ document.getElementById('dot').className='dot off'; document.getElementById('live').textContent='no game running'; }
}
async function postG(text){ await fetch('/guidance',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text})}); showG(); }
async function showG(){ try{ const g=await (await fetch('/guidance',{cache:'no-store'})).json();
  document.getElementById('gcur').innerHTML = g.text ? ('<b>active:</b> '+g.text.replace(/</g,'&lt;')) : '<b>none</b> — exo plays on its own'; }catch(e){} }
document.getElementById('gsend').onclick=()=>{ const v=document.getElementById('gin').value.trim(); if(v) postG(v); };
document.getElementById('gin').addEventListener('keydown',e=>{ if(e.key==='Enter'){ const v=e.target.value.trim(); if(v) postG(v); } });
document.getElementById('gclear').onclick=()=>{ document.getElementById('gin').value=''; postG(''); };
setInterval(tick,700); tick(); showG();
</script></body></html>"""


class Handler(http.server.BaseHTTPRequestHandler):
    def _send(self, code: int, ctype: str, body: bytes) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        path = self.path.split("?", 1)[0]
        if path == "/":
            self._send(200, "text/html; charset=utf-8", PAGE.encode())
        elif path == "/screen.png":
            p = os.path.join(DIR, "screen.png")
            if os.path.exists(p):
                self._send(200, "image/png", open(p, "rb").read())
            else:
                self._send(404, "text/plain", b"no screen yet")
        elif path == "/state":
            p = os.path.join(DIR, "state.json")
            self._send(200, "application/json", open(p, "rb").read() if os.path.exists(p) else b"{}")
        elif path == "/guidance":
            self._send(200, "application/json", open(GUIDANCE, "rb").read() if os.path.exists(GUIDANCE) else b"{}")
        else:
            self._send(404, "text/plain", b"not found")

    def do_POST(self) -> None:
        if self.path.split("?", 1)[0] != "/guidance":
            self._send(404, "text/plain", b"not found")
            return
        n = int(self.headers.get("Content-Length", 0))
        try:
            body = json.loads(self.rfile.read(n) or b"{}")
            text = str(body.get("text", "")).strip()
        except Exception:
            text = ""
        # Persist (or clear). The harness reads this each turn and injects it.
        os.makedirs(DIR, exist_ok=True)
        with open(GUIDANCE, "w") as f:
            json.dump({"text": text}, f)
        self._send(200, "application/json", json.dumps({"ok": True, "text": text}).encode())

    def log_message(self, *args) -> None:  # quiet
        pass


def main() -> None:
    ap = argparse.ArgumentParser(description="Live web view for the Pokémon run.")
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--host", default="127.0.0.1",
                    help="bind address. 127.0.0.1 = localhost only (use ssh -L). "
                         "0.0.0.0 = all interfaces (reachable over a Tailscale tailnet / LAN).")
    args = ap.parse_args()
    os.makedirs(DIR, exist_ok=True)
    print(f"live view binding {args.host}:{args.port}")
    if args.host in ("127.0.0.1", "localhost"):
        print(f"  localhost-only — port-forward: ssh -L {args.port}:localhost:{args.port} <box>")
    else:
        print(f"  reachable on the tailnet/LAN at <host>:{args.port}")
    http.server.ThreadingHTTPServer((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
