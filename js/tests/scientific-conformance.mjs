// Shared by the browser page and module worker. Exact native values, no CF decoding.
export function conformance(terrio, bytes, memory) {
  const check = (ok, message) => { if (!ok) throw new Error(message); };
  const equal = (actual, expected, message) => check(actual.length === expected.length && actual.every((v, i) => Object.is(v, expected[i])), message);
  const rejects = (fn, message) => { let failed = false; try { fn(); } catch { failed = true; } check(failed, message); };
  const handles = [];
  const own = (h) => { handles.push(h); return h; };
  const release = (h) => { handles.splice(handles.indexOf(h), 1); h.free(); };
  const start = performance.now();
  try {
    const ds = own(terrio.openMultidim(bytes));
    check(ds.info().root.attributes.title === "Synthetic numeric conformance", "group attributes");
    check(ds.arrayPaths.includes("/sample"), "array discovery");
    const a = own(ds.array("/sample"));
    equal(Array.from(a.shape, Number), [2, 3, 4], "shape");
    equal(a.dimensionNames, ["time", "y", "x"], "dimension order");
    const info = a.info();
    equal(info.chunks, [1, 2, 2], "source chunks");
    check(info.attributes.scale_factor === 0.25 && info.attributes.add_offset === 100 && info.attributes._FillValue === -32768 && info.attributes.units === "K", "packing attributes");
    const time = own(ds.array("time"));
    check(time.info().attributes.calendar === "standard", "calendar");
    equal([...time.read()], [0, 1.5], "coordinate values");
    const expected = Array.from({ length: 24 }, (_, i) => i === 0 ? -32768 : i - 8);
    const copied = a.read();
    check(copied instanceof Int16Array, "native Int16");
    equal([...copied], expected, "raw packed values including fill");
    const again = a.read();
    check(again.buffer !== copied.buffer, "reads own distinct JS buffers");
    again[0] = 123;
    equal([...a.read()], expected, "mutating result cannot mutate source");
    for (const [name, Type, values] of [
      ["unsigned", Uint16Array, [0, 65535]],
      ["signed64", BigInt64Array, [-9007199254741001n, 9007199254741001n]],
      ["unsigned64", BigUint64Array, [0n, 18446744073709551615n]],
      ["floating", Float64Array, [NaN, Infinity]],
    ]) {
      const h = own(ds.array(name)); const result = h.read();
      check(result instanceof Type, `native ${name}`); equal([...result], values, name);
    }
    rejects(() => ds.array("missing"), "missing variable");
    rejects(() => a.isel("missing", 0), "missing dimension");
    rejects(() => a.isel("time", 2), "index bounds");
    rejects(() => a.slice("x", 0, 2, 0), "zero stride");
    rejects(() => a.transpose(["x", "x", "time"]), "invalid permutation");
    const slab = own(a.isel("time", 1));
    const stride = own(slab.slice("x", 0, 2, 2));
    const child = own(stride.transpose(["x", "y"]));
    release(stride); release(slab); release(a); release(ds);
    equal([...child.read()], [4, 8, 12, 6, 10, 14], "Arc-retained child after ancestors disposed");
    let growth = false;
    if (memory) {
      check(copied.buffer !== memory.buffer, "copy does not alias WASM");
      const before = memory.buffer.byteLength;
      memory.grow(1);
      growth = memory.buffer.byteLength > before;
      check(growth, "explicit WASM memory growth");
    }
    for (let i = 0; i < 16; i++) child.read();
    equal([...copied], expected, "copy survives subsequent reads and memory growth");
    release(child);
    for (const input of [new Uint8Array(), new Uint8Array([1, 2, 3]), bytes.slice(0, 16)]) {
      rejects(() => terrio.openMultidim(input), "malformed/truncated file");
    }
    // Decode failure must not poison the next open.
    const recovered = own(terrio.openMultidim(bytes));
    const recoveredArray = own(recovered.array("sample"));
    equal([...recoveredArray.read()], expected, "reopen after errors");
    return { copied, fixtureBytes: bytes.byteLength, elapsedMs: performance.now() - start, growth };
  } finally {
    for (const h of handles.reverse()) h.free();
  }
}
