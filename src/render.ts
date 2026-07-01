export const PREVIEW_LIMIT = 1200;

export interface Truncated {
  content: string;
  truncated: boolean;
  totalChars: number;
}

/** Truncate long text with an explicit indicator, per AXI content-truncation. */
export function truncateText(text: string, limit = PREVIEW_LIMIT): Truncated {
  if (text.length <= limit) {
    return { content: text, truncated: false, totalChars: text.length };
  }
  return {
    content: `${text.slice(0, limit)}\n… [truncated]`,
    truncated: true,
    totalChars: text.length,
  };
}
