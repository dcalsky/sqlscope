// The runtime-neutral entry point, as bundlers and browsers load it.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import * as sqlscope from "../dist/index.js";

test("needs init, then works", async () => {
  assert.throws(() => sqlscope.outputColumns("SELECT a FROM t"), /await init\(\)/);
  const bytes = readFileSync(new URL("../wasm/sqlscope_wasm_bg.wasm", import.meta.url));
  await Promise.all([sqlscope.init(bytes), sqlscope.init(bytes)]);
  assert.deepEqual(sqlscope.outputColumns("SELECT a FROM t"), ["a"]);
  await sqlscope.init();
});
