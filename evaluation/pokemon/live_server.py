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
import difflib
import http.server
import json
import os

DIR = "/tmp/exo-pokemon"  # must match pokemon_runner.py / harness-pokemon.ts
GUIDANCE = os.path.join(DIR, "guidance.json")  # player "coach" channel; read by the harness
READ_ONLY = False  # set by --read-only: hide the coach box + reject guidance posts (safe for public sharing)
# The harness version the agent STARTED from (its self-edits are diffed against this
# to show, live, what its policy edits actually added).
BASELINE_HARNESS = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..", "..", "examples", "simple-coding-agent", "harness-pokemon-selfimprove.ts",
)


def _harness_tool_names(text: str) -> set:
    """Tool names defined in a harness file (skipping // comments)."""
    import re
    code = "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("//"))
    return set(re.findall(r"""name:\s*["']([^"']+)["']\s*,\s*description:""", code))


def _baseline_tool_names() -> set:
    try:
        return _harness_tool_names(open(BASELINE_HARNESS).read())
    except Exception:
        return set()


def _policy_additions(harness: str) -> list:
    """Lines the agent ADDED to its own file vs the version it started from — i.e.
    what its policy self-edits actually changed. Demonstrates the benefit live."""
    try:
        base = open(BASELINE_HARNESS).read()
    except Exception:
        return []
    added = []
    for line in difflib.ndiff(base.splitlines(), harness.splitlines()):
        if line.startswith("+ "):
            t = line[2:].rstrip()
            if t.strip():
                added.append(t)
    return added

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
 .spend{grid-column:1/-1} canvas{width:100%;height:auto;margin-top:10px;border-radius:8px;background:#0c0d18}
 .big{font-size:26px;font-weight:800;color:#7fdca0;font-variant-numeric:tabular-nums} .rate{font-size:13px;color:#9aa0c0}
 .rate b{font-variant-numeric:tabular-nums} .down{color:#7fdca0} .up{color:#f08a8a}
</style></head><body>
<div class="wrap">
 <h1>🎮 exo plays Pok&eacute;mon &mdash; live</h1>
 <div class="sub"><span class="dot off" id="dot"></span> <span class="live" id="live">waiting for a game to start&hellip;</span></div>
 <div class="screen">
   <img id="screen" src="/screen.png" alt="game screen">
   <div class="bar"><span class="turn" id="turn">turn &mdash;</span> <span id="buttons"></span></div>
   <div class="panel" style="margin-top:12px;width:100%"><p class="lbl">🗺️ game progress</p>
     <div class="bar" style="justify-content:flex-start;gap:14px">
       <span class="chip" id="pbadges">badges —</span>
       <span class="chip" id="pmaps">maps —</span>
       <span class="chip" id="ppos">pos —</span>
     </div>
     <pre id="minimap" style="margin-top:8px;font-size:12px;line-height:1.05;max-height:200px">&mdash;</pre>
   </div>
 </div>
 <div>
   <div class="panel coach"><p class="lbl">🎤 direct exo (live)</p>
     <input id="gin" placeholder="tell exo what to do — e.g. 'enter the building to your left'" autocomplete="off">
     <div class="btns"><button id="gsend">Send</button><button id="gclear" class="sec">Clear</button></div>
     <div class="gcur" id="gcur"></div>
   </div>
   <div class="panel" style="margin-top:14px"><p class="lbl">reasoning</p><div class="reason" id="reason">&mdash;</div></div>
   <div class="panel" style="margin-top:14px"><p class="lbl">🛠️ self-improvement &mdash; tools it built</p>
     <div class="gcur" id="selfedits"></div>
     <div id="tools"></div>
   </div>
   <div class="panel" style="margin-top:14px"><p class="lbl">durable memory &mdash; what it learned</p><ul id="mem"></ul></div>
 </div>
 <div class="panel spend">
   <p class="lbl">💰 cumulative spend — watch the curve flatten as it gets efficient</p>
   <span class="big" id="costtot">$—</span> <span class="rate" id="costrate"></span>
   <canvas id="chart" width="940" height="240"></canvas>
   <div id="improvelog" style="margin-top:14px"></div>
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
      // Only re-render tools when they actually change, else the 700ms refresh
      // would snap any open <details> shut while you're reading the source.
      const tkey=JSON.stringify(s.tools||[]);
      if(td && tkey!==window.__toolsKey){ window.__toolsKey=tkey;
        td.innerHTML=(s.tools&&s.tools.length)
          ? ('<p class="lbl" style="margin-top:9px">agent-created tools ('+s.tools.length+')</p>'+s.tools.map(t=>'<details><summary>'+(t.name||'tool').replace(/</g,'&lt;')+'</summary><pre>'+(t.source||'').replace(/</g,'&lt;').slice(0,2000)+'</pre></details>').join(''))
          : '<div class="gcur" style="margin-top:6px"><b>created tools:</b> none yet</div>';
      }
      const g=s.game||{};
      if(typeof g.map==='number'){
        document.getElementById('pbadges').textContent='badges '+(g.badges||0);
        document.getElementById('pmaps').textContent='maps '+(s.maps_visited||0);
        document.getElementById('ppos').textContent='map '+g.map+' ('+g.x+','+g.y+')';
        const mm=document.getElementById('minimap'); if(mm && typeof g.minimap==='string') mm.textContent=g.minimap||'—';
      }
      if(typeof s.cost_total==='number'){
        document.getElementById('costtot').textContent='$'+s.cost_total.toFixed(2);
        const r=s.cost_recent_avg||0, p=s.cost_prev_avg||0;
        let cls='', arrow='';
        if(p>0 && r<p){ cls='down'; arrow='▼'; } else if(r>p){ cls='up'; arrow='▲'; }
        const pct = p>0 ? Math.round((r-p)/p*100) : 0;
        document.getElementById('costrate').innerHTML =
          'recent <b>$'+r.toFixed(3)+'</b>/turn '+(p>0?('<span class="'+cls+'">'+arrow+' '+(pct>0?'+':'')+pct+'% vs earlier</span>'):'');
        drawChart(s.cost_series||[], s.improvement_events||[]);
      }
      // self-improvement log: WHAT it changed about itself (tools built + policy it added)
      const il=document.getElementById('improvelog'); const adds=s.policy_additions||[]; const tls=s.tools||[];
      const ilkey=JSON.stringify([adds.length, tls.map(t=>t.name)]);
      if(il && ilkey!==window.__ilKey){ window.__ilKey=ilkey;
        let html='<p class="lbl">🧠 what it changed about itself</p>';
        if(tls.length) html+='<div style="margin:4px 0 8px"><b style="color:#6cf">tools it built ('+tls.length+'):</b><ul style="margin:4px 0">'+
          tls.map(t=>'<li><b>'+String(t.name||'').replace(/</g,'&lt;')+'</b> — '+String(t.source||'').replace(/</g,'&lt;')+'</li>').join('')+'</ul></div>';
        if(adds.length) html+='<details><summary style="color:#ffb454;cursor:pointer">policy/code it added to its own file ('+adds.length+' lines) — click</summary><pre>'+
          adds.map(l=>String(l).replace(/</g,'&lt;')).join(String.fromCharCode(10))+'</pre></details>';
        if(!tls.length && !adds.length) html+='<div class="gcur">nothing yet — it has not edited itself</div>';
        il.innerHTML=html;
      }
      if(s.turn!==last){ last=s.turn; document.getElementById('screen').src='/screen.png?t='+s.turn+'_'+Date.now(); }
    }
  }catch(e){ document.getElementById('dot').className='dot off'; document.getElementById('live').textContent='no game running'; }
}
function drawChart(series, events){
  const cv=document.getElementById('chart'); if(!cv||!series.length) return;
  const ctx=cv.getContext('2d'), W=cv.width, H=cv.height, pad=44;
  ctx.clearRect(0,0,W,H);
  const xs=series.map(d=>d[0]), ys=series.map(d=>d[1]);
  const xmax=Math.max(1,xs[xs.length-1]), ymax=Math.max(0.01,ys[ys.length-1]);
  const X=t=>pad+(W-pad-12)*(t/xmax), Y=v=>H-pad-(H-pad-16)*(v/ymax);
  // grid + y labels (cost)
  ctx.strokeStyle='#1c1d33'; ctx.fillStyle='#6b6f93'; ctx.font='11px system-ui'; ctx.lineWidth=1;
  for(let i=0;i<=4;i++){ const v=ymax*i/4, y=Y(v); ctx.beginPath(); ctx.moveTo(pad,y); ctx.lineTo(W-12,y); ctx.stroke();
    ctx.fillText('$'+v.toFixed(v<1?2:0), 6, y+4); }
  ctx.fillText('turn '+xmax, W-70, H-14); ctx.fillText('0', pad-4, H-14);
  // per-turn cost as faint bars on a secondary (right) scale — shows whether
  // the cost PER TURN is bounded/falling (the "growth slows" signal)
  const pt=series.map(d=>d[2]||0); const ptmax=Math.max(0.001,...pt);
  const bw=Math.max(1,(W-pad-12)/series.length);
  ctx.fillStyle='rgba(108,204,255,.30)';
  for(let i=0;i<series.length;i++){ const h=(H-pad-16)*(pt[i]/ptmax); ctx.fillRect(X(xs[i])-bw/2, H-pad-h, Math.max(1,bw*0.8), h); }
  ctx.fillStyle='#6cf'; ctx.font='11px system-ui'; ctx.fillText('bars: $/turn (max $'+ptmax.toFixed(3)+')', pad+4, 14);
  // cumulative spend area + line
  const grad=ctx.createLinearGradient(0,0,0,H); grad.addColorStop(0,'rgba(127,220,160,.28)'); grad.addColorStop(1,'rgba(127,220,160,0)');
  ctx.beginPath(); ctx.moveTo(X(xs[0]),Y(ys[0])); for(let i=1;i<series.length;i++) ctx.lineTo(X(xs[i]),Y(ys[i]));
  ctx.lineTo(X(xs[xs.length-1]),H-pad); ctx.lineTo(X(xs[0]),H-pad); ctx.closePath(); ctx.fillStyle=grad; ctx.fill();
  ctx.beginPath(); ctx.moveTo(X(xs[0]),Y(ys[0])); for(let i=1;i<series.length;i++) ctx.lineTo(X(xs[i]),Y(ys[i]));
  ctx.strokeStyle='#7fdca0'; ctx.lineWidth=2.2; ctx.stroke();
  // self-improvement markers: when the agent built a tool / edited its policy /
  // banked a memory, mark that turn on the curve.
  events=events||[];
  const kindColor={tool:'#6cf',policy:'#ffb454',memory:'#c08cff'};
  const byTurn={}; series.forEach(d=>byTurn[d[0]]=d[1]);
  events.forEach(e=>{ const x=X(e.turn), col=kindColor[e.kind]||'#fff';
    ctx.strokeStyle=col+'55'; ctx.setLineDash([3,3]); ctx.lineWidth=1;
    ctx.beginPath(); ctx.moveTo(x,18); ctx.lineTo(x,H-pad); ctx.stroke(); ctx.setLineDash([]);
    const yv=(e.turn in byTurn)?Y(byTurn[e.turn]):22;
    ctx.fillStyle=col; ctx.beginPath(); ctx.arc(x,yv,3.6,0,6.3); ctx.fill();
  });
  if(events.length){ let lx=W-300; ctx.font='11px system-ui';
    [['tool','tool built'],['policy','policy edit'],['memory','memory']].forEach(([k,lbl])=>{
      ctx.fillStyle=kindColor[k]; ctx.beginPath(); ctx.arc(lx,11,3.6,0,6.3); ctx.fill();
      ctx.fillStyle='#9aa0c0'; ctx.fillText(lbl,lx+7,14); lx+=92; });
  }
}
async function postG(text){ await fetch('/guidance',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text})}); showG(); }
async function showG(){ try{ const g=await (await fetch('/guidance',{cache:'no-store'})).json();
  document.getElementById('gcur').innerHTML = g.text ? ('<b>active:</b> '+g.text.replace(/</g,'&lt;')) : '<b>none</b> — exo plays on its own'; }catch(e){} }
document.getElementById('gsend').onclick=()=>{ const v=document.getElementById('gin').value.trim(); if(v) postG(v); };
document.getElementById('gin').addEventListener('keydown',e=>{ if(e.key==='Enter'){ const v=e.target.value.trim(); if(v) postG(v); } });
document.getElementById('gclear').onclick=()=>{ document.getElementById('gin').value=''; postG(''); };
(async function(){ try{ const c=await (await fetch('/config')).json();
  if(c.read_only){ const el=document.querySelector('.coach'); if(el) el.style.display='none'; } }catch(e){} })();
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
            try:
                s = json.load(open(p)) if os.path.exists(p) else {}
                if isinstance(s.get("harness"), str):
                    s["policy_additions"] = _policy_additions(s["harness"])
                # Show only AGENT-built tools, not the baseline template that ships
                # in the harness, so the panel reflects real self-improvement.
                base = _baseline_tool_names()
                if isinstance(s.get("tools"), list):
                    s["tools"] = [t for t in s["tools"] if t.get("name") not in base]
                body = json.dumps(s).encode()
            except Exception:
                body = open(p, "rb").read() if os.path.exists(p) else b"{}"
            self._send(200, "application/json", body)
        elif path == "/guidance":
            self._send(200, "application/json", open(GUIDANCE, "rb").read() if os.path.exists(GUIDANCE) else b"{}")
        elif path == "/config":
            self._send(200, "application/json", json.dumps({"read_only": READ_ONLY}).encode())
        else:
            self._send(404, "text/plain", b"not found")

    def do_POST(self) -> None:
        if READ_ONLY:
            self._send(403, "text/plain", b"read-only")
            return
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
    ap.add_argument("--read-only", action="store_true",
                    help="hide the coach box + reject guidance posts (safe for public sharing)")
    args = ap.parse_args()
    global READ_ONLY
    READ_ONLY = args.read_only
    os.makedirs(DIR, exist_ok=True)
    print(f"live view binding {args.host}:{args.port}")
    if args.host in ("127.0.0.1", "localhost"):
        print(f"  localhost-only — port-forward: ssh -L {args.port}:localhost:{args.port} <box>")
    else:
        print(f"  reachable on the tailnet/LAN at <host>:{args.port}")
    http.server.ThreadingHTTPServer((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
