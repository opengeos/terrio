//! Peak live requested Rust heap bytes for the actual JS-style opening expressions.
//! Run each case in a fresh process; see owned_bytes_benchmark.md beside this file.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Meter;
fn added(n: usize) {
    let live = LIVE.fetch_add(n, SeqCst) + n;
    PEAK.fetch_max(live, SeqCst);
}
// SAFETY: forwards the original pointer and layout unchanged to System. Counters
// perform no allocations. A failed allocation is never added to the live total.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            added(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), SeqCst);
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), SeqCst);
            added(size);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;
fn classic(count: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(80 + count * 2);
    bytes.extend_from_slice(b"CDF\x01");
    // One dimension x and Int16 variable v; even count keeps CDF-1 padding valid.
    for word in [0u32, 10, 1, 1, 0x78000000, count as u32, 0, 0, 11, 1, 1, 0x76000000, 1, 0, 0, 0, 3, (count * 2) as u32, 80] {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    for i in 0..count {
        bytes.extend_from_slice(&((i % 101) as i16 - 50).to_be_bytes());
    }
    bytes
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 4, "FORMAT(classic|hdf5|hdf5-userblock) MODE(old|owned) MiB");
    let mib: usize = args[3].parse().unwrap();
    assert!((1..=512).contains(&mib));
    let count = mib * 1024 * 1024 / 2;
    let bytes = classic(count);
    let bytes = match args[1].as_str() {
        "classic" => bytes,
        "hdf5" | "hdf5-userblock" => {
            let hdf = terrio::multidim::open_bytes_owned(bytes).unwrap().to_netcdf_bytes().unwrap();
            if args[1] == "hdf5-userblock" {
                let mut prefixed = vec![0; 512];
                prefixed.extend_from_slice(&hdf);
                prefixed
            } else {
                hdf
            }
        }
        _ => panic!("unknown format"),
    };
    // Fixture construction and its temporaries have finished. Keep the source
    // alive in both modes, modeling a caller-owned JS Uint8Array outside WASM.
    let baseline = LIVE.load(SeqCst);
    PEAK.store(baseline, SeqCst);
    let dataset = match args[2].as_str() {
        "old" => terrio::multidim::open_bytes(&bytes.to_vec()).unwrap(),
        "owned" => terrio::multidim::open_bytes_owned(bytes.to_vec()).unwrap(),
        _ => panic!("unknown mode"),
    };
    let peak = PEAK.load(SeqCst);
    let retained = LIVE.load(SeqCst);
    // Validate lazy reads after the dataset has dropped, outside the measurement.
    let view = dataset.array("v").unwrap().view();
    drop(dataset);
    let values = view.read().unwrap().values().unwrap();
    let values = values.as_array().unwrap();
    assert_eq!(values.len(), count);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(value.as_i64().unwrap(), (i % 101) as i64 - 50);
    }
    println!(
        "format={},mode={},input={},baseline={},peak={},peak_delta={},retained_delta={}",
        args[1],
        args[2],
        bytes.len(),
        baseline,
        peak,
        peak - baseline,
        retained - baseline
    );
    std::hint::black_box(bytes);
}
