import { conformance } from "./scientific-conformance.mjs";
self.onmessage = async ({ data: { id, moduleUrl, wasmUrl, bytes } }) => {
  try {
    const terrio = await import(moduleUrl);
    const wasm = await terrio.default({ module_or_path: wasmUrl });
    const result = conformance(terrio, bytes, wasm.memory);
    self.postMessage({ id, result }, [result.copied.buffer]);
    if (result.copied.byteLength !== 0) throw new Error("worker result did not detach");
    self.postMessage({ id, detached: true });
  } catch (error) { self.postMessage({ id, error: String(error) }); }
};
