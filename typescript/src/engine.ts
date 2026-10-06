// Runs operations on the sqlscope WebAssembly module.

import initWasm, { call as wasmCall, initSync as initWasmSync, version as wasmVersion } from "../wasm/sqlscope_wasm.js";
import type { InitInput, SyncInitInput } from "../wasm/sqlscope_wasm.js";

/** Base class of every sqlscope error. */
export class SqlscopeError extends Error {
  override name = "SqlscopeError";
}

/** An option or argument is invalid. */
export class InvalidArgumentError extends SqlscopeError {
  override name = "InvalidArgumentError";
}

/** The SQL text could not be parsed. */
export class ParseError extends SqlscopeError {
  override name = "ParseError";
}

/** The statement shape is not supported, or the input exceeded a safety limit. */
export class UnsupportedError extends SqlscopeError {
  override name = "UnsupportedError";
}

/** sqlscope produced an invalid result or crashed (a bug). */
export class InternalError extends SqlscopeError {
  override name = "InternalError";
}

const ERRORS: Record<string, new (message: string) => SqlscopeError> = {
  invalid_argument: InvalidArgumentError,
  parse: ParseError,
  unsupported: UnsupportedError,
};

// The public types avoid DOM types (BufferSource, Response, ...) so that
// Node.js projects without the DOM lib can type-check against them.

/** The bytes of the WebAssembly module. */
export type WasmBytes = ArrayBuffer | ArrayBufferView;

/** A `Response` to the WebAssembly module, such as `fetch(url)` returns. */
export interface WasmResponse {
  readonly url: string;
  arrayBuffer(): Promise<ArrayBuffer>;
}

/** A compiled `WebAssembly.Module`. */
export type WasmModule = typeof globalThis extends { WebAssembly: { Module: abstract new (...args: never) => infer M } }
  ? M
  : never;

/** What {@link init} accepts: a URL or response to fetch, the bytes, or a compiled module. */
export type WasmSource = string | URL | WasmResponse | WasmBytes | WasmModule;

let ready = false;
let pending: Promise<void> | undefined;

/**
 * Load the WebAssembly module. Resolves immediately when it is loaded.
 *
 * Under Node.js (and Deno and Bun) the module is loaded when the package is
 * imported, so calling `init` is never needed. Elsewhere, await it once before
 * any other call. Without `source` it fetches `sqlscope.wasm` next to the
 * package, which bundlers resolve; pass a URL or the bytes to serve it from
 * somewhere else.
 */
export function init(source?: WasmSource | Promise<WasmSource>): Promise<void> {
  if (ready) return Promise.resolve();
  const input = source === undefined ? undefined : { module_or_path: source as InitInput | Promise<InitInput> };
  pending ??= initWasm(input).then(
    () => {
      ready = true;
    },
    (error: unknown) => {
      pending = undefined;
      throw error;
    },
  );
  return pending;
}

/** Load the WebAssembly module synchronously from its bytes or a compiled module. */
export function initSync(source: WasmBytes | WasmModule): void {
  if (ready) return;
  initWasmSync({ module: source as SyncInitInput });
  ready = true;
}

function engine(): void {
  if (!ready) {
    throw new SqlscopeError("sqlscope is not initialized: await init() before calling it");
  }
}

/** The sqlscope version of the WebAssembly module ("X.Y.Z"). */
export function version(): string {
  engine();
  return wasmVersion();
}

type Response = { ok: unknown } | { error: { kind: string; message: string } };

/** Run one operation and return its decoded result, throwing on error. */
export function call(operation: string, request: object): unknown {
  engine();
  let text: string;
  try {
    text = wasmCall(operation, JSON.stringify(request));
  } catch (error) {
    // A panic traps; the module may be unusable afterwards.
    if (error instanceof WebAssembly.RuntimeError) {
      throw new InternalError(`sqlscope crashed: ${error.message}`, { cause: error });
    }
    throw error;
  }
  const response = JSON.parse(text) as Response;
  if ("error" in response) {
    const { kind, message } = response.error;
    throw new (ERRORS[kind] ?? InternalError)(message);
  }
  return response.ok;
}
