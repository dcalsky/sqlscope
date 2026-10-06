// The Node.js entry point: loads the WebAssembly module on import.

import { readFileSync } from "node:fs";

import { initSync } from "./engine.js";

initSync(readFileSync(new URL("../wasm/sqlscope_wasm_bg.wasm", import.meta.url)));

export * from "./index.js";
