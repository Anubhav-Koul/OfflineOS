//! A read-only GGUF header parser.
//!
//! We need three things out of a model file before we can decide how to run it:
//! its architecture, how many transformer blocks it has, and how big each of
//! those blocks is on disk. The first two come from the key/value metadata; the
//! third is the interesting one.
//!
//! Computing a tensor's size from its dimensions requires a table of every
//! `ggml` quantization type's block size — 40-odd entries that drift upstream
//! and would silently produce wrong numbers if they fell out of date. We sidestep
//! that entirely: GGUF stores each tensor's *offset* into the data section, and
//! tensors are written back to back, so a tensor's size is the distance to the
//! next offset (and the last one runs to the end of the file). No type table, no
//! drift.
//!
//! The parser is bounded everywhere it reads a length off the wire — a model
//! file is untrusted input the moment a user downloads one from the hub.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Every GGUF file starts with these four bytes.
const MAGIC: &[u8; 4] = b"GGUF";

/// v1 used 32-bit lengths and is long dead; we read v2 and v3, which share a
/// layout for everything we look at.
const SUPPORTED_VERSIONS: &[u32] = &[2, 3];

/// Default tensor-data alignment when `general.alignment` is absent.
const DEFAULT_ALIGNMENT: u64 = 32;

// Bounds. A well-formed model is nowhere near any of these; a corrupt or
// hostile one would otherwise ask us to allocate its 64-bit length field.
const MAX_KV_COUNT: u64 = 8192;
const MAX_TENSOR_COUNT: u64 = 1 << 20;
const MAX_STRING_LEN: u64 = 32 << 20; // chat templates can be tens of KB
const MAX_ARRAY_LEN: u64 = 1 << 26; // tokenizer vocabularies reach ~10^6
const MAX_DIMENSIONS: u32 = 8;

/// A metadata value.
///
/// Arrays are recorded but not materialized: the only arrays in a GGUF file are
/// tokenizer vocabularies and merge tables, which run to millions of strings and
/// which nothing in this crate reads.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Any of the fixed-width unsigned types, widened.
    U64(u64),
    /// Any of the fixed-width signed types, widened.
    I64(i64),
    /// `float32` or `float64`.
    F64(f64),
    /// `bool`.
    Bool(bool),
    /// `string`.
    String(String),
    /// An array, skipped over rather than read.
    Array {
        /// The GGUF type id of the elements.
        element_type: u32,
        /// How many elements it held.
        len: u64,
    },
}

impl Value {
    /// Interpret as a non-negative integer, whatever integer width it was
    /// stored as. `None` for negatives and non-integers.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::U64(value) => Some(*value),
            Value::I64(value) => u64::try_from(*value).ok(),
            _ => None,
        }
    }

    /// Interpret as a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }
}

/// One tensor's header entry.
#[derive(Debug, Clone)]
struct TensorInfo {
    name: String,
    offset: u64,
}

/// Everything this crate needs to know about a GGUF file, read from its header.
#[derive(Debug, Clone)]
pub struct GgufModel {
    /// Where the file lives.
    pub path: PathBuf,
    /// Total size of the file on disk. Weights dominate it.
    pub file_size: u64,
    /// `general.architecture`, e.g. `qwen3`, `llama`.
    pub architecture: String,
    /// `general.name`, when present.
    pub name: Option<String>,
    /// `{arch}.block_count` — the number of transformer layers.
    pub block_count: u32,
    /// `{arch}.context_length` — the context the model was trained for.
    pub context_length: Option<u32>,
    /// `{arch}.embedding_length`.
    pub embedding_length: Option<u32>,
    /// `{arch}.attention.head_count`.
    pub head_count: Option<u32>,
    /// `{arch}.attention.head_count_kv`. Equal to `head_count` for models that
    /// don't use grouped-query attention.
    pub head_count_kv: Option<u32>,
    /// `{arch}.attention.key_length`, when the model doesn't use
    /// `embedding_length / head_count`.
    pub key_length: Option<u32>,
    /// `{arch}.attention.value_length`.
    pub value_length: Option<u32>,
    /// On-disk bytes of each transformer block, indexed by block number.
    /// Length is always [`GgufModel::block_count`].
    pub layer_bytes: Vec<u64>,
    /// On-disk bytes of everything that isn't a transformer block: the token
    /// embeddings, the output norm, and the output projection. llama.cpp only
    /// moves these to the GPU when `n_gpu_layers > block_count`.
    pub overhead_bytes: u64,
    /// All scalar and string metadata, for callers that want more than the
    /// fields above (the chat template, quantization type, and so on).
    pub metadata: BTreeMap<String, Value>,
}

impl GgufModel {
    /// Read the header of the GGUF file at `path`.
    ///
    /// Only the header is read — a few hundred KB for a large model — never the
    /// weights.
    pub async fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let handle = tokio::task::spawn_blocking(move || Self::read_blocking(&path));
        match handle.await {
            Ok(result) => result,
            Err(join_error) => Err(Error::io(
                "GGUF parse task failed",
                std::io::Error::other(join_error),
            )),
        }
    }

    /// Blocking form of [`GgufModel::read`], for callers already on a blocking
    /// thread.
    pub fn read_blocking(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|source| Error::io(format!("opening {}", path.display()), source))?;
        let file_size = file
            .metadata()
            .map_err(|source| Error::io(format!("stat-ing {}", path.display()), source))?
            .len();
        let mut reader = Reader {
            inner: BufReader::with_capacity(1 << 16, file),
            path,
            pos: 0,
        };
        parse(&mut reader, path, file_size)
    }

    /// Bytes that must be resident to run the whole model, ignoring the KV
    /// cache. Equal to the file size in practice; stated explicitly because the
    /// planner reasons in terms of the parts.
    pub fn weight_bytes(&self) -> u64 {
        self.layer_bytes.iter().sum::<u64>() + self.overhead_bytes
    }

    /// Size of one attention head's key (and separately value) vector.
    ///
    /// Most architectures don't store `key_length`, in which case it is
    /// `embedding_length / head_count`. Returns `None` when the metadata needed
    /// to derive it is missing, which makes the planner fall back to a
    /// conservative estimate instead of guessing.
    pub fn head_dims(&self) -> Option<(u64, u64)> {
        let derived = || {
            let embedding = self.embedding_length? as u64;
            let heads = self.head_count? as u64;
            if heads == 0 {
                return None;
            }
            Some(embedding / heads)
        };
        let key = match self.key_length {
            Some(length) => length as u64,
            None => derived()?,
        };
        let value = match self.value_length {
            Some(length) => length as u64,
            None => derived()?,
        };
        Some((key, value))
    }

    /// Number of key/value heads, defaulting to [`GgufModel::head_count`] for
    /// architectures without grouped-query attention.
    pub fn kv_heads(&self) -> Option<u64> {
        self.head_count_kv
            .or(self.head_count)
            .map(u64::from)
            .filter(|heads| *heads > 0)
    }
}

/// Parse the header. Split out from [`GgufModel::read_blocking`] so the reader
/// setup and the format logic stay separable.
fn parse<R: Read + Seek>(
    reader: &mut Reader<'_, R>,
    path: &Path,
    file_size: u64,
) -> Result<GgufModel> {
    let corrupt = |reason: String| Error::Gguf {
        path: path.to_path_buf(),
        reason,
    };

    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(corrupt(format!(
            "expected magic {:?}, found {magic:?}",
            MAGIC
        )));
    }

    let version = reader.u32()?;
    if !SUPPORTED_VERSIONS.contains(&version) {
        return Err(corrupt(format!(
            "unsupported GGUF version {version}; this build reads {SUPPORTED_VERSIONS:?}"
        )));
    }

    let tensor_count = reader.u64()?;
    if tensor_count > MAX_TENSOR_COUNT {
        return Err(corrupt(format!("implausible tensor count {tensor_count}")));
    }
    let kv_count = reader.u64()?;
    if kv_count > MAX_KV_COUNT {
        return Err(corrupt(format!("implausible metadata count {kv_count}")));
    }

    let mut metadata = BTreeMap::new();
    for _ in 0..kv_count {
        let key = reader.string()?;
        let value_type = reader.u32()?;
        let value = reader.value(value_type)?;
        metadata.insert(key, value);
    }

    let mut tensors = Vec::with_capacity(tensor_count.min(4096) as usize);
    for _ in 0..tensor_count {
        let name = reader.string()?;
        let dimensions = reader.u32()?;
        if dimensions > MAX_DIMENSIONS {
            return Err(corrupt(format!(
                "tensor {name:?} claims {dimensions} dimensions"
            )));
        }
        for _ in 0..dimensions {
            let _extent = reader.u64()?;
        }
        let _ggml_type = reader.u32()?;
        let offset = reader.u64()?;
        tensors.push(TensorInfo { name, offset });
    }

    let alignment = metadata
        .get("general.alignment")
        .and_then(Value::as_u64)
        .filter(|alignment| alignment.is_power_of_two())
        .unwrap_or(DEFAULT_ALIGNMENT);
    let data_start = align_up(reader.pos, alignment);
    let data_size = file_size.checked_sub(data_start).ok_or_else(|| {
        corrupt(format!(
            "header claims tensor data starts at {data_start} but the file is only {file_size} bytes"
        ))
    })?;

    let architecture = metadata
        .get("general.architecture")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("missing general.architecture".to_string()))?
        .to_string();

    let key = |suffix: &str| format!("{architecture}.{suffix}");
    let scalar_u32 = |suffix: &str| -> Option<u32> {
        metadata
            .get(&key(suffix))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };

    let block_count = scalar_u32("block_count")
        .ok_or_else(|| corrupt(format!("missing {}", key("block_count"))))?;
    if block_count == 0 || block_count as u64 > MAX_TENSOR_COUNT {
        return Err(corrupt(format!("implausible block_count {block_count}")));
    }

    let (layer_bytes, overhead_bytes) =
        attribute_tensor_bytes(tensors, data_size, block_count, path)?;

    Ok(GgufModel {
        path: path.to_path_buf(),
        file_size,
        name: metadata
            .get("general.name")
            .and_then(Value::as_str)
            .map(str::to_string),
        block_count,
        context_length: scalar_u32("context_length"),
        embedding_length: scalar_u32("embedding_length"),
        head_count: scalar_u32("attention.head_count"),
        head_count_kv: scalar_u32("attention.head_count_kv"),
        key_length: scalar_u32("attention.key_length"),
        value_length: scalar_u32("attention.value_length"),
        architecture,
        layer_bytes,
        overhead_bytes,
        metadata,
    })
}

/// Turn tensor offsets into per-block byte counts.
///
/// Each tensor's size is the gap to the next offset; the last runs to the end of
/// the data section. Tensors named `blk.<n>.…` belong to block `n`; everything
/// else (`token_embd.weight`, `output.weight`, `output_norm.weight`) is
/// overhead that only lands on the GPU at full offload.
fn attribute_tensor_bytes(
    mut tensors: Vec<TensorInfo>,
    data_size: u64,
    block_count: u32,
    path: &Path,
) -> Result<(Vec<u64>, u64)> {
    let corrupt = |reason: String| Error::Gguf {
        path: path.to_path_buf(),
        reason,
    };

    let mut layer_bytes = vec![0u64; block_count as usize];
    let mut overhead_bytes = 0u64;
    if tensors.is_empty() {
        return Ok((layer_bytes, overhead_bytes));
    }

    // The writer emits tensors in ascending offset order, but nothing in the
    // format guarantees it, and the sizes below are meaningless if it doesn't
    // hold.
    tensors.sort_by_key(|tensor| tensor.offset);
    if let Some(last) = tensors.last()
        && last.offset > data_size
    {
        return Err(corrupt(format!(
            "tensor {:?} starts at {} but the data section is only {data_size} bytes",
            last.name, last.offset
        )));
    }

    for (index, tensor) in tensors.iter().enumerate() {
        let end = match tensors.get(index + 1) {
            Some(next) => next.offset,
            None => data_size,
        };
        let size = end.saturating_sub(tensor.offset);
        match block_index(&tensor.name) {
            Some(block) if (block as usize) < layer_bytes.len() => {
                layer_bytes[block as usize] += size;
            }
            // A `blk.N.` tensor beyond `block_count` means the metadata and the
            // tensor list disagree; charge it to overhead rather than silently
            // dropping bytes the planner needs to account for.
            Some(block) => {
                tracing::warn!(
                    tensor = %tensor.name,
                    block,
                    block_count,
                    "tensor names a block beyond block_count; counting it as overhead"
                );
                overhead_bytes += size;
            }
            None => overhead_bytes += size,
        }
    }

    Ok((layer_bytes, overhead_bytes))
}

/// `blk.17.attn_q.weight` → `Some(17)`.
fn block_index(tensor_name: &str) -> Option<u32> {
    let rest = tensor_name.strip_prefix("blk.")?;
    let digits = rest.split('.').next()?;
    digits.parse().ok()
}

/// Round `value` up to the next multiple of `alignment` (a power of two).
fn align_up(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment.is_power_of_two());
    value.div_ceil(alignment) * alignment
}

/// A position-tracking reader. GGUF is a forward-only format, and the tensor
/// data offset is relative to where the header ends, so we need to know the
/// header's length exactly.
struct Reader<'p, R> {
    inner: BufReader<R>,
    path: &'p Path,
    pos: u64,
}

impl<R: Read + Seek> Reader<'_, R> {
    fn corrupt(&self, reason: String) -> Error {
        Error::Gguf {
            path: self.path.to_path_buf(),
            reason,
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf).map_err(|source| {
            Error::io(
                format!("reading GGUF header of {}", self.path.display()),
                source,
            )
        })?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    fn u32(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u64()?;
        if len > MAX_STRING_LEN {
            return Err(self.corrupt(format!("string field claims {len} bytes")));
        }
        let mut buf = vec![0u8; len as usize];
        self.read_exact(&mut buf)?;
        String::from_utf8(buf).map_err(|_| self.corrupt("string field is not valid UTF-8".into()))
    }

    /// Skip `count` bytes without materializing them.
    fn skip(&mut self, count: u64) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let target = self
            .pos
            .checked_add(count)
            .ok_or_else(|| self.corrupt(format!("length field {count} overflows the file")))?;
        self.inner.seek(SeekFrom::Start(target)).map_err(|source| {
            Error::io(
                format!("seeking in GGUF header of {}", self.path.display()),
                source,
            )
        })?;
        self.pos = target;
        Ok(())
    }

    /// Read a metadata value of GGUF type `value_type`.
    fn value(&mut self, value_type: u32) -> Result<Value> {
        Ok(match value_type {
            0 => Value::U64(u64::from(self.byte()?)),
            1 => Value::I64(i64::from(self.byte()? as i8)),
            2 => Value::U64(u64::from(self.u16()?)),
            3 => Value::I64(i64::from(self.u16()? as i16)),
            4 => Value::U64(u64::from(self.u32()?)),
            5 => Value::I64(i64::from(self.u32()? as i32)),
            6 => Value::F64(f64::from(f32::from_bits(self.u32()?))),
            7 => Value::Bool(self.byte()? != 0),
            8 => Value::String(self.string()?),
            9 => self.array()?,
            10 => Value::U64(self.u64()?),
            11 => Value::I64(self.u64()? as i64),
            12 => Value::F64(f64::from_bits(self.u64()?)),
            other => return Err(self.corrupt(format!("unknown metadata type {other}"))),
        })
    }

    /// Consume an array, recording only its shape.
    fn array(&mut self) -> Result<Value> {
        let element_type = self.u32()?;
        let len = self.u64()?;
        if len > MAX_ARRAY_LEN {
            return Err(self.corrupt(format!("array claims {len} elements")));
        }
        // Nested arrays are legal in the format but appear in no real model, and
        // supporting them would mean unbounded recursion on untrusted input.
        if element_type == 9 {
            return Err(self.corrupt("nested arrays are not supported".into()));
        }
        match fixed_width(element_type) {
            Some(width) => self.skip(width.saturating_mul(len))?,
            None if element_type == 8 => {
                for _ in 0..len {
                    let string_len = self.u64()?;
                    if string_len > MAX_STRING_LEN {
                        return Err(
                            self.corrupt(format!("array string element claims {string_len} bytes"))
                        );
                    }
                    self.skip(string_len)?;
                }
            }
            None => {
                return Err(self.corrupt(format!("array of unknown element type {element_type}")));
            }
        }
        Ok(Value::Array { element_type, len })
    }

    fn byte(&mut self) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.read_exact(&mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }
}

/// Byte width of the fixed-size GGUF scalar types.
fn fixed_width(value_type: u32) -> Option<u64> {
    Some(match value_type {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_index_reads_the_layer_number() {
        assert_eq!(block_index("blk.17.attn_q.weight"), Some(17));
        assert_eq!(block_index("blk.0.ffn_up.weight"), Some(0));
        assert_eq!(block_index("token_embd.weight"), None);
        assert_eq!(block_index("output.weight"), None);
        assert_eq!(block_index("blk.notanumber.weight"), None);
    }

    #[test]
    fn align_up_rounds_to_the_next_multiple() {
        assert_eq!(align_up(0, 32), 0);
        assert_eq!(align_up(1, 32), 32);
        assert_eq!(align_up(32, 32), 32);
        assert_eq!(align_up(33, 32), 64);
    }

    #[test]
    fn tensor_bytes_come_from_offset_deltas() {
        let tensors = vec![
            TensorInfo {
                name: "token_embd.weight".into(),
                offset: 0,
            },
            TensorInfo {
                name: "blk.0.attn_q.weight".into(),
                offset: 100,
            },
            TensorInfo {
                name: "blk.0.ffn_up.weight".into(),
                offset: 150,
            },
            TensorInfo {
                name: "blk.1.attn_q.weight".into(),
                offset: 200,
            },
            TensorInfo {
                name: "output.weight".into(),
                offset: 260,
            },
        ];
        let (layers, overhead) =
            attribute_tensor_bytes(tensors, 300, 2, Path::new("test.gguf")).expect("valid");
        // blk.0 spans 100..200, blk.1 spans 200..260.
        assert_eq!(layers, vec![100, 60]);
        // token_embd 0..100 plus output 260..300.
        assert_eq!(overhead, 140);
    }

    #[test]
    fn tensor_offsets_past_the_data_section_are_rejected() {
        let tensors = vec![TensorInfo {
            name: "output.weight".into(),
            offset: 500,
        }];
        let error = attribute_tensor_bytes(tensors, 300, 1, Path::new("test.gguf"))
            .expect_err("offset exceeds the data section");
        assert!(matches!(error, Error::Gguf { .. }), "{error:?}");
    }

    #[test]
    fn unsorted_tensor_offsets_are_normalized() {
        let tensors = vec![
            TensorInfo {
                name: "blk.1.attn_q.weight".into(),
                offset: 200,
            },
            TensorInfo {
                name: "blk.0.attn_q.weight".into(),
                offset: 0,
            },
        ];
        let (layers, overhead) =
            attribute_tensor_bytes(tensors, 300, 2, Path::new("test.gguf")).expect("valid");
        assert_eq!(layers, vec![200, 100]);
        assert_eq!(overhead, 0);
    }
}
