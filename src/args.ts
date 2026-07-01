import { readFileSync } from "node:fs";
import { AxiError } from "axi-sdk-js";

export interface ParsedArgs {
  positionals: string[];
  flags: Record<string, string | boolean | string[]>;
}

/**
 * Minimal flag parser. Supports `--key value`, `--key=value`, boolean `--flag`,
 * and repeated `--key` (collected into an array). Everything else is positional.
 */
export function parseArgs(args: string[]): ParsedArgs {
  const positionals: string[] = [];
  const flags: Record<string, string | boolean | string[]> = {};

  for (let i = 0; i < args.length; i++) {
    const arg = args[i]!;
    if (arg === "--") {
      positionals.push(...args.slice(i + 1));
      break;
    }
    if (arg.startsWith("--")) {
      const body = arg.slice(2);
      const eq = body.indexOf("=");
      let key: string;
      let value: string | boolean;
      if (eq !== -1) {
        key = body.slice(0, eq);
        value = body.slice(eq + 1);
      } else {
        key = body;
        const next = args[i + 1];
        if (next !== undefined && !next.startsWith("--")) {
          value = next;
          i++;
        } else {
          value = true;
        }
      }
      const existing = flags[key];
      if (existing === undefined) {
        flags[key] = value;
      } else if (Array.isArray(existing)) {
        existing.push(String(value));
      } else {
        flags[key] = [String(existing), String(value)];
      }
    } else {
      positionals.push(arg);
    }
  }

  return { positionals, flags };
}

export function flagString(flags: ParsedArgs["flags"], key: string): string | undefined {
  const v = flags[key];
  if (v === undefined || typeof v === "boolean") return undefined;
  return Array.isArray(v) ? v[v.length - 1] : v;
}

export function flagBool(flags: ParsedArgs["flags"], key: string): boolean {
  return flags[key] === true || flags[key] === "true";
}

export function flagInt(flags: ParsedArgs["flags"], key: string, fallback: number): number {
  const s = flagString(flags, key);
  if (s === undefined) return fallback;
  const n = Number.parseInt(s, 10);
  return Number.isFinite(n) ? n : fallback;
}

/**
 * Resolve content for write/update commands: from `--content`, or from stdin
 * when `--content -` or `--stdin` is given. Throws a usage error otherwise.
 */
export function resolveContent(flags: ParsedArgs["flags"]): string {
  const content = flagString(flags, "content");
  if (content !== undefined && content !== "-") return content;
  if (content === "-" || flagBool(flags, "stdin")) {
    return readStdin();
  }
  throw new AxiError("No content provided", "VALIDATION_ERROR", [
    "Pass --content \"text\", or --content - / --stdin to read from stdin",
  ]);
}

export function readStdin(): string {
  try {
    return readFileSync(0, "utf8");
  } catch {
    return "";
  }
}
