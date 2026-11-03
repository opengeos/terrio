// Runs inside a fresh module worker; no conformance memory.grow in these samples.
self.onmessage = async ({ data: { moduleUrl, wasmUrl, bytes, runs } }) => {
  try {
    const start = performance.now();
    const terrio = await import(moduleUrl);
    const wasm = await terrio.default({ module_or_path: wasmUrl });
    const initializationMs = performance.now() - start;
    const counters = () => ({
      wasmCapacityBytes: wasm.memory.buffer.byteLength,
      jsHeapUsedBytes: performance.memory?.usedJSHeapSize ?? null,
      jsHeapTotalBytes: performance.memory?.totalJSHeapSize ?? null,
    });
    const before = counters();
    const samples = [];
    const warmInitializationMs = [];
    for (let i = 0; i < runs; i++) {
      const initStart = performance.now();
      const cached = await import(moduleUrl);
      await cached.default({ module_or_path: wasmUrl });
      warmInitializationMs.push(performance.now() - initStart);
      let ds, array;
      const t = performance.now();
      try {
        ds = terrio.openMultidim(bytes);
        array = ds.array("sample");
        const values = array.read();
        if (!(values instanceof Int16Array) || values.length !== 24 || values[23] !== 15)
          throw new Error("benchmark decode mismatch");
      } finally { array?.free(); ds?.free(); }
      samples.push(performance.now() - t);
    }
    self.postMessage({ initializationMs, warmInitializationMs, samples, before, after: counters() });
  } catch (error) { self.postMessage({ error: String(error) }); }
};
