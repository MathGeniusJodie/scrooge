#!/usr/bin/env python3
"""
Regenerate flowchart.svg.

Edit SPINE / SIDES / NOTE below, then:
    python3 gen_flowchart.py > flowchart.svg

All y-positions are computed from SPINE order — just add/remove entries and re-run.
"""

# ── dimensions ────────────────────────────────────────────────────────────────
W   = 940
CX  = 470          # spine centre x
BW  = 360          # spine box width
BX  = CX - BW//2  # left x of spine boxes  (= 290)
GAP = 20           # vertical gap between spine nodes
LS  = 16           # px per additional text line
DX  = 100          # diamond horizontal half-extent

def bh(n): return 20 + n * LS + 10   # box height: 1 title + n sub-lines
def dh(n): return 28 + n * LS        # diamond height: n label-lines

# ── content ───────────────────────────────────────────────────────────────────
# Each entry: (id, "box"|"dia", title_string_or_label_list, sub_lines_list)
SPINE = [
    ("cli",     "box", "CLI entry", [
        "main.rs → Cli / main()",
    ]),
    ("task",    "box", "Task loop", [
        "agents.rs → Orchestrator::run_task()",
    ]),
    ("ctx",     "box", "Deterministic context  (zero LLM tokens)", [
        "agents.rs → ensure_overview()  ·  overview.rs → load() / save()",
        "codemap.rs → build_cached() / brief()  ·  practices.rs → summary()",
        "gather_context() — read-only Cratchit pass",
        "tools: read_file · symbol_info · callers · callees",
    ]),
    ("brief",   "box", "Build Scrooge briefing — cached prefix", [
        "injected into log once; provider KV-cache amortises cost",
    ]),
    ("scrooge", "box", "Scrooge turn", [
        'agents.rs → client.chat("scrooge")  ·  openrouter.rs',
        "tools: delegate_to_cratchit  ·  web_answer (≤3 total)",
    ]),
    ("d_del",   "dia", ["tool", "calls?"], []),
    ("exec",    "box", "Execute & verify  (delegate_to_cratchit)", [
        "agents.rs → execute_and_verify()",
    ]),
    ("crat",    "box", "Cratchit tool loop  (≤40 iters)", [
        "agents.rs → cratchit_execute() → tool_loop()",
        "budget warning injected at 5 remaining",
    ]),
    ("clamp",   "box", "Clamp report  (≤12 lines / 1 200 chars)", [
        "agents.rs → clamp_report()",
    ]),
    ("d_fil",   "dia", ["files", "changed?"], []),
    ("checks",  "box", "Check suite: format → test → lint-fix → lint", [
        "checks.rs → run() / load() / run_cmd()",
        "config: .scrooge/checks.toml",
    ]),
    ("append",  "box", "Append CHANGED + CHECKS to report", [
        "agents.rs → execute_and_verify()",
    ]),
]

# Each entry: (id, anchor_id, x, w, title, subs)
SIDES = [
    ("done",  "d_del",  622, 290, "Task complete  (no tool calls)", [
        "checks clean / None → refresh_overview + bill",
        "checks red → inject nudge, loop back (≤2)",
    ]),
    ("web",   "d_del",   20, 256, "web_answer", [
        "tools.rs → web_answer()",
        "result appended to Scrooge's log",
    ]),
    ("tools", "crat",   686, 234, "Tools  (Landlock-sandboxed)", [
        "tools.rs → Toolbox::dispatch()",
        "read/edit/replace_symbol · shell ·",
        "python · query_docs · helpers …",
    ]),
]

NOTE = [
    "Plugin/MCP mode: Claude Code plays Scrooge — mcp.rs → Server::call_tool()  "
    "·  give_cratchit_task → agents.rs → delegate() → execute_and_verify()",
    "hooks: session_start.py (inject brief)  ·  prompt_submit.py (practices)  "
    "·  stop.py (scrooge check)",
]

# ── layout pass ───────────────────────────────────────────────────────────────
pos = {}
y = 44
for nid, ntype, title_or_labels, subs in SPINE:
    h = bh(len(subs)) if ntype == "box" else dh(len(title_or_labels))
    pos[nid] = dict(y=y, h=h, type=ntype)
    y += h + GAP

NOTE_H = 20 + (len(NOTE) - 1) * LS + 10
NOTE_Y = y + 10
SVG_H  = NOTE_Y + NOTE_H + 20

for sid, anchor, sx, sw, _t, ssubs in SIDES:
    an = pos[anchor]
    sy = an["y"] + (2 if an["type"] == "dia" else 0)
    pos[sid] = dict(y=sy, h=bh(len(ssubs)), x=sx, w=sw, type="side")

# ── position accessors ────────────────────────────────────────────────────────
def top(i):   return pos[i]["y"]
def bot(i):   return pos[i]["y"] + pos[i]["h"]
def midy(i):  return pos[i]["y"] + pos[i]["h"] // 2
def lft(i):   return pos[i].get("x", BX)
def rgt(i):   return pos[i].get("x", BX) + pos[i].get("w", BW)

# ── SVG builder ───────────────────────────────────────────────────────────────
buf = []
o = buf.append

def esc(s):
    return (s.replace("&", "&amp;")
             .replace('"', "&quot;")
             .replace("<", "&lt;")
             .replace(">", "&gt;"))

def seg(x1, y1, x2, y2, css="edge"):
    o(f'  <line class="{css}" x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}"/>')

def pth(d, css="edge"):
    o(f'  <path class="{css}" d="{d}"/>')

def txt(tx, ty, s, css, anchor="middle", fill=None):
    fa = f' fill="{fill}"' if fill else ""
    o(f'  <text class="{css}" x="{tx}" y="{ty}" text-anchor="{anchor}"{fa}>{esc(s)}</text>')

# ── header ────────────────────────────────────────────────────────────────────
o(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{SVG_H}" '
  f'viewBox="0 0 {W} {SVG_H}" font-family="Helvetica, Arial, sans-serif">')
o("""\
  <defs>
    <marker id="arr"   viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
      <path d="M0,0 L10,5 L0,10 z" fill="#334"/></marker>
    <marker id="arr-r" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
      <path d="M0,0 L10,5 L0,10 z" fill="#a33"/></marker>
    <marker id="arr-b" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
      <path d="M0,0 L10,5 L0,10 z" fill="#336"/></marker>
  </defs>
  <style>
    .box  { fill:#eef2fb; stroke:#334; stroke-width:1.2; }
    .side { fill:#fdf3e3; stroke:#875; stroke-width:1.2; }
    .dia  { fill:#e8f7ea; stroke:#363; stroke-width:1.2; }
    .t    { font-size:13px; font-weight:bold; fill:#112; text-anchor:middle; }
    .s    { font-size:11px; font-style:italic; fill:#556; text-anchor:middle; }
    .l    { font-size:11px; fill:#334; }
    .edge { stroke:#334; stroke-width:1.3; fill:none; marker-end:url(#arr); }
    .loop { stroke:#a33; stroke-width:1.3; fill:none; marker-end:url(#arr-r); stroke-dasharray:5 3; }
    .fwd  { stroke:#336; stroke-width:1.3; fill:none; marker-end:url(#arr-b); stroke-dasharray:4 3; }
  </style>""")

txt(CX, 26, "scrooge run — pipeline",
    css="", anchor="middle", fill=None)
# (override inline since this doesn't fit the .t class)
buf[-1] = (f'  <text x="{CX}" y="26" text-anchor="middle" '
           f'font-size="17" font-weight="bold" fill="#112">'
           f'scrooge run — pipeline</text>')

# ── spine nodes ───────────────────────────────────────────────────────────────
for nid, ntype, title_or_labels, subs in SPINE:
    n   = pos[nid]
    ny  = n["y"]
    nh  = n["h"]

    if ntype == "box":
        title = title_or_labels
        o(f'  <rect class="box" x="{BX}" y="{ny}" width="{BW}" height="{nh}" rx="6"/>')
        ty = ny + 20
        txt(CX, ty, title, "t")
        for i, s in enumerate(subs):
            txt(CX, ty + (i + 1) * LS, s, "s")
    else:
        labels = title_or_labels
        cyd = ny + nh // 2
        pts = f"{CX},{ny} {CX+DX},{cyd} {CX},{ny+nh} {CX-DX},{cyd}"
        o(f'  <polygon class="dia" points="{pts}"/>')
        oy = cyd - (len(labels) - 1) * LS // 2 + 5
        for i, lab in enumerate(labels):
            txt(CX, oy + i * LS, lab, "t")

# ── side panels ───────────────────────────────────────────────────────────────
for sid, _anchor, sx, sw, stitle, ssubs in SIDES:
    n  = pos[sid]
    ny = n["y"]
    nh = n["h"]
    cx = sx + sw // 2
    o(f'  <rect class="side" x="{sx}" y="{ny}" width="{sw}" height="{nh}" rx="6"/>')
    txt(cx, ny + 20, stitle, "t")
    for i, s in enumerate(ssubs):
        txt(cx, ny + 20 + (i + 1) * LS, s, "s")

# ── note bar ──────────────────────────────────────────────────────────────────
o(f'  <rect class="side" x="20" y="{NOTE_Y}" width="{W - 40}" height="{NOTE_H}" rx="6"/>')
for i, ln in enumerate(NOTE):
    txt(CX, NOTE_Y + 20 + i * LS, ln, "l")

# ── spine arrows (auto-generated between consecutive SPINE entries) ────────────
# Labels for specific transitions
EDGE_LABELS = {
    ("d_del", "exec"): "delegate_to_cratchit",
    ("d_fil", "checks"): "yes",
}
for i in range(len(SPINE) - 1):
    a, b = SPINE[i][0], SPINE[i+1][0]
    seg(CX, bot(a), CX, top(b))
    if (a, b) in EDGE_LABELS:
        my = (bot(a) + top(b)) // 2 + 4
        o(f'  <text class="l" x="{CX + 6}" y="{my}">{esc(EDGE_LABELS[(a, b)])}</text>')

# ── horizontal arrows to side panels ──────────────────────────────────────────
# d_del right → done (no)
dcy = midy("d_del")
seg(CX + DX, dcy, lft("done"), dcy)
o(f'  <text class="l" x="{CX + DX + 4}" y="{dcy - 8}">no</text>')

# d_del left → web (web_answer)
seg(CX - DX, dcy, rgt("web"), dcy)
o(f'  <text class="l" x="{CX - DX - 62}" y="{dcy - 8}">web_answer</text>')

# cratchit ↔ toolbox (bidirectional)
cm = midy("crat")
seg(rgt("crat"), cm - 8, lft("tools"), cm - 8)
seg(lft("tools"), cm + 8, rgt("crat"), cm + 8)

# d_fil right → bypass → append (no: skip checks)
dfcy   = midy("d_fil")
bpx    = BX + BW + 8   # bypass rail, 8px right of spine edge
apm    = midy("append")
pth(f"M{CX + DX},{dfcy} L{bpx},{dfcy} L{bpx},{apm} L{BX + BW},{apm}")
o(f'  <text class="l" x="{CX + DX + 5}" y="{dfcy - 8}">no: skip checks</text>')

# ── loop-back paths ───────────────────────────────────────────────────────────
# 1. Blue left rail: append bottom → x=88 → scrooge mid-left
LEFT_RAIL = 88
abm = bot("append")
sm  = midy("scrooge")
pth(f"M{CX},{abm} L{LEFT_RAIL},{abm} L{LEFT_RAIL},{sm} L{BX},{sm}", "fwd")
rmy = (abm + sm) // 2
o(f'  <text class="l" x="14" y="{rmy - 8}"  fill="#336">report</text>')
o(f'  <text class="l" x="14" y="{rmy + 6}"  fill="#336">→ tool</text>')
o(f'  <text class="l" x="14" y="{rmy + 20}" fill="#336">result</text>')

# 2. Blue web_answer panel top → scrooge left edge
web_cx  = lft("web") + pos["web"]["w"] // 2
sentry  = top("scrooge") + 14
pth(f"M{web_cx},{top('web')} L{web_cx},{sentry} L{BX},{sentry}", "fwd")

# 3. Red checks-fail retry: checks left → x=224 → crat left
RETRY_X = 224
pth(f"M{BX},{midy('checks')} L{RETRY_X},{midy('checks')} "
    f"L{RETRY_X},{midy('crat')} L{BX},{midy('crat')}", "loop")
rmy2 = (midy("checks") + midy("crat")) // 2
o(f'  <text class="l" x="{RETRY_X - 76}" y="{rmy2 - 8}"  fill="#a33">checks</text>')
o(f'  <text class="l" x="{RETRY_X - 76}" y="{rmy2 + 6}"  fill="#a33">fail:</text>')
o(f'  <text class="l" x="{RETRY_X - 76}" y="{rmy2 + 20}" fill="#a33">retry (≤2)</text>')

# 4. Red nudge: done right → x=928 → scrooge right edge
NUDGE_X = 928
pth(f"M{rgt('done')},{dcy} L{NUDGE_X},{dcy} L{NUDGE_X},{sm} L{BX + BW},{sm}", "loop")
nudge_cx = (lft("done") + NUDGE_X) // 2
o(f'  <text class="l" x="{nudge_cx}" y="{sm - 8}" text-anchor="middle" fill="#a33">'
  f'checks red: inject nudge (≤2), continue</text>')

o("</svg>")

print("\n".join(buf))
