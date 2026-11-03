// Minimal single-threaded module-worker example. No isolation headers required.
// Send {id, moduleUrl, wasmUrl, bytes: Uint8Array}; transfer bytes.buffer if desired.
// The moduleUrl must identify a wasm-bindgen --target web module (not load.js).
self.onmessage = async ({ data: { id, moduleUrl, wasmUrl, bytes } }) => {
  let dataset, array, child;
  try {
    const terrio = await import(moduleUrl);
    await terrio.default({ module_or_path: wasmUrl });
    dataset = terrio.openMultidim(bytes);
    array = dataset.array("sample");
    child = array.isel("time", 1);
    const shape = Array.from(child.shape, Number);
    const values = child.read(); // JS-owned copy: safe to transfer after free().
    child.free(); child = null;
    array.free(); array = null;
    dataset.free(); dataset = null;
    self.postMessage({ id, shape, values }, [values.buffer]);
  } catch (error) {
    self.postMessage({ id, error: String(error) });
  } finally {
    child?.free(); array?.free(); dataset?.free();
  }
};
