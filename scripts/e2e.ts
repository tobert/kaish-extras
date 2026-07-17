// Real-time e2e for the try-kaish page: drives headless Chromium over raw CDP
// (no dependencies). The page's kaish engine lives in a Web Worker, which
// --virtual-time-budget can't wait on — this harness polls in real time.
//
// Usage: deno run --allow-net --allow-run scripts/e2e.ts [url]
//   default url: http://127.0.0.1:8137/

const url = Deno.args[0] ?? "http://127.0.0.1:8137/";
const port = 9223;

const chrome = new Deno.Command("chromium", {
  args: [
    "--headless=new", "--disable-gpu", "--no-sandbox",
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
for (let i = 0; i < 50 && !target; i++) {
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

// 3. Ctrl-C interrupts a runaway loop: worker restarts and reseeds.
await type("while true; do true; done");
await new Promise((r) => setTimeout(r, 600));   // let it really wedge the worker
await ctrlC();
await waitFor("interrupt notice", async () => (await screenText()).includes("interrupted"));
await waitFor("reseed after interrupt", async () => {
  const t = await screenText();
  return t.indexOf("seeded", t.indexOf("interrupted")) > -1;
});
await type("echo back-alive");
await waitFor("shell alive after ^C", async () => (await screenText()).includes("back-alive"));
console.log("ctrl-c interrupt + restart: OK");

console.log("E2E OK");
try { chrome.kill(); } catch { /* already gone */ }
Deno.exit(0);
