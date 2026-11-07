#!/usr/bin/env python3
"""Bin every distinct command in replay.jsonl and write bins.jsonl + tables.md.

Bins: WORKS, USELESS-ERROR, SILENT-WRONG, USEFUL-REFUSAL, NOT-ALLOWED-BY-DESIGN.
WORKS means same effect as real curl on exit code, files written, and the
meaning of stdout. Cosmetic stdout divergences are tagged (C-*) and counted
separately; they do not change the bin. Rules below were checked against the
printed output of every distinct command (see the report).

  C-hdr    -i/-I: header names lowercase and sorted, status line has no reason phrase
  C-wrote  -o: prints "Wrote N bytes to PATH" on stdout (no trailing newline)
  C-cl0    request without a body sends "content-length: 0"
  C-ua     default User-Agent is kaish-curl, not curl/x.y
  C-enc    --data-urlencode encodes a space as %20, curl as +
"""
import json, re, collections
R = [json.loads(l) for l in open("replay.jsonl")]
resp = [json.loads(l) for l in open("responses.jsonl")]
cmd2 = {r["cmd"]: r for r in R}


def norm_echo(s):
    try:
        d = json.loads(s)
    except Exception:
        return None
    h = d.get("headers")
    if isinstance(h, dict):
        if h.get("user-agent", "").startswith(("kaish-curl", "curl/")):
            h.pop("user-agent")
        if h.get("content-length") == "0":
            h.pop("content-length")
    if isinstance(d.get("body"), str) and d.get("content_type") == "application/x-www-form-urlencoded":
        d["body"] = d["body"].replace("%20", "+")
        d["headers"].pop("content-length", None)
    return d


def strip_hdr(s):
    # keep only the body after the blank line; header block compared separately
    return s.split("\r\n\r\n", 1)[-1] if "\r\n\r\n" in s else s


def classify(cmd, k, u):
    tags = []
    same = k["stdout"] == u["stdout"] and k["exit"] == u["exit"] and k["files"] == u["files"]
    if same:
        return "WORKS", tags, ""
    ke, ue = k["stderr"], u["stderr"]
    # --- refusals -------------------------------------------------------
    if k["exit"] == 127 and "time:" in ke + k["stdout"]:
        return "USELESS-ERROR", ["kaish"], "kaish has no `time`; message says 'external commands are not available'"
    if "'@path' body syntax" in ke:
        return "USELESS-ERROR", [], "refusal's next step (`--data-binary -`) sends the literal body '-', not stdin"
    if "is not a flag curl accepts here" in ke:
        flag = re.search(r"'(-[^']*)'", ke).group(1)
        return "USELESS-ERROR", [], f"unknown flag {flag}: names no replacement"
    if "'-O' is not supported" in ke:
        return "NOT-ALLOWED-BY-DESIGN", [], "docs/issues.md CU20 (-O dropped); message names -o"
    if "'-w' is not supported" in ke:
        return "NOT-ALLOWED-BY-DESIGN", [], "docs/curl.md Literate errors (--write-out), CU5; message names --json"
    if "'--retry' is not supported" in ke:
        return "NOT-ALLOWED-BY-DESIGN", [], "docs/curl.md Literate errors (--retry), CU5; message names a shell loop"
    if "'-F' (multipart)" in ke:
        return "NOT-ALLOWED-BY-DESIGN", [], "docs/curl.md Literate errors (--form), CU5; message names --data, which cannot send a file"
    if "'-G' is not supported" in ke or "'--get' is not supported" in ke:
        return "NOT-ALLOWED-BY-DESIGN", [], "docs/curl.md Literate errors (--get), CU5; message says to put the query in the URL"
    if "'-v' is not supported" in ke or "'-v' is not supported" in k["stdout"]:
        return "NOT-ALLOWED-BY-DESIGN", [], "docs/curl.md Literate errors (--verbose), CU5; message names -i"
    # --- exit 0 but different --------------------------------------------
    if "| jq" in cmd and k["stdout"] == "null\n" and u["stdout"] != "null\n":
        return "SILENT-WRONG", [], "SW-1 jq receives the structured response object, prints null"
    if "split(\" \")[1]'" in cmd and k["stdout"] == "404\r\n":
        return "SILENT-WRONG", [], "SW-2 status line has no reason phrase, so the number keeps a trailing CR"
    if re.search(r"Wrote \d+ bytes to '[^']+'[0-9]", k["stdout"]):
        return "SILENT-WRONG", [], "SW-3 -o confirmation, with no newline, prepended to a computed value"
    # --- cosmetic divergences -------------------------------------------
    ok = k["exit"] == u["exit"] and k["files"] == u["files"]
    if ok:
        if "-i" in cmd.split() or "-I" in cmd.split() or re.search(r" -s?[iI] ", cmd) or " -sI " in cmd or " -si " in cmd:
            if strip_hdr(k["stdout"]) == strip_hdr(u["stdout"]) or cmd.startswith("curl -I") or " -I " in cmd or " -sI " in cmd:
                return "WORKS", ["C-hdr"], ""
        if re.search(r"-o \S+", k["stdout"] and cmd) and re.match(r"Wrote \d+ bytes to '[^']+'$", k["stdout"]):
            return "WORKS", ["C-wrote"], ""
        if k["stdout"].startswith("Wrote ") and k["stdout"].split("\n", 1)[-1] == "" or re.match(r"Wrote \d+ bytes to '[^']+'$", k["stdout"]):
            return "WORKS", ["C-wrote"], ""
        a, b = norm_echo(k["stdout"]), norm_echo(u["stdout"])
        if a is not None and a == b:
            t = ["C-ua"]
            if json.loads(k["stdout"]).get("headers", {}).get("content-length") == "0":
                t.append("C-cl0")
            if "data-urlencode" in cmd and "%20" in k["stdout"]:
                t.append("C-enc")
            return "WORKS", t, ""
    return "UNCLASSIFIED", [], ""


out = []
for r in R:
    b, t, why = classify(r["cmd"], r["kaish"], r["curl"])
    out.append({"id": r["id"], "cmd": r["cmd"], "bin": b, "tags": t, "why": why, "uses": r["uses"]})
with open("bins.jsonl", "w") as f:
    for o in out:
        f.write(json.dumps(o) + "\n")

# ---- tables ---------------------------------------------------------------
TASKS = {int(a): b for a, b in (l.rstrip("\n").split("\t", 1) for l in open("tasks.txt"))}
bin_of = {o["cmd"]: o for o in out}
BINS = ["WORKS", "USELESS-ERROR", "SILENT-WRONG", "USEFUL-REFUSAL", "NOT-ALLOWED-BY-DESIGN"]
runs = sorted({r["run"] for r in resp})
cells = collections.Counter()
by_arm = collections.defaultdict(collections.Counter)
cos = collections.Counter()
for r in resp:
    o = bin_of[r["command"]]
    cells[o["bin"]] += 1
    by_arm[r["arm"]][o["bin"]] += 1
    for t in o["tags"]:
        cos[t] += 1
by_run = collections.defaultdict(collections.Counter)
for r in resp:
    by_run[r["run"]][bin_of[r["command"]]["bin"]] += 1
lines = []
w = lines.append
w("## Totals (cells = one first command per run x task)\n")
w(f"{len(resp)} cells from {len(runs)} runs x 28 tasks; {len(out)} distinct command lines.\n")
w("| bin | cells | cold (6 runs) | warm (6 runs) | distinct spellings |")
w("|---|---|---|---|---|")
dist = collections.Counter(o["bin"] for o in out)
for b in BINS:
    w(f"| {b} | {cells[b]} | {by_arm['cold'][b]} | {by_arm['warm'][b]} | {dist[b]} |")
w(f"| total | {sum(cells.values())} | {sum(by_arm['cold'].values())} | {sum(by_arm['warm'].values())} | {len(out)} |\n")
w("Cosmetic divergences inside WORKS cells (tag: cells): " + ", ".join(f"{k}: {v}" for k, v in sorted(cos.items())) + ".\n")
w("### Per run\n")
w("| run | " + " | ".join(BINS) + " |")
w("|---|" + "---|" * len(BINS))
for run in runs:
    w(f"| {run} | " + " | ".join(str(by_run[run][b]) for b in BINS) + " |")
w("")
w("## Spellings per task (runs that typed it -> bin; tags in brackets)\n")
for t in sorted(TASKS):
    w(f"### T{t}. {TASKS[t]}\n")
    cnt = collections.defaultdict(lambda: collections.Counter())
    for r in resp:
        if r["task"] == t:
            cnt[r["command"]][r["arm"]] += 1
    rows = sorted(cnt.items(), key=lambda kv: -sum(kv[1].values()))
    for cmdline, c in rows:
        o = bin_of[cmdline]
        tag = f" [{' '.join(o['tags'])}]" if o["tags"] else ""
        short = cmdline if len(cmdline) <= 150 else cmdline[:147] + "..."
        w(f"- {sum(c.values())} (cold {c['cold']}, warm {c['warm']}) **{o['bin']}**{tag}: `{short}`".replace("|", "\\|"))
    w("")
open("tables.md", "w").write("\n".join(lines))
sw = collections.Counter()
for r in resp:
    o = bin_of[r["command"]]
    if o["bin"] in ("SILENT-WRONG", "USELESS-ERROR"):
        sw[(o["bin"], o["why"])] += 1
for (b, why), n in sorted(sw.items(), key=lambda kv: -kv[1]):
    print(n, b, why)

if __name__ == "__main__":
    un = [o for o in out if o["bin"] == "UNCLASSIFIED"]
    print("distinct", collections.Counter(o["bin"] for o in out))
    for o in un:
        print("UNCLASSIFIED", o["id"], o["cmd"][:150])
