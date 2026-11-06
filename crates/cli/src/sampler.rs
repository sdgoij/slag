//! A flat self-time sampler for attribution runs.
//!
//! Enabled with `SLAG_SAMPLE=<interval-µs>` (e.g. `SLAG_SAMPLE=500`). A
//! watchdog thread periodically suspends the engine thread, reads its
//! instruction pointer, and resumes it. The recorded addresses are image
//! offsets (module base subtracted), printed to stderr at exit as
//!
//! ```text
//! profile <rva-hex> <count>
//! ```
//!
//! `tools/profile.py` turns those into function names with
//! `llvm-symbolizer`, so the binary needs no symbol handling of its own. A
//! sample outside the executable's image is code in another module — the
//! system heap, a C runtime's `memcpy`, JIT-generated code — and is counted
//! per containing module (see `profile outside <module> <count>`), because
//! "outside" on its own says only that the engine's own code is not running.
//!
//! The reading is a flat self-time share: it answers "which function is
//! running", not "which caller is paying". That is the question a
//! `mean-jitGap` table cannot answer, and it is the one to ask before
//! choosing a slice (see `.agents/skills/slag-performance`).

/// Starts the sampler when `SLAG_SAMPLE` is set, and prints the histogram
/// when the guard drops. Inert (and free) otherwise.
pub struct Profiler {
    #[cfg(windows)]
    inner: Option<imp::Sampler>,
}

impl Profiler {
    #[cfg(windows)]
    pub fn start() -> Self {
        let interval = std::env::var("SLAG_SAMPLE")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|micros| *micros > 0);
        Self {
            inner: interval.and_then(imp::Sampler::start),
        }
    }

    #[cfg(not(windows))]
    pub fn start() -> Self {
        Self {}
    }
}

impl Drop for Profiler {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(sampler) = self.inner.take() {
            sampler.finish();
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;

    /// The opened thread handle, moved to the sampler thread. The handle stays
    /// valid until that thread closes it, and nothing else touches it.
    struct ThreadHandle(Handle);

    // SAFETY: the value is a Win32 handle, which is thread-agnostic; only the
    // sampler thread ever dereferences it, and it closes it on the way out.
    unsafe impl Send for ThreadHandle {}

    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const THREAD_GET_CONTEXT: u32 = 0x0008;
    const THREAD_QUERY_INFORMATION: u32 = 0x0040;
    // `CONTEXT_CONTROL` on AMD64 (`CONTEXT_AMD64 | 0x1`).
    const CONTEXT_CONTROL: u32 = 0x0010_0001;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn OpenThread(desired_access: u32, inherit: i32, thread_id: u32) -> Handle;
        fn SuspendThread(thread: Handle) -> u32;
        fn ResumeThread(thread: Handle) -> u32;
        fn GetThreadContext(thread: Handle, context: *mut Context) -> i32;
        fn CloseHandle(object: Handle) -> i32;
        fn Sleep(millis: u32);
        fn GetModuleHandleW(name: *const u16) -> Handle;
        fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut Handle) -> i32;
        fn GetModuleFileNameW(module: Handle, name: *mut u16, size: u32) -> u32;
    }

    // The SDK's `winmm.lib` exports a plain `PlaySound` beside the
    // `PlaySoundA`/`PlaySoundW` pair, and raylib exports its own `PlaySound`,
    // so a binary that links both fails with `LNK2005: PlaySound already
    // defined in winmm.lib`. With the `raylib` feature on, `runtime::raylib`
    // answers these two calls (and raylib's own `__imp_` slots) from
    // `winmm.dll`, which leaves the import library nothing to resolve; without
    // it there is no collision and the import stays.
    #[cfg(not(feature = "raylib"))]
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    #[cfg(feature = "raylib")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    /// `CONTEXT` for AMD64 is 1232 bytes with 16-byte alignment; only
    /// `ContextFlags` (0x30) and `Rip` (0xF8) are read here.
    #[repr(C, align(16))]
    struct Context {
        buf: [u8; 1232],
    }

    impl Context {
        fn control() -> Self {
            let mut context = Self { buf: [0; 1232] };
            context.buf[0x30..0x34].copy_from_slice(&CONTEXT_CONTROL.to_ne_bytes());
            context
        }

        fn rip(&self) -> u64 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.buf[0xF8..0x100]);
            u64::from_ne_bytes(bytes)
        }
    }

    pub struct Sampler {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<Histogram>>,
    }

    /// `Self`-time histogram: image offset -> samples, plus the raw addresses of
    /// the samples that landed outside the executable. Those are resolved to a
    /// module only when the profile is printed: the loader's API deadlocks if a
    /// sampling thread calls it while the engine thread — which may be inside a
    /// load — is suspended.
    pub struct Histogram {
        pub samples: u64,
        pub outside: u64,
        pub by_offset: HashMap<u64, u64>,
        pub outside_rips: HashMap<usize, u64>,
    }

    impl Sampler {
        pub fn start(interval_micros: u64) -> Option<Self> {
            let thread_id = unsafe { GetCurrentThreadId() };
            // `SuspendThread` cannot take the pseudo-handle `GetCurrentThread`
            // returns, so open the real one.
            let handle = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                    0,
                    thread_id,
                )
            };
            if handle.is_null() {
                return None;
            }
            let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
            let size = image_size(base);
            if size == 0 {
                unsafe { CloseHandle(handle) };
                return None;
            }
            unsafe { timeBeginPeriod(1) };
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let thread = ThreadHandle(handle);
            let worker = std::thread::Builder::new()
                .name("slag-sampler".to_string())
                .spawn(move || sample(thread, &flag, interval_micros, base, size))
                .ok()?;
            Some(Self {
                stop,
                worker: Some(worker),
            })
        }

        pub fn finish(mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let histogram = match self.worker.take().and_then(|worker| worker.join().ok()) {
                Some(histogram) => histogram,
                None => return,
            };
            unsafe { timeEndPeriod(1) };
            print(&histogram);
        }
    }

    fn sample(
        handle: ThreadHandle,
        stop: &AtomicBool,
        interval_micros: u64,
        base: usize,
        size: usize,
    ) -> Histogram {
        let handle = handle.0;
        let mut histogram = Histogram {
            samples: 0,
            outside: 0,
            by_offset: HashMap::new(),
            outside_rips: HashMap::new(),
        };
        let interval = Duration::from_micros(interval_micros);
        let mut deadline = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            // `u32::MAX` is the documented failure return of `SuspendThread`.
            if unsafe { SuspendThread(handle) } != u32::MAX {
                let mut context = Context::control();
                if unsafe { GetThreadContext(handle, &mut context) } != 0 {
                    let rip = context.rip() as usize;
                    histogram.samples += 1;
                    if rip >= base && rip - base < size {
                        *histogram.by_offset.entry((rip - base) as u64).or_insert(0) += 1;
                    } else {
                        histogram.outside += 1;
                        *histogram.outside_rips.entry(rip).or_insert(0) += 1;
                    }
                }
                unsafe { ResumeThread(handle) };
            }
            deadline += interval;
            let now = Instant::now();
            if deadline > now {
                let remaining = deadline - now;
                if remaining > Duration::from_millis(1) {
                    // `timeBeginPeriod(1)` bounds the sleep granularity; the
                    // spin below recovers the rest.
                    let coarse = remaining - Duration::from_micros(400);
                    unsafe { Sleep(coarse.as_millis() as u32) };
                }
                while Instant::now() < deadline {
                    std::thread::yield_now();
                }
            } else {
                deadline = now;
            }
        }
        unsafe { CloseHandle(handle) };
        histogram
    }

    /// The `(file name, address - module base)` of the module containing
    /// `address` (`("<unknown>", 0)` when no loaded module owns it).
    fn module_of(address: usize) -> (String, usize) {
        // SAFETY: both calls only read module metadata for a live address; the
        // temporary `Handle` is never used as a reference.
        unsafe {
            let mut module: Handle = std::ptr::null_mut();
            let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
            if GetModuleHandleExW(flags, address as *const u16, &mut module) == 0 {
                return ("<unknown>".to_string(), 0);
            }
            let mut buffer = [0u16; 260];
            let len = GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) as usize;
            let name = String::from_utf16_lossy(&buffer[..len.min(buffer.len())]);
            let file = match name.rsplit(['\\', '/']).next() {
                Some(file) if !file.is_empty() => file.to_string(),
                _ => "<unknown>".to_string(),
            };
            (file, address.wrapping_sub(module as usize))
        }
    }

    /// The loaded image's `SizeOfImage`, read from its own PE header.
    fn image_size(base: usize) -> usize {
        if base == 0 {
            return 0;
        }
        // SAFETY: the module is mapped for the life of the process; the
        // header offsets below are fixed by the PE32+ format.
        unsafe {
            let dos = base as *const u8;
            let pe = u32::from_ne_bytes(std::ptr::read_unaligned(dos.add(0x3C) as *const [u8; 4]));
            let image = (base as *const u8).add(pe as usize);
            // Signature (4) + COFF header (20) + `SizeOfImage` at 0x38 of the
            // optional header.
            let size_offset = 4 + 20 + 0x38;
            u32::from_ne_bytes(std::ptr::read_unaligned(
                image.add(size_offset) as *const [u8; 4]
            )) as usize
        }
    }

    fn print(histogram: &Histogram) {
        let shared = |samples: u64| {
            if histogram.samples == 0 {
                0.0
            } else {
                100.0 * samples as f64 / histogram.samples as f64
            }
        };
        eprintln!(
            "profile samples {} outside {} ({:.1}%)",
            histogram.samples,
            histogram.outside,
            shared(histogram.outside)
        );
        let mut rows: Vec<(&u64, &u64)> = histogram.by_offset.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        for (offset, count) in rows {
            eprintln!("profile 0x{offset:x} {count} {:.2}", shared(*count));
        }
        let mut modules: HashMap<(String, usize), u64> = HashMap::new();
        for (rip, count) in &histogram.outside_rips {
            *modules.entry(module_of(*rip)).or_insert(0) += count;
        }
        let mut modules: Vec<(&(String, usize), &u64)> = modules.iter().collect();
        modules.sort_by(|a, b| b.1.cmp(a.1));
        for ((module, offset), count) in modules {
            eprintln!(
                "profile outside {module} 0x{offset:x} {count} {:.2}",
                shared(*count)
            );
        }
        eprintln!("profile end");
    }
}
