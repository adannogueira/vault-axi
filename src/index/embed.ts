/**
 * Local sentence embeddings via transformers.js. Loaded lazily so non-index
 * commands never pay the startup cost, and fully optional: if the model can't
 * load (offline first run, disabled via env), the vault still works on FTS5 +
 * the wikilink graph.
 */

export const DEFAULT_MODEL = "TaylorAI/bge-micro-v2";
export const EMBED_DIM = 384;
const MAX_INPUT_CHARS = 2000;

type FeatureExtractor = (
  texts: string[],
  opts: { pooling: "mean"; normalize: boolean },
) => Promise<{ tolist: () => number[][] }>;

let pipe: FeatureExtractor | null = null;
let initTried = false;
let available = false;

export function embeddingsDisabled(): boolean {
  return process.env.VAULT_AXI_NO_EMBED === "1";
}

export function modelName(): string {
  return process.env.VAULT_AXI_EMBED_MODEL || DEFAULT_MODEL;
}

/** Attempt to load the model. Returns false if embeddings are unavailable. */
export async function initEmbedder(): Promise<boolean> {
  if (initTried) return available;
  initTried = true;
  if (embeddingsDisabled()) {
    available = false;
    return false;
  }
  try {
    const { pipeline, env } = await import("@huggingface/transformers");
    // Keep everything local; never hit a hub token path.
    env.allowLocalModels = true;
    const extractor = (await pipeline("feature-extraction", modelName())) as unknown as FeatureExtractor;
    pipe = extractor;
    available = true;
  } catch {
    available = false;
    pipe = null;
  }
  return available;
}

export function isAvailable(): boolean {
  return available;
}

/** Embed a batch of texts into normalized Float32 vectors, or null if unavailable. */
export async function embed(texts: string[]): Promise<Float32Array[] | null> {
  if (!pipe) {
    const ok = await initEmbedder();
    if (!ok || !pipe) return null;
  }
  const clipped = texts.map((t) => t.slice(0, MAX_INPUT_CHARS));
  const output = await pipe!(clipped, { pooling: "mean", normalize: true });
  return output.tolist().map((row) => Float32Array.from(row));
}

export function toBlob(vec: Float32Array): Buffer {
  return Buffer.from(vec.buffer, vec.byteOffset, vec.byteLength);
}

export function fromBlob(buf: Buffer): Float32Array {
  return new Float32Array(buf.buffer, buf.byteOffset, buf.byteLength / 4);
}

/** Cosine similarity for already-normalized vectors reduces to a dot product. */
export function cosine(a: Float32Array, b: Float32Array): number {
  const n = Math.min(a.length, b.length);
  let dot = 0;
  for (let i = 0; i < n; i++) dot += a[i]! * b[i]!;
  return dot;
}
