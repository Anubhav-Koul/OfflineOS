//! GPU and system-memory probing.
//!
//! On Windows we go through DXGI rather than a vendor SDK. `IDXGIAdapter3::
//! QueryVideoMemoryInfo` reports the *budget* the OS is currently willing to
//! give a process and how much of it that process already uses, which is the
//! number that actually predicts whether an allocation will succeed — it
//! shrinks when another application (a game, a browser's compositor) takes
//! VRAM, where a static "total VRAM" reading would not. It also works
//! identically for NVIDIA, AMD, and Intel, so there is one code path rather
//! than an nvml/adlx/level-zero fork.
//!
//! Everywhere else we report no GPUs, which makes the planner fall back to CPU
//! inference. The desktop target is Windows; the crate stays buildable on Linux
//! for CI and development.

use crate::error::Result;

/// Least `DedicatedVideoMemory` an adapter can report and still be believed to
/// have memory of its own.
///
/// A nonzero reading does *not* mean the adapter is discrete. Integrated GPUs
/// report the BIOS's UMA carve-out here — an AMD Radeon 780M reports 485 MiB,
/// Intel iGPUs typically 128 MiB — even though every byte of it is system RAM.
/// Every discrete GPU worth offloading to has at least 2 GiB, so a 1 GiB floor
/// separates the two cleanly without excluding an old 2 GiB card.
const DISCRETE_VRAM_FLOOR: u64 = 1 << 30;

/// A GPU we could offload layers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuAdapter {
    /// What the adapter claims as its own video memory. See
    /// [`GpuAdapter::is_discrete`] — a small nonzero value means an integrated
    /// GPU's carve-out of system RAM, not real VRAM.
    pub dedicated_vram_bytes: u64,
    /// Human-readable adapter name, e.g. `NVIDIA GeForce RTX 4070`.
    pub name: String,
    /// What the OS is currently prepared to let a process allocate.
    pub budget_bytes: u64,
    /// How much of that budget is already spoken for.
    pub used_bytes: u64,
}

impl GpuAdapter {
    /// Headroom for a new allocation: the budget minus what's already used.
    pub fn free_bytes(&self) -> u64 {
        self.budget_bytes.saturating_sub(self.used_bytes)
    }

    /// Whether the adapter has memory of its own.
    ///
    /// Offloading to an integrated GPU buys little — the weights sit in system
    /// RAM either way — and it competes with the desktop compositor for the
    /// same bandwidth. See [`DISCRETE_VRAM_FLOOR`] for why this is a threshold
    /// rather than a `> 0` test.
    pub fn is_discrete(&self) -> bool {
        self.dedicated_vram_bytes >= DISCRETE_VRAM_FLOOR
    }
}

/// System RAM, which bounds whether a model can run at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemMemory {
    /// Total installed RAM.
    pub total_bytes: u64,
    /// RAM that can be allocated without swapping.
    pub available_bytes: u64,
}

/// Enumerate usable GPUs, best (most free memory) first.
///
/// Returns an empty vector — not an error — when the platform has no GPU we can
/// query. An `Err` means the platform *should* have been able to answer and
/// didn't, which is worth surfacing.
pub fn probe_gpus() -> Result<Vec<GpuAdapter>> {
    let mut adapters = platform::probe_gpus()?;
    adapters.sort_by_key(|adapter| std::cmp::Reverse(adapter.free_bytes()));
    Ok(adapters)
}

/// The adapter we'd offload to: the discrete one with the most free memory, or
/// an integrated one if that's all there is.
pub fn best_gpu() -> Result<Option<GpuAdapter>> {
    let adapters = probe_gpus()?;
    let discrete = adapters.iter().find(|adapter| adapter.is_discrete());
    Ok(discrete.or_else(|| adapters.first()).cloned())
}

/// Installed and available system RAM, when the platform can tell us.
pub fn system_memory() -> Result<Option<SystemMemory>> {
    platform::system_memory()
}

#[cfg(windows)]
mod platform {
    use super::{GpuAdapter, SystemMemory};
    use crate::error::{Error, Result};

    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
        IDXGIFactory1,
    };
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::core::Interface as _;

    pub(super) fn probe_gpus() -> Result<Vec<GpuAdapter>> {
        // SAFETY: DXGI is a COM API with no thread-affinity requirements for
        // factory creation or adapter enumeration. Every interface pointer is
        // owned by a windows-rs wrapper that releases it on drop, and `desc` is
        // a plain POD struct we hand out by pointer for the duration of the
        // call.
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|error| Error::GpuProbe(error.to_string()))?;

            let mut adapters = Vec::new();
            for index in 0.. {
                let adapter = match factory.EnumAdapters1(index) {
                    Ok(adapter) => adapter,
                    // The documented terminator for adapter enumeration.
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(Error::GpuProbe(error.to_string())),
                };

                let desc = adapter
                    .GetDesc1()
                    .map_err(|error| Error::GpuProbe(error.to_string()))?;

                // WARP / "Microsoft Basic Render Driver": a CPU rasterizer that
                // reports memory it does not have. `Flags` is a bit set, so test
                // for the bit rather than comparing the whole field.
                if DXGI_ADAPTER_FLAG(desc.Flags as i32).contains(DXGI_ADAPTER_FLAG_SOFTWARE) {
                    continue;
                }

                // `IDXGIAdapter3` (Windows 10+) is what carries the memory-info
                // query. An adapter that doesn't support it can't be budgeted,
                // so it isn't a candidate.
                let Ok(adapter3) = adapter.cast::<IDXGIAdapter3>() else {
                    continue;
                };
                let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                adapter3
                    .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                    .map_err(|error| Error::GpuProbe(error.to_string()))?;

                adapters.push(GpuAdapter {
                    name: utf16_to_string(&desc.Description),
                    dedicated_vram_bytes: desc.DedicatedVideoMemory as u64,
                    budget_bytes: info.Budget,
                    used_bytes: info.CurrentUsage,
                });
            }
            Ok(adapters)
        }
    }

    pub(super) fn system_memory() -> Result<Option<SystemMemory>> {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: `status` is a correctly sized, correctly initialized
        // MEMORYSTATUSEX; `dwLength` is set as the API requires.
        unsafe { GlobalMemoryStatusEx(&mut status) }
            .map_err(|error| Error::GpuProbe(format!("GlobalMemoryStatusEx failed: {error}")))?;
        Ok(Some(SystemMemory {
            total_bytes: status.ullTotalPhys,
            available_bytes: status.ullAvailPhys,
        }))
    }

    /// `DXGI_ADAPTER_DESC1::Description` is a fixed 128-wchar buffer padded with
    /// NULs.
    fn utf16_to_string(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{GpuAdapter, SystemMemory};
    use crate::error::Result;

    /// No GPU enumeration off Windows. Not an error: the planner reads this as
    /// "CPU only", which is the correct behavior for a Linux CI runner.
    pub(super) fn probe_gpus() -> Result<Vec<GpuAdapter>> {
        Ok(Vec::new())
    }

    pub(super) fn system_memory() -> Result<Option<SystemMemory>> {
        #[cfg(target_os = "linux")]
        {
            let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
                return Ok(None); // silent-ok: memory probe is advisory; the planner degrades to "unknown"
            };
            let field = |name: &str| -> Option<u64> {
                let line = meminfo.lines().find(|line| line.starts_with(name))?;
                let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                Some(kib * 1024)
            };
            // `MemAvailable` is the kernel's own estimate of allocatable memory.
            if let (Some(total_bytes), Some(available_bytes)) =
                (field("MemTotal:"), field("MemAvailable:"))
            {
                return Ok(Some(SystemMemory {
                    total_bytes,
                    available_bytes,
                }));
            }
            Ok(None)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_bytes_saturates_when_usage_exceeds_budget() {
        // The OS can shrink the budget below current usage under pressure.
        let adapter = GpuAdapter {
            name: "test".into(),
            dedicated_vram_bytes: 8 << 30,
            budget_bytes: 1 << 30,
            used_bytes: 2 << 30,
        };
        assert_eq!(adapter.free_bytes(), 0);
    }

    #[test]
    fn integrated_adapters_are_not_discrete() {
        let integrated = GpuAdapter {
            name: "Intel(R) UHD Graphics".into(),
            dedicated_vram_bytes: 0,
            budget_bytes: 4 << 30,
            used_bytes: 0,
        };
        assert!(!integrated.is_discrete());
    }

    /// Regression: an AMD APU reports a 485 MiB BIOS carve-out as
    /// `DedicatedVideoMemory`. Treating any nonzero value as discrete made the
    /// planner offload to the iGPU whenever its carve-out had more headroom than
    /// a small discrete card — moving weights into system RAM and calling it a
    /// GPU offload.
    #[test]
    fn an_integrated_gpus_memory_carve_out_does_not_make_it_discrete() {
        let apu = GpuAdapter {
            name: "AMD Radeon(TM) Graphics".into(),
            dedicated_vram_bytes: 485 << 20,
            budget_bytes: 15 << 30,
            used_bytes: 0,
        };
        assert!(!apu.is_discrete());

        // A genuinely small discrete card still counts.
        let old_card = GpuAdapter {
            name: "NVIDIA GeForce GTX 1050".into(),
            dedicated_vram_bytes: 2 << 30,
            budget_bytes: 2 << 30,
            used_bytes: 0,
        };
        assert!(old_card.is_discrete());
    }

    #[test]
    fn probing_never_panics_on_this_machine() {
        // On Windows this exercises the real DXGI path; elsewhere it returns an
        // empty vector. Either way it must not error.
        let adapters = probe_gpus().expect("probe should not fail");
        for adapter in &adapters {
            assert!(adapter.free_bytes() <= adapter.budget_bytes);
        }
    }
}
