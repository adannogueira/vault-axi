import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export default readFileSync(fileURLToPath(new URL("../wasm/core_bg.wasm", import.meta.url)));
