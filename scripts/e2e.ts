// Real-time e2e for the try-kaish page: drives headless Chromium over raw CDP
// (no dependencies). The page's kaish engine lives in a Web Worker, which
// --virtual-time-budget can't wait on — this harness polls in real time.
//
// Usage: deno run --allow-net --allow-run scripts/e2e.ts [url]
//   default url: http://127.0.0.1:8137/

const url = Deno.args[0] ?? "http://127.0.0.1:8137/";
const port = 9223;

function findBrowser(): string {
  for (const c of ["chromium", "google-chrome-stable", "google-chrome", "chromium-browser"]) {
    try {
      new Deno.Command(c, { args: ["--version"], stdout: "null", stderr: "null" })
        .outputSync();
      return c;
    } catch { /* keep looking */ }
  }
  console.error("E2E FAIL: no chromium/chrome binary found");
  Deno.exit(1);
}

const chrome = new Deno.Command(findBrowser(), {
  args: [
    "--headless=new", "--disable-gpu", "--no-sandbox",
    // Branded Chrome (136+) silently ignores remote debugging on the
    // default profile — a throwaway user-data-dir is mandatory. Chrome
    // creates the directory itself.
    `--user-data-dir=/tmp/kaish-e2e-profile-${port}`,
    `--remote-debugging-port=${port}`, "about:blank",
  ],
  stdout: "null", stderr: "null",
}).spawn();

function fail(msg: string): never {
  console.error(`E2E FAIL: ${msg}`);
  try { chrome.kill(); } catch { /* already gone */ }
  Deno.exit(1);
}

// Wait for the debugger endpoint, then a page target.
let target: { webSocketDebuggerUrl: string } | undefined;
for (let i = 0; i < 150 && !target; i++) {
  await new Promise((r) => setTimeout(r, 200));
  try {
    const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
    target = targets.find((t: { type: string }) => t.type === "page");
  } catch { /* not up yet */ }
}
if (!target) fail("chromium debugger endpoint never appeared");

const ws = new WebSocket(target!.webSocketDebuggerUrl);
let nextId = 0;
const replies = new Map<number, (v: unknown) => void>();
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id !== undefined) replies.get(m.id)?.(m);
};
await new Promise((r) => (ws.onopen = r));

function send(method: string, params: object = {}): Promise<unknown> {
  const id = ++nextId;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise((resolve) => replies.set(id, resolve));
}
// deno-lint-ignore no-explicit-any
async function evalJs(expression: string): Promise<any> {
  const m = (await send("Runtime.evaluate", {
    expression, returnByValue: true,
  })) as { result?: { result?: { value?: unknown } } };
  // deno-lint-ignore no-explicit-any
  return (m as any).result?.result?.value;
}
async function screenText(): Promise<string> {
  return (await evalJs(`document.getElementById('screen').innerText`)) ?? "";
}
async function waitFor(desc: string, pred: () => Promise<boolean>, ms = 30000) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    if (await pred()) return;
    await new Promise((r) => setTimeout(r, 250));
  }
  fail(`timeout waiting for: ${desc}\n--- screen ---\n${await screenText()}`);
}
async function type(line: string) {
  await evalJs(`(() => {
    const c = document.getElementById('cmdline');
    c.value = ${JSON.stringify(line)};
    c.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
  })()`);
}
async function ctrlC() {
  await evalJs(`document.getElementById('cmdline').dispatchEvent(
    new KeyboardEvent('keydown', { key: 'c', ctrlKey: true, bubbles: true }))`);
}

await send("Page.enable");
await send("Page.navigate", { url });

// 1. Boot + seed through the worker.
await waitFor("seeded banner", async () => (await screenText()).includes("seeded"));
console.log("boot+seed: OK");

// 2. Commands run through the worker, FIFO.
await type("uname -a");
await type("grep -c 'pub fn' /src/kaish/crates/kaish-types/src/clock.rs");
await waitFor("uname output", async () => (await screenText()).includes("kaish"));
await waitFor("grep output", async () => /clock\.rs|\n1\n/.test(await screenText()));
console.log("exec via worker: OK");

// 3. Tab completion: unique command completes with a trailing space; a
//    path prefix extends to the candidates' common prefix via the VFS.
async function tab(line: string) {
  await evalJs(`(() => {
    const c = document.getElementById('cmdline');
    c.value = ${JSON.stringify(line)};
    c.setSelectionRange(c.value.length, c.value.length);
    c.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true }));
  })()`);
}
async function cmdlineValue(): Promise<string> {
  return (await evalJs(`document.getElementById('cmdline').value`)) ?? "";
}
await tab("unam");
await waitFor("command completion 'unam' -> 'uname '",
  async () => (await cmdlineValue()) === "uname ");
await tab("cat /src/ka");
await waitFor("path LCP '/src/ka' -> '/src/kaish'",
  async () => (await cmdlineValue()) === "cat /src/kaish");
await tab("echo --no-ne");
await waitFor("flag completion 'echo --no-ne' -> '--no-newline '",
  async () => (await cmdlineValue()) === "echo --no-newline ");
await evalJs(`document.getElementById('cmdline').value = ''`);
console.log("tab completion: OK");

// 4. Ctrl-C interrupts a runaway loop. Under cross-origin isolation (the
//    coi service worker provides it even on GitHub Pages) this is tier 2:
//    in-place kernel interrupt, exit 130, session state preserved. Without
//    isolation it falls back to tier 1: worker restart + reseed.
const isolated = await evalJs("window.crossOriginIsolated === true");
if (isolated) {
  await type("KEEP=preserved-7x9");
  await type("while true; do true; done");
  await new Promise((r) => setTimeout(r, 600)); // let it really wedge the worker
  await ctrlC();
  await waitFor("exit 130 after tier-2 ^C",
    async () => (await screenText()).includes("exit 130"));
  await type("echo $KEEP");
  await waitFor("session state preserved across ^C",
    async () => /\npreserved-7x9\n/.test(await screenText()));
  if ((await screenText()).includes("shell restarted")) {
    fail("tier-2 ^C restarted the shell instead of interrupting in place");
  }
  console.log("ctrl-c tier-2 (in-place, state preserved): OK");
} else {
  await type("while true; do true; done");
  await new Promise((r) => setTimeout(r, 600));
  await ctrlC();
  await waitFor("interrupt notice", async () => (await screenText()).includes("interrupted"));
  await waitFor("reseed after interrupt", async () => {
    const t = await screenText();
    return t.indexOf("seeded", t.indexOf("interrupted")) > -1;
  });
  await type("echo back-alive");
  await waitFor("shell alive after ^C", async () => (await screenText()).includes("back-alive"));
  console.log("ctrl-c tier-1 (restart fallback): OK");
}

console.log("E2E OK");
try { chrome.kill(); } catch { /* already gone */ }
Deno.exit(0);
