// The kaish engine lives in this worker: the main thread stays responsive
// while the kernel runs, and Ctrl-C can terminate + respawn the worker
// without taking the page down. Messages are processed strictly in order
// (the worker is single-threaded and execute() is synchronous), so command
// results always come back FIFO.
import init, { KaishShell } from './pkg/kaish_web.js';

let shell = null;

self.onmessage = async (ev) => {
  const msg = ev.data;
  try {
    if (msg.type === 'boot') {
      const t0 = performance.now();
      await init();
      shell = new KaishShell();
      console.log('[kaish-worker] booted');
      postMessage({ type: 'ready', ms: performance.now() - t0 });
    } else if (msg.type === 'seed') {
      const t0 = performance.now();
      let bytes = 0;
      for (const [path, data] of msg.files) {
        shell.seed_file(path, data);
        bytes += data.length;
      }
      postMessage({
        type: 'seeded',
        count: msg.files.length,
        bytes,
        ms: performance.now() - t0,
      });
    } else if (msg.type === 'exec') {
      const r = JSON.parse(shell.execute(msg.line));
      postMessage({ type: 'result', id: msg.id, ...r });
    } else if (msg.type === 'complete') {
      const r = JSON.parse(shell.complete(msg.line, msg.pos));
      postMessage({ type: 'completions', id: msg.id, ...r });
    }
  } catch (e) {
    // A wasm panic poisons the instance; report and let the main thread
    // respawn us.
    postMessage({ type: 'crash', id: msg.id, error: String(e) });
  }
};
