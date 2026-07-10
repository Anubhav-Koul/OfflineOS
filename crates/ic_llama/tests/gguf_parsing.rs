//! End-to-end GGUF parsing against synthetic files written to disk.
//!
//! The unit tests in `src/gguf.rs` cover the helpers; these cover the format —
//! header layout, the tensor-offset arithmetic that gives us per-layer sizes,
//! and the bounds that stop a corrupt file from being interesting.

use std::path::Path;

use ic_llama::Error;
use ic_llama::gguf::{GgufModel, Value};

/// Incrementally builds a GGUF file. Mirrors the writer side of the spec:
/// magic, version, counts, metadata, tensor infos, alignment padding, data.
#[derive(Default)]
struct GgufBuilder {
    metadata: Vec<u8>,
    metadata_count: u64,
    tensors: Vec<u8>,
    tensor_count: u64,
    version: u32,
    magic: [u8; 4],
}

impl GgufBuilder {
    fn new() -> Self {
        Self {
            version: 3,
            magic: *b"GGUF",
            ..Default::default()
        }
    }

    fn magic(mut self, magic: &[u8; 4]) -> Self {
        self.magic = *magic;
        self
    }

    fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    fn string(mut self, key: &str, value: &str) -> Self {
        push_string(&mut self.metadata, key);
        self.metadata.extend(8u32.to_le_bytes()); // GGUF type: string
        push_string(&mut self.metadata, value);
        self.metadata_count += 1;
        self
    }

    fn u32_value(mut self, key: &str, value: u32) -> Self {
        push_string(&mut self.metadata, key);
        self.metadata.extend(4u32.to_le_bytes()); // GGUF type: uint32
        self.metadata.extend(value.to_le_bytes());
        self.metadata_count += 1;
        self
    }

    /// A tokenizer-style string array: large, and nothing reads it.
    fn string_array(mut self, key: &str, items: &[&str]) -> Self {
        push_string(&mut self.metadata, key);
        self.metadata.extend(9u32.to_le_bytes()); // GGUF type: array
        self.metadata.extend(8u32.to_le_bytes()); // element type: string
        self.metadata.extend((items.len() as u64).to_le_bytes());
        for item in items {
            push_string(&mut self.metadata, item);
        }
        self.metadata_count += 1;
        self
    }

    /// A fixed-width array, which the parser skips by arithmetic rather than by
    /// reading element by element.
    fn u32_array(mut self, key: &str, items: &[u32]) -> Self {
        push_string(&mut self.metadata, key);
        self.metadata.extend(9u32.to_le_bytes()); // GGUF type: array
        self.metadata.extend(4u32.to_le_bytes()); // element type: uint32
        self.metadata.extend((items.len() as u64).to_le_bytes());
        for item in items {
            self.metadata.extend(item.to_le_bytes());
        }
        self.metadata_count += 1;
        self
    }

    fn tensor(mut self, name: &str, dims: &[u64], offset: u64) -> Self {
        push_string(&mut self.tensors, name);
        self.tensors.extend((dims.len() as u32).to_le_bytes());
        for dim in dims {
            self.tensors.extend(dim.to_le_bytes());
        }
        self.tensors.extend(0u32.to_le_bytes()); // ggml type: F32, unread
        self.tensors.extend(offset.to_le_bytes());
        self.tensor_count += 1;
        self
    }

    /// Serialize, padding the header to `alignment` and appending `data_size`
    /// bytes of tensor data.
    fn build(self, data_size: usize) -> Vec<u8> {
        self.build_with_alignment(data_size, 32)
    }

    fn build_with_alignment(self, data_size: usize, alignment: usize) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend(self.magic);
        buffer.extend(self.version.to_le_bytes());
        buffer.extend(self.tensor_count.to_le_bytes());
        buffer.extend(self.metadata_count.to_le_bytes());
        buffer.extend(&self.metadata);
        buffer.extend(&self.tensors);
        while buffer.len() % alignment != 0 {
            buffer.push(0);
        }
        buffer.resize(buffer.len() + data_size, 0xAB);
        buffer
    }
}

fn push_string(buffer: &mut Vec<u8>, value: &str) {
    buffer.extend((value.len() as u64).to_le_bytes());
    buffer.extend(value.as_bytes());
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write model file");
    path
}

/// A two-block model whose tensors are laid out at known offsets:
///
/// ```text
///   0..100   blk.0
/// 100..220   blk.1
/// 220..280   output.weight   (overhead)
/// ```
fn two_block_model() -> GgufBuilder {
    GgufBuilder::new()
        .string("general.architecture", "testarch")
        .string("general.name", "Test Model")
        .u32_value("testarch.block_count", 2)
        .u32_value("testarch.context_length", 4096)
        .u32_value("testarch.embedding_length", 64)
        .u32_value("testarch.attention.head_count", 4)
        .u32_value("testarch.attention.head_count_kv", 2)
        .tensor("blk.0.attn_q.weight", &[8, 8], 0)
        .tensor("blk.1.attn_q.weight", &[8, 8], 100)
        .tensor("output.weight", &[8, 8], 220)
}

#[tokio::test]
async fn a_well_formed_model_yields_its_shape_and_per_layer_sizes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = write(temp.path(), "model.gguf", &two_block_model().build(280));

    let model = GgufModel::read(&path).await.expect("parse");

    assert_eq!(model.architecture, "testarch");
    assert_eq!(model.name.as_deref(), Some("Test Model"));
    assert_eq!(model.block_count, 2);
    assert_eq!(model.context_length, Some(4096));

    // Sizes come from the gaps between successive tensor offsets; the last
    // tensor runs to the end of the data section.
    assert_eq!(model.layer_bytes, vec![100, 120]);
    assert_eq!(model.overhead_bytes, 60);
    assert_eq!(model.weight_bytes(), 280);
    assert_eq!(model.file_size, path.metadata().expect("stat").len());
}

#[tokio::test]
async fn attention_shape_drives_the_kv_cache_estimate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = write(temp.path(), "model.gguf", &two_block_model().build(280));

    let model = GgufModel::read(&path).await.expect("parse");

    // Grouped-query attention: fewer KV heads than attention heads.
    assert_eq!(model.kv_heads(), Some(2));
    // Not stored, so derived as embedding_length / head_count = 64 / 4.
    assert_eq!(model.head_dims(), Some((16, 16)));
}

#[tokio::test]
async fn an_explicit_key_length_overrides_the_derived_head_dim() {
    let temp = tempfile::tempdir().expect("tempdir");
    let bytes = two_block_model()
        .u32_value("testarch.attention.key_length", 128)
        .u32_value("testarch.attention.value_length", 192)
        .build(280);
    let path = write(temp.path(), "model.gguf", &bytes);

    let model = GgufModel::read(&path).await.expect("parse");
    assert_eq!(model.head_dims(), Some((128, 192)));
}

#[tokio::test]
async fn large_arrays_are_recorded_but_not_materialized() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Arrays sit between the keys we care about, so a bad skip would corrupt
    // everything after them.
    let bytes = two_block_model()
        .string_array("tokenizer.ggml.tokens", &["<s>", "hello", "world"])
        .u32_array("tokenizer.ggml.token_type", &[1, 1, 1, 1])
        .string("general.quantization_version", "2")
        .build(280);
    let path = write(temp.path(), "model.gguf", &bytes);

    let model = GgufModel::read(&path).await.expect("parse");

    assert_eq!(
        model.metadata.get("tokenizer.ggml.tokens"),
        Some(&Value::Array {
            element_type: 8,
            len: 3
        })
    );
    assert_eq!(
        model.metadata.get("tokenizer.ggml.token_type"),
        Some(&Value::Array {
            element_type: 4,
            len: 4
        })
    );
    // The key written after both arrays parsed correctly, so the skips landed
    // exactly where the next key begins.
    assert_eq!(
        model
            .metadata
            .get("general.quantization_version")
            .and_then(Value::as_str),
        Some("2")
    );
    assert_eq!(model.layer_bytes, vec![100, 120]);
}

#[tokio::test]
async fn a_custom_alignment_is_honored_when_locating_the_data_section() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Declaring 4096 moves the data section, and with it every tensor's absolute
    // position. Get this wrong and the last tensor's size comes out negative.
    let bytes = two_block_model()
        .u32_value("general.alignment", 4096)
        .build_with_alignment(280, 4096);
    let path = write(temp.path(), "model.gguf", &bytes);

    let model = GgufModel::read(&path).await.expect("parse");
    assert_eq!(model.layer_bytes, vec![100, 120]);
    assert_eq!(model.overhead_bytes, 60);
}

#[tokio::test]
async fn a_file_that_is_not_gguf_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = write(
        temp.path(),
        "model.gguf",
        &two_block_model().magic(b"GGML").build(280),
    );

    let error = GgufModel::read(&path).await.expect_err("bad magic");
    assert!(matches!(error, Error::Gguf { .. }), "{error:?}");
}

#[tokio::test]
async fn the_long_dead_v1_format_is_rejected_rather_than_misread() {
    let temp = tempfile::tempdir().expect("tempdir");
    // v1 used 32-bit lengths, so reading it as v3 would consume the wrong number
    // of bytes for every field.
    let path = write(
        temp.path(),
        "model.gguf",
        &two_block_model().version(1).build(280),
    );

    let error = GgufModel::read(&path)
        .await
        .expect_err("unsupported version");
    let Error::Gguf { reason, .. } = error else {
        panic!("expected a GGUF error");
    };
    assert!(reason.contains("version 1"), "{reason}");
}

#[tokio::test]
async fn a_model_without_a_block_count_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let bytes = GgufBuilder::new()
        .string("general.architecture", "testarch")
        .build(0);
    let path = write(temp.path(), "model.gguf", &bytes);

    let error = GgufModel::read(&path).await.expect_err("no block_count");
    let Error::Gguf { reason, .. } = error else {
        panic!("expected a GGUF error");
    };
    assert!(reason.contains("testarch.block_count"), "{reason}");
}

#[tokio::test]
async fn a_truncated_file_is_rejected_rather_than_underflowing() {
    let temp = tempfile::tempdir().expect("tempdir");
    // The header claims a data section that the file does not contain. The
    // `file_size - data_start` subtraction must not wrap.
    let mut bytes = two_block_model().build(280);
    bytes.truncate(bytes.len() - 300);
    let path = write(temp.path(), "model.gguf", &bytes);

    let error = GgufModel::read(&path).await.expect_err("truncated");
    assert!(
        matches!(error, Error::Gguf { .. } | Error::Io { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_tensor_pointing_past_the_data_section_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let bytes = GgufBuilder::new()
        .string("general.architecture", "testarch")
        .u32_value("testarch.block_count", 1)
        .tensor("blk.0.attn_q.weight", &[8, 8], 0)
        // Claims to start well beyond the 100-byte data section.
        .tensor("output.weight", &[8, 8], 100_000)
        .build(100);
    let path = write(temp.path(), "model.gguf", &bytes);

    let error = GgufModel::read(&path).await.expect_err("bad offset");
    assert!(matches!(error, Error::Gguf { .. }), "{error:?}");
}

#[tokio::test]
async fn an_implausible_metadata_count_is_refused_before_allocating() {
    let temp = tempfile::tempdir().expect("tempdir");
    // A hostile file claiming u64::MAX metadata entries must not make us try to
    // read them.
    let mut bytes = Vec::new();
    bytes.extend(b"GGUF");
    bytes.extend(3u32.to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    bytes.extend(u64::MAX.to_le_bytes());
    let path = write(temp.path(), "model.gguf", &bytes);

    let error = GgufModel::read(&path).await.expect_err("implausible count");
    let Error::Gguf { reason, .. } = error else {
        panic!("expected a GGUF error");
    };
    assert!(reason.contains("implausible"), "{reason}");
}

#[tokio::test]
async fn a_missing_file_reports_io_not_corruption() {
    let temp = tempfile::tempdir().expect("tempdir");
    let error = GgufModel::read(temp.path().join("absent.gguf"))
        .await
        .expect_err("missing file");
    assert!(matches!(error, Error::Io { .. }), "{error:?}");
}
