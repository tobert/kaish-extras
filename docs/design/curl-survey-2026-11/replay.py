#!/usr/bin/env python3
"""Replay every distinct command from responses.jsonl through (a) the kaish
probe (examples/probe.rs) and (b) real curl under bash, against server.py.

    replay.py <probe-binary> > replay.jsonl     (server.py must be on :8765)

Each command runs after a /reset of the server counters, in a fresh directory
seeded with the same fixtures the probe seeds (keep in step with probe.rs).
"""
import json, os, subprocess, sys, tempfile, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = {
    "payload.json": '{"name":"widget","qty":3}\n',
    "upload.txt": "hello upload\n",
    "token.txt": "abc123\n",
    "partial.txt": "0123456789abcdef\n" * 3,
}
probe = sys.argv[1]


def reset():
    urllib.request.urlopen("http://127.0.0.1:8765/reset").read()


def real(cmd):
    reset()
    with tempfile.TemporaryDirectory() as d:
        for n, b in FIXTURES.items():
            open(os.path.join(d, n), "w").write(b)
        try:
            p = subprocess.run(["bash", "-c", cmd], cwd=d, capture_output=True, timeout=30,
                               env={"PATH": os.environ["PATH"], "HOME": d})
            out, err, code = p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace"), p.returncode
        except subprocess.TimeoutExpired:
            out, err, code = "", "[harness timeout]", -9
        files = {}
        for n in sorted(os.listdir(d)):
            b = open(os.path.join(d, n), encoding="utf-8", errors="replace").read()
            if FIXTURES.get(n) != b:
                files[n] = b
        return {"stdout": out, "stderr": err, "exit": code, "files": files}


def kaish(cmd):
    reset()
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as f:
        f.write(json.dumps({"id": 0, "cmd": cmd}) + "\n")
    try:
        p = subprocess.run([probe, "run", f.name], capture_output=True, timeout=60)
        line = p.stdout.decode().strip().splitlines()
        if not line:
            return {"stdout": "", "stderr": "[probe crashed] " + p.stderr.decode()[-500:], "exit": -2, "files": {}}
        r = json.loads(line[-1])
        return {k: r[k] for k in ("stdout", "stderr", "exit", "files")}
    finally:
        os.unlink(f.name)


seen = {}
for line in open(os.path.join(HERE, "responses.jsonl")):
    r = json.loads(line)
    seen.setdefault(r["command"], []).append((r["run"], r["task"]))
for i, (cmd, uses) in enumerate(seen.items()):
    print(json.dumps({"id": i, "cmd": cmd, "uses": uses, "kaish": kaish(cmd), "curl": real(cmd)}), flush=True)
