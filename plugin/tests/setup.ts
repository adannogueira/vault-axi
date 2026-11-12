import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { loadCore } from "../src/core";

loadCore(readFileSync(fileURLToPath(new URL("../wasm/core_bg.wasm", import.meta.url))));
