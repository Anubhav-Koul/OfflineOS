//! Deciding how many layers to offload to the GPU.
//!
//! `llama-server` takes a single `-ngl N` knob, and getting it wrong is
//! expensive in both directions: too low wastes the GPU, too high makes the
//! server allocate past the VRAM budget and die (usually mid-generation, which
//! is how a model ends up marked suspect). So we compute it rather than guess.
//!
//! Two facts about llama.cpp drive the arithmetic:
//!
//! - `-ngl N` offloads the **last** `N` transformer blocks, not the first.
//! - `-ngl N` where `N > block_count` additionally offloads the non-block
//!   tensors (token embeddings and the output projection).
//!
//! Each offloaded block also brings its slice of the KV cache onto the device,
//! which is sized by the context length and is frequently larger than the block
//! itself at long contexts. Ignoring it is the classic way to compute an `-ngl`
//! that fits the weights and then OOMs on the first long prompt.

use std::fmt;

use crate::gguf::GgufModel;
use crate::hardware::{GpuAdapter, SystemMemory};

/// Bytes per KV cache element for the default `f16` cache.
const F16_BYTES: u64 = 2;

/// Held back from the VRAM budget for llama.cpp's compute buffers, the CUDA/
/// Vulkan context, and allocator fragmentation. Compute buffers scale with
/// batch size rather than model size, and 512 MiB covers the default batch for
/// every model we'd run on a desktop.
const DEFAULT_VRAM_RESERVE_BYTES: u64 = 512 << 20;

/// Tunables for [`plan`]. Defaults match `llama-server`'s own defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementPolicy {
    /// Size of one KV cache element. `2` for the default `f16` cache; `1` when
    /// the caller passes `--cache-type-k q8_0`.
    pub kv_cache_bytes_per_element: u64,
    /// VRAM withheld from the layer budget.
    pub vram_reserve_bytes: u64,
    /// Whether to offload to an integrated GPU. Off by default: an iGPU's
    /// "VRAM" is system RAM, so offloading moves bytes without making them
    /// faster to reach, and it competes with the OS compositor.
    pub allow_integrated_gpu: bool,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self {
            kv_cache_bytes_per_element: F16_BYTES,
            vram_reserve_bytes: DEFAULT_VRAM_RESERVE_BYTES,
            allow_integrated_gpu: false,
        }
    }
}

/// Inputs to a placement decision.
#[derive(Debug, Clone, Copy)]
pub struct PlacementRequest<'a> {
    /// The model we're about to load.
    pub model: &'a GgufModel,
    /// The adapter we'd offload to, if any.
    pub gpu: Option<&'a GpuAdapter>,
    /// System RAM, when known. Without it the "won't fit anywhere" check is
    /// skipped rather than guessed.
    pub system: Option<SystemMemory>,
    /// The context window the server will be started with.
    pub ctx_size: u32,
    /// Tunables.
    pub policy: PlacementPolicy,
}

/// What we decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every block plus the output tensors are on the GPU.
    FullOffload,
    /// Some trailing blocks are on the GPU; the rest run on the CPU.
    PartialOffload,
    /// Nothing is on the GPU.
    CpuOnly,
    /// The model cannot be loaded on this machine at all.
    Refuse {
        /// User-facing explanation.
        reason: String,
    },
}

/// Something the user should know about this placement, even when it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// No GPU was found or none was usable.
    NoGpu,
    /// An integrated GPU was found but skipped by policy.
    IntegratedGpuSkipped {
        /// Adapter name.
        name: String,
    },
    /// The GPU exists but couldn't hold even one block after the reserve.
    NotEnoughVram {
        /// Free VRAM on the adapter.
        free_bytes: u64,
        /// What a single trailing block plus its KV slice would have cost.
        needed_bytes: u64,
    },
    /// Only some blocks fit.
    PartialOffload {
        /// Blocks placed on the GPU.
        placed: u32,
        /// Blocks in the model.
        total: u32,
    },
    /// The model's metadata didn't say enough to size the KV cache, so the
    /// estimate below only accounts for weights and is optimistic.
    KvCacheNotEstimated,
    /// The requested context exceeds what the model was trained for.
    ContextExceedsTrained {
        /// What we asked for.
        requested: u32,
        /// What the model declares.
        trained: u32,
    },
    /// It fits, but with little room to spare.
    TightSystemMemory {
        /// What the model needs in RAM.
        needed_bytes: u64,
        /// What the OS says is available.
        available_bytes: u64,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::NoGpu => write!(f, "no usable GPU found; running on CPU"),
            Warning::IntegratedGpuSkipped { name } => write!(
                f,
                "skipping integrated GPU {name}: its memory is system RAM, so offloading would not speed generation up"
            ),
            Warning::NotEnoughVram {
                free_bytes,
                needed_bytes,
            } => write!(
                f,
                "not enough free VRAM to offload even one layer ({} MiB free, {} MiB needed); running on CPU",
                mib(*free_bytes),
                mib(*needed_bytes)
            ),
            Warning::PartialOffload { placed, total } => write!(
                f,
                "offloading {placed} of {total} layers to the GPU; the rest run on the CPU"
            ),
            Warning::KvCacheNotEstimated => write!(
                f,
                "model metadata does not describe its attention shape, so the KV cache size could not be estimated; VRAM use may exceed the estimate"
            ),
            Warning::ContextExceedsTrained { requested, trained } => write!(
                f,
                "requested context of {requested} tokens exceeds the {trained} the model was trained for; quality will degrade"
            ),
            Warning::TightSystemMemory {
                needed_bytes,
                available_bytes,
            } => write!(
                f,
                "model needs {} MiB but only {} MiB of RAM is available; the system may swap",
                mib(*needed_bytes),
                mib(*available_bytes)
            ),
        }
    }
}

/// A placement decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The value to pass to `llama-server -ngl`. `block_count + 1` means full
    /// offload including the output tensors.
    pub n_gpu_layers: u32,
    /// Whether this is a full, partial, or no offload — or a refusal.
    pub verdict: Verdict,
    /// Estimated VRAM the server will consume, excluding the reserve.
    pub estimated_vram_bytes: u64,
    /// Estimated host RAM the server will consume: the blocks that stayed on
    /// the CPU, their KV slices, and the output tensors when not offloaded.
    pub estimated_host_bytes: u64,
    /// Everything the user should be told, in the order discovered.
    pub warnings: Vec<Warning>,
}

impl Placement {
    /// Whether the model can be loaded at all.
    pub fn is_runnable(&self) -> bool {
        !matches!(self.verdict, Verdict::Refuse { .. })
    }

    /// The refusal reason, when there is one.
    pub fn refusal(&self) -> Option<&str> {
        match &self.verdict {
            Verdict::Refuse { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Choose an `-ngl` for `request`.
///
/// Never fails: a machine that cannot run the model produces a
/// [`Verdict::Refuse`] carrying a user-facing reason, so the caller can show it
/// rather than translate an error.
pub fn plan(request: PlacementRequest<'_>) -> Placement {
    let PlacementRequest {
        model,
        gpu,
        system,
        ctx_size,
        policy,
    } = request;

    let mut warnings = Vec::new();

    if let Some(trained) = model.context_length
        && ctx_size > trained
    {
        warnings.push(Warning::ContextExceedsTrained {
            requested: ctx_size,
            trained,
        });
    }

    let kv_per_layer = kv_bytes_per_layer(model, ctx_size, policy.kv_cache_bytes_per_element);
    if kv_per_layer.is_none() {
        warnings.push(Warning::KvCacheNotEstimated);
    }
    let kv_per_layer = kv_per_layer.unwrap_or(0);
    let kv_total = kv_per_layer.saturating_mul(u64::from(model.block_count));

    let gpu = usable_gpu(gpu, policy, &mut warnings);
    let offload = match gpu {
        Some(adapter) => offload_layers(model, adapter, policy, kv_per_layer, &mut warnings),
        None => Offload::none(),
    };

    // Whatever did not go to the GPU has to live in RAM.
    let host_weight_bytes = model
        .layer_bytes
        .iter()
        .rev()
        .skip(offload.blocks as usize)
        .sum::<u64>();
    let host_kv_bytes =
        kv_per_layer.saturating_mul(u64::from(model.block_count.saturating_sub(offload.blocks)));
    let host_overhead = if offload.includes_output {
        0
    } else {
        model.overhead_bytes
    };
    let estimated_host_bytes = host_weight_bytes + host_kv_bytes + host_overhead;

    let verdict = if let Some(reason) = refusal_reason(model, kv_total, gpu, system) {
        Verdict::Refuse { reason }
    } else if offload.includes_output {
        Verdict::FullOffload
    } else if offload.blocks > 0 {
        warnings.push(Warning::PartialOffload {
            placed: offload.blocks,
            total: model.block_count,
        });
        Verdict::PartialOffload
    } else {
        Verdict::CpuOnly
    };

    if !matches!(verdict, Verdict::Refuse { .. })
        && let Some(memory) = system
        && estimated_host_bytes > memory.available_bytes.saturating_mul(9) / 10
        && estimated_host_bytes <= memory.available_bytes
    {
        warnings.push(Warning::TightSystemMemory {
            needed_bytes: estimated_host_bytes,
            available_bytes: memory.available_bytes,
        });
    }

    Placement {
        n_gpu_layers: offload.n_gpu_layers(),
        verdict,
        estimated_vram_bytes: offload.vram_bytes,
        estimated_host_bytes,
        warnings,
    }
}

/// How much of the model went to the device.
struct Offload {
    /// Trailing transformer blocks placed on the GPU.
    blocks: u32,
    /// Whether the non-block tensors went too, which is what `-ngl
    /// block_count + 1` means to llama.cpp.
    includes_output: bool,
    /// Weights + KV slices we expect to occupy, excluding the reserve.
    vram_bytes: u64,
}

impl Offload {
    fn none() -> Self {
        Self {
            blocks: 0,
            includes_output: false,
            vram_bytes: 0,
        }
    }

    fn n_gpu_layers(&self) -> u32 {
        self.blocks + u32::from(self.includes_output)
    }
}

/// Fill the VRAM budget with trailing blocks, since that is the set llama.cpp
/// offloads for a given `-ngl`.
fn offload_layers(
    model: &GgufModel,
    gpu: &GpuAdapter,
    policy: PlacementPolicy,
    kv_per_layer: u64,
    warnings: &mut Vec<Warning>,
) -> Offload {
    let free = gpu.free_bytes();
    let mut budget = free.saturating_sub(policy.vram_reserve_bytes);

    let mut blocks = 0u32;
    let mut vram_bytes = 0u64;
    for layer_bytes in model.layer_bytes.iter().rev() {
        let cost = layer_bytes.saturating_add(kv_per_layer);
        if cost > budget {
            break;
        }
        budget -= cost;
        vram_bytes += cost;
        blocks += 1;
    }

    if blocks == 0 {
        let needed = model
            .layer_bytes
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_add(kv_per_layer)
            .saturating_add(policy.vram_reserve_bytes);
        warnings.push(Warning::NotEnoughVram {
            free_bytes: free,
            needed_bytes: needed,
        });
        return Offload::none();
    }

    // Only when every block is resident does llama.cpp accept an `-ngl` that
    // also moves the embeddings and output projection.
    let includes_output = blocks == model.block_count && model.overhead_bytes <= budget;
    if includes_output {
        vram_bytes += model.overhead_bytes;
    }

    Offload {
        blocks,
        includes_output,
        vram_bytes,
    }
}

/// Filter out adapters we won't offload to, recording why.
fn usable_gpu<'a>(
    gpu: Option<&'a GpuAdapter>,
    policy: PlacementPolicy,
    warnings: &mut Vec<Warning>,
) -> Option<&'a GpuAdapter> {
    let Some(adapter) = gpu else {
        warnings.push(Warning::NoGpu);
        return None;
    };
    if !adapter.is_discrete() && !policy.allow_integrated_gpu {
        warnings.push(Warning::IntegratedGpuSkipped {
            name: adapter.name.clone(),
        });
        return None;
    }
    Some(adapter)
}

/// The model cannot be loaded when its weights and KV cache exceed everything
/// the machine has, GPU and RAM combined. Without a memory reading we decline to
/// guess and let the load proceed.
fn refusal_reason(
    model: &GgufModel,
    kv_total: u64,
    gpu: Option<&GpuAdapter>,
    system: Option<SystemMemory>,
) -> Option<String> {
    let memory = system?;
    let needed = model.weight_bytes().saturating_add(kv_total);
    let capacity = memory
        .available_bytes
        .saturating_add(gpu.map(GpuAdapter::free_bytes).unwrap_or(0));
    if needed <= capacity {
        return None;
    }
    Some(format!(
        "{} needs about {} MiB of memory ({} MiB of weights plus {} MiB of KV cache) but only {} MiB is available across GPU and RAM",
        model.name.as_deref().unwrap_or(&model.architecture),
        mib(needed),
        mib(model.weight_bytes()),
        mib(kv_total),
        mib(capacity),
    ))
}

/// KV cache bytes for a single transformer block at `ctx_size` tokens.
///
/// `2` for the key and value caches; each holds `ctx * kv_heads * head_dim`
/// elements.
fn kv_bytes_per_layer(model: &GgufModel, ctx_size: u32, bytes_per_element: u64) -> Option<u64> {
    let heads = model.kv_heads()?;
    let (key_dim, value_dim) = model.head_dims()?;
    let per_token = heads
        .checked_mul(key_dim.checked_add(value_dim)?)?
        .checked_mul(bytes_per_element)?;
    per_token.checked_mul(u64::from(ctx_size))
}

fn mib(bytes: u64) -> u64 {
    bytes / (1 << 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A model with `block_count` blocks of `layer_bytes` each. Attention shape
    /// is set so `kv_bytes_per_layer` is exactly `ctx * 2 * heads * head_dim`.
    fn model(block_count: u32, layer_bytes: u64, overhead_bytes: u64) -> GgufModel {
        GgufModel {
            path: PathBuf::from("test.gguf"),
            file_size: layer_bytes * u64::from(block_count) + overhead_bytes,
            architecture: "test".into(),
            name: Some("test-model".into()),
            block_count,
            context_length: Some(4096),
            embedding_length: Some(64),
            head_count: Some(4),
            head_count_kv: Some(4),
            key_length: None,
            value_length: None,
            layer_bytes: vec![layer_bytes; block_count as usize],
            overhead_bytes,
            metadata: BTreeMap::new(),
        }
    }

    fn gpu(free_bytes: u64) -> GpuAdapter {
        GpuAdapter {
            name: "Test GPU".into(),
            dedicated_vram_bytes: 8 << 30,
            budget_bytes: free_bytes,
            used_bytes: 0,
        }
    }

    /// No reserve, no KV, so the arithmetic in each test is exactly the layer
    /// bytes.
    fn bare_policy() -> PlacementPolicy {
        PlacementPolicy {
            kv_cache_bytes_per_element: 0,
            vram_reserve_bytes: 0,
            allow_integrated_gpu: false,
        }
    }

    fn request<'a>(
        model: &'a GgufModel,
        gpu: Option<&'a GpuAdapter>,
        policy: PlacementPolicy,
    ) -> PlacementRequest<'a> {
        PlacementRequest {
            model,
            gpu,
            system: Some(SystemMemory {
                total_bytes: 64 << 30,
                available_bytes: 32 << 30,
            }),
            ctx_size: 4096,
            policy,
        }
    }

    #[test]
    fn no_gpu_means_cpu_only() {
        let model = model(8, 100, 50);
        let placement = plan(request(&model, None, bare_policy()));
        assert_eq!(placement.n_gpu_layers, 0);
        assert_eq!(placement.verdict, Verdict::CpuOnly);
        assert!(placement.warnings.contains(&Warning::NoGpu));
        // Everything runs on the host.
        assert_eq!(placement.estimated_host_bytes, 8 * 100 + 50);
    }

    #[test]
    fn abundant_vram_offloads_everything_including_output() {
        let model = model(8, 100, 50);
        let gpu = gpu(1 << 30);
        let placement = plan(request(&model, Some(&gpu), bare_policy()));
        // `block_count + 1` is llama.cpp's "everything, including output".
        assert_eq!(placement.n_gpu_layers, 9);
        assert_eq!(placement.verdict, Verdict::FullOffload);
        assert_eq!(placement.estimated_vram_bytes, 8 * 100 + 50);
        assert_eq!(placement.estimated_host_bytes, 0);
    }

    #[test]
    fn all_blocks_fit_but_output_does_not_stays_partial() {
        let model = model(8, 100, 50);
        // Room for the 8 blocks (800) but not the 50 bytes of output tensors.
        let gpu = gpu(830);
        let placement = plan(request(&model, Some(&gpu), bare_policy()));
        assert_eq!(placement.n_gpu_layers, 8);
        assert_eq!(placement.verdict, Verdict::PartialOffload);
        assert_eq!(placement.estimated_host_bytes, 50);
    }

    #[test]
    fn tight_vram_offloads_a_prefix_of_the_trailing_blocks() {
        let model = model(8, 100, 50);
        let gpu = gpu(350);
        let placement = plan(request(&model, Some(&gpu), bare_policy()));
        assert_eq!(placement.n_gpu_layers, 3);
        assert_eq!(placement.verdict, Verdict::PartialOffload);
        assert_eq!(placement.estimated_vram_bytes, 300);
        // 5 blocks and the output tensors stayed behind.
        assert_eq!(placement.estimated_host_bytes, 5 * 100 + 50);
        assert!(placement.warnings.contains(&Warning::PartialOffload {
            placed: 3,
            total: 8
        }));
    }

    #[test]
    fn reserve_is_withheld_before_any_layer_is_placed() {
        let model = model(8, 100, 50);
        let gpu = gpu(350);
        let policy = PlacementPolicy {
            vram_reserve_bytes: 100,
            ..bare_policy()
        };
        let placement = plan(request(&model, Some(&gpu), policy));
        assert_eq!(placement.n_gpu_layers, 2);
    }

    #[test]
    fn kv_cache_consumes_budget_alongside_weights() {
        let model = model(8, 100, 50);
        // heads=4, head_dim=64/4=16, so per layer = 4 * (16+16) * 1 * ctx.
        // With ctx=1 and 1 byte per element that is 128 bytes per layer.
        let gpu = gpu(456);
        let policy = PlacementPolicy {
            kv_cache_bytes_per_element: 1,
            ..bare_policy()
        };
        let mut request = request(&model, Some(&gpu), policy);
        request.ctx_size = 1;
        let placement = plan(request);
        // Each layer now costs 100 + 128 = 228, so only two fit in 456.
        assert_eq!(placement.n_gpu_layers, 2);
        assert_eq!(placement.estimated_vram_bytes, 456);
    }

    #[test]
    fn integrated_gpu_is_skipped_by_default() {
        let model = model(8, 100, 50);
        let integrated = GpuAdapter {
            name: "Intel(R) Iris(R) Xe".into(),
            dedicated_vram_bytes: 0,
            budget_bytes: 8 << 30,
            used_bytes: 0,
        };
        let placement = plan(request(&model, Some(&integrated), bare_policy()));
        assert_eq!(placement.verdict, Verdict::CpuOnly);
        assert!(matches!(
            placement.warnings.first(),
            Some(Warning::IntegratedGpuSkipped { .. })
        ));

        let policy = PlacementPolicy {
            allow_integrated_gpu: true,
            ..bare_policy()
        };
        let placement = plan(request(&model, Some(&integrated), policy));
        assert_eq!(placement.verdict, Verdict::FullOffload);
    }

    #[test]
    fn a_gpu_too_small_for_one_layer_falls_back_to_cpu_with_a_warning() {
        let model = model(8, 100, 50);
        let gpu = gpu(30);
        let placement = plan(request(&model, Some(&gpu), bare_policy()));
        assert_eq!(placement.n_gpu_layers, 0);
        assert_eq!(placement.verdict, Verdict::CpuOnly);
        assert!(matches!(
            placement.warnings.first(),
            Some(Warning::NotEnoughVram { .. })
        ));
    }

    #[test]
    fn a_model_larger_than_gpu_and_ram_combined_is_refused() {
        let model = model(8, 100, 50);
        let mut request = request(&model, None, bare_policy());
        request.system = Some(SystemMemory {
            total_bytes: 1000,
            available_bytes: 100,
        });
        let placement = plan(request);
        assert!(!placement.is_runnable());
        let reason = placement.refusal().expect("a reason");
        assert!(reason.contains("only"), "{reason}");
    }

    #[test]
    fn unknown_system_memory_never_refuses() {
        let model = model(8, 100, 50);
        let mut request = request(&model, None, bare_policy());
        request.system = None;
        assert!(plan(request).is_runnable());
    }

    #[test]
    fn context_beyond_training_warns() {
        let model = model(2, 100, 0);
        let mut request = request(&model, None, bare_policy());
        request.ctx_size = 8192; // trained for 4096
        let placement = plan(request);
        assert!(
            placement
                .warnings
                .contains(&Warning::ContextExceedsTrained {
                    requested: 8192,
                    trained: 4096
                })
        );
    }

    #[test]
    fn missing_attention_metadata_warns_instead_of_guessing() {
        let mut model = model(2, 100, 0);
        model.head_count = None;
        model.head_count_kv = None;
        let placement = plan(request(&model, None, PlacementPolicy::default()));
        assert!(placement.warnings.contains(&Warning::KvCacheNotEstimated));
    }
}
