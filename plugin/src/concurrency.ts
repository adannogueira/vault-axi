// Bounded concurrency and cooperative main-thread yielding so Obsidian's UI stays responsive.

const DEFAULT_FRAME_BUDGET_MS = 16;

function nowMs(): number {
  return typeof performance !== "undefined" ? performance.now() : Date.now();
}

/**
 * Yield a macrotask so the browser can paint pending status updates and process user input.
 * Uses `MessageChannel` (same technique as React's scheduler) to avoid `setTimeout`'s 4ms clamp.
 */
export function yieldToMain(): Promise<void> {
  if (typeof MessageChannel !== "undefined") {
    return new Promise((resolve) => {
      const channel = new MessageChannel();
      channel.port1.onmessage = () => {
        channel.port1.close();
        channel.port2.close();
        resolve();
      };
      channel.port2.postMessage(undefined);
    });
  }
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

/**
 * Returns an async checkpoint function that yields to the event loop whenever more than
 * `budgetMs` has elapsed since the previous yield.
 */
export function createFrameYielder(budgetMs = DEFAULT_FRAME_BUDGET_MS): () => Promise<void> {
  let last = nowMs();
  return async () => {
    if (nowMs() - last >= budgetMs) {
      await yieldToMain();
      last = nowMs();
    }
  };
}

/** Run `worker` over `items` with at most `concurrency` executions in flight at once. */
export async function runConcurrent<T>(
  items: readonly T[],
  concurrency: number,
  worker: (item: T, index: number) => Promise<void>,
): Promise<void> {
  if (items.length === 0) return;
  const limit = Math.max(1, Math.min(concurrency, items.length));
  let next = 0;
  let failed = false;
  const runWorker = async (): Promise<void> => {
    while (!failed) {
      const index = next++;
      if (index >= items.length) return;
      try {
        await worker(items[index] as T, index);
      } catch (e) {
        failed = true;
        throw e;
      }
    }
  };
  await Promise.all(Array.from({ length: limit }, () => runWorker()));
}
