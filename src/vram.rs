//! Context-window estimation and setup-file editing for the GUI.
//!
//! The GUI's per-service "context" slider edits `flags.ctx_size` in
//! `~/.config/llm-server-indicator/setups/<name>.toml`. Its upper bound is
//! estimated from what would actually fit in free GPU VRAM:
//!
//! ```text
//! usable  = free_vram - SAFETY_MARGIN_MIB - weights_on_gpu
//! max_ctx = floor(usable / kv_bytes_per_token)   (rounded down to 256)
//! ```
//!
//! `kv_bytes_per_token` is derived from the model's GGUF metadata (full-
//! attention layer count × KV head count × head size × cache quantization).
//! Reading the metadata matters for hybrid architectures (e.g. `qwen35`)
//! where only every Nth layer is full attention — counting all blocks would
//! overestimate the per-token cost many times over.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::SystemTime;

use once_cell::sync::Lazy;

use crate::run::{SetupSpec, setups_dir};

/// VRAM kept aside (beyond model weights + KV cache) for the CUDA context,
/// compute buffers, and the desktop compositor.
pub const SAFETY_MARGIN_MIB: u64 = 1024;
/// Minimum context window the slider will offer.
pub const MIN_CTX: u32 = 6400;
/// Generic slider maximum when estimation is not possible (no nvidia-smi,
/// unreadable GGUF, …). Deliberately conservative: a context that does not
/// fit simply fails to load and the user adjusts it.
pub const FALLBACK_MAX_CTX: u32 = 131072;
/// Estimated maxima are rounded down to a multiple of this.
const CTX_ROUND: u64 = 256;
const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// GGUF header (minimal, read-only)
// ---------------------------------------------------------------------------

/// The architecture-specific keys the estimator needs. Everything else in
/// the header is skipped (tokenizer blobs can be tens of MB).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct GgufInfo {
    pub arch: Option<String>,
    pub block_count: Option<u64>,
    pub embedding_length: Option<u64>,
    pub head_count: Option<u64>,
    pub head_count_kv: Option<u64>,
    pub head_size: Option<u64>,
    pub key_count: Option<u64>,
    pub value_count: Option<u64>,
    pub key_length: Option<u64>,
    pub value_length: Option<u64>,
    /// Hybrid models: `true` for layers that are linear/recurrent attention
    /// (fixed-size state, no per-token KV growth).
    pub recurrent_layers: Option<Vec<bool>>,
    /// Full attention only every `interval`-th layer.
    pub full_attention_interval: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    U64(u64),
    I64(i64),
    Str(String),
    Bool(bool),
    Array(Vec<Value>),
}

fn is_wanted(key: &str) -> bool {
    key == "general.architecture"
        || key.ends_with(".block_count")
        || key.ends_with(".embedding_length")
        || key.ends_with(".attention.head_count")
        || key.ends_with(".attention.head_count_kv")
        || key.ends_with(".attention.head_size")
        || key.ends_with(".attention.key_count")
        || key.ends_with(".attention.value_count")
        || key.ends_with(".attention.key_length")
        || key.ends_with(".attention.value_length")
        || key.ends_with(".attention.recurrent_layers")
        || key.ends_with(".full_attention_interval")
}

/// Parse a GGUF file header (magic + KV metadata only, no tensors) from any
/// reader. Only the keys in [`GgufInfo`] are materialized; the rest is
/// skipped by size so a huge tokenizer blob costs one read, not one alloc.
pub fn parse_gguf_info<R: Read>(mut r: R) -> Result<GgufInfo, String> {
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    read_into(&mut r, &mut buf4)?;
    if &buf4 != b"GGUF" {
        return Err("not a GGUF file (bad magic)".into());
    }
    read_into(&mut r, &mut buf4)?;
    let version = u32::from_le_bytes(buf4);
    if version != 2 && version != 3 {
        return Err(format!("unsupported GGUF version {version}"));
    }
    read_into(&mut r, &mut buf8)?; // tensor_count: not needed
    read_into(&mut r, &mut buf8)?;
    let n_kv = u64::from_le_bytes(buf8);
    if n_kv > 100_000 {
        return Err("implausible GGUF metadata size".into());
    }

    let mut info = GgufInfo::default();
    for _ in 0..n_kv {
        read_into(&mut r, &mut buf8)?;
        let klen = usize::try_from(u64::from_le_bytes(buf8)).map_err(|_| "GGUF key too long")?;
        if klen > (1 << 20) {
            return Err("GGUF key too long".into());
        }
        let mut key = vec![0u8; klen];
        read_into(&mut r, &mut key)?;
        let key = String::from_utf8_lossy(&key).into_owned();
        read_into(&mut r, &mut buf4)?;
        let vtype = u32::from_le_bytes(buf4);
        if is_wanted(&key) {
            let val = read_value(&mut r, vtype)?;
            apply_key(&mut info, &key, val);
        } else {
            skip_value(&mut r, vtype)?;
        }
    }
    Ok(info)
}

/// Read a GGUF header from a file (incremental: only the header is read,
/// the file may be tens of GB).
pub fn read_gguf_info(path: &Path) -> Result<GgufInfo, String> {
    let file =
        fs::File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    parse_gguf_info(file)
}

/// Parsed GGUF headers, keyed by path with the file's mtime at parse time.
/// Parsing a real model header takes ~0.7 s (the tokenizer arrays are
/// hundreds of thousands of small elements, one read each) — the GUI polls
/// every couple of seconds, so re-parsing the same file each time froze the
/// window. Model metadata is immutable in practice; a changed mtime
/// invalidates the entry.
static GGUF_INFO_CACHE: Lazy<Mutex<HashMap<String, (SystemTime, GgufInfo)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// [`read_gguf_info`] with caching: returns the cached parse when the file's
/// mtime is unchanged.
pub fn read_gguf_info_cached(path: &Path) -> Result<GgufInfo, String> {
    let mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let key = path.to_string_lossy().into_owned();
    if let Some(m) = &mtime {
        let cache = GGUF_INFO_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, info)) = cache.get(&key).filter(|(t, _)| t == m) {
            return Ok(info.clone());
        }
    }
    let info = read_gguf_info(path)?;
    if let Some(m) = mtime {
        GGUF_INFO_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (m, info.clone()));
    }
    Ok(info)
}

fn read_into<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), String> {
    r.read_exact(buf)
        .map_err(|e| format!("GGUF read error: {e}"))
}

/// Scalar size in bytes for non-string, non-array GGUF value types.
fn scalar_size(ty: u32) -> Option<usize> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4 | 5 | 6 => Some(4),
        10 | 11 | 12 => Some(8),
        _ => None,
    }
}

fn read_value<R: Read>(r: &mut R, ty: u32) -> Result<Value, String> {
    match ty {
        0 => Ok(Value::U64(u8::from_le_bytes([read1(r)?]) as u64)),
        1 => Ok(Value::I64(i8::from_le_bytes([read1(r)?]) as i64)),
        2 => Ok(Value::U64(u16::from_le_bytes(read2(r)?) as u64)),
        3 => Ok(Value::I64(i16::from_le_bytes(read2(r)?) as i64)),
        4 => Ok(Value::U64(u32::from_le_bytes(read4(r)?) as u64)),
        5 => Ok(Value::I64(i32::from_le_bytes(read4(r)?) as i64)),
        6 => {
            let _ = read4(r)?;
            Ok(Value::U64(0)) // float32: not needed
        }
        7 => Ok(Value::Bool(read1(r)? != 0)),
        8 => {
            let n = u64::from_le_bytes(read8(r)?);
            if n > (1 << 26) {
                return Err("GGUF string too long".into());
            }
            let mut s = vec![0u8; n as usize];
            read_into(r, &mut s)?;
            Ok(Value::Str(String::from_utf8_lossy(&s).into_owned()))
        }
        9 => {
            let elem = u32::from_le_bytes(read4(r)?);
            let count = u64::from_le_bytes(read8(r)?);
            if count > 10_000_000 {
                return Err("GGUF array too large".into());
            }
            let mut items = Vec::with_capacity(count as usize);
            for _ in 0..count {
                items.push(read_value(r, elem)?);
            }
            Ok(Value::Array(items))
        }
        10 => Ok(Value::U64(u64::from_le_bytes(read8(r)?))),
        11 => Ok(Value::I64(i64::from_le_bytes(read8(r)?))),
        12 => {
            let _ = read8(r)?;
            Ok(Value::U64(0)) // float64: not needed
        }
        other => Err(format!("unsupported GGUF value type {other}")),
    }
}

fn skip_n(r: &mut impl Read, n: u64) -> Result<(), String> {
    let mut buf = [0u8; 8192];
    let mut remaining = n;
    while remaining > 0 {
        let to_read = std::cmp::min(remaining, buf.len() as u64) as usize;
        let read = r.read(&mut buf[..to_read]).map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("unexpected end of GGUF header".into());
        }
        remaining -= read as u64;
    }
    Ok(())
}

fn skip_value<R: Read>(r: &mut R, ty: u32) -> Result<(), String> {
    match ty {
        8 => {
            let n = u64::from_le_bytes(read8(r)?);
            if n > (1 << 26) {
                return Err("GGUF string too long".into());
            }
            skip_n(r, n)
        }
        9 => {
            let elem = u32::from_le_bytes(read4(r)?);
            let count = u64::from_le_bytes(read8(r)?);
            if count > 10_000_000 {
                return Err("GGUF array too large".into());
            }
            for _ in 0..count {
                skip_value(r, elem)?;
            }
            Ok(())
        }
        _ => {
            let n = scalar_size(ty).ok_or_else(|| format!("unsupported GGUF value type {ty}"))?;
            skip_n(r, n as u64)
        }
    }
}

fn read1<R: Read>(r: &mut R) -> Result<u8, String> {
    let mut b = [0u8; 1];
    read_into(r, &mut b)?;
    Ok(b[0])
}
fn read2<R: Read>(r: &mut R) -> Result<[u8; 2], String> {
    let mut b = [0u8; 2];
    read_into(r, &mut b)?;
    Ok(b)
}
fn read4<R: Read>(r: &mut R) -> Result<[u8; 4], String> {
    let mut b = [0u8; 4];
    read_into(r, &mut b)?;
    Ok(b)
}
fn read8<R: Read>(r: &mut R) -> Result<[u8; 8], String> {
    let mut b = [0u8; 8];
    read_into(r, &mut b)?;
    Ok(b)
}

fn as_u64(v: Value) -> Option<u64> {
    match v {
        Value::U64(n) => Some(n),
        Value::I64(n) if n >= 0 => Some(n as u64),
        _ => None,
    }
}

fn apply_key(info: &mut GgufInfo, key: &str, val: Value) {
    if key == "general.architecture" {
        if let Value::Str(s) = val {
            info.arch = Some(s);
        }
        return;
    }
    if key.ends_with(".attention.recurrent_layers") {
        if let Value::Array(items) = val {
            info.recurrent_layers = Some(
                items
                    .into_iter()
                    .filter_map(|v| match v {
                        Value::Bool(b) => Some(b),
                        _ => None,
                    })
                    .collect(),
            );
        }
        return;
    }
    let Some(n) = as_u64(val) else {
        return;
    };
    if key.ends_with(".block_count") {
        info.block_count = Some(n);
    } else if key.ends_with(".embedding_length") {
        info.embedding_length = Some(n);
    } else if key.ends_with(".attention.head_count") {
        info.head_count = Some(n);
    } else if key.ends_with(".attention.head_count_kv") {
        info.head_count_kv = Some(n);
    } else if key.ends_with(".attention.head_size") {
        info.head_size = Some(n);
    } else if key.ends_with(".attention.key_count") {
        info.key_count = Some(n);
    } else if key.ends_with(".attention.value_count") {
        info.value_count = Some(n);
    } else if key.ends_with(".attention.key_length") {
        info.key_length = Some(n);
    } else if key.ends_with(".attention.value_length") {
        info.value_length = Some(n);
    } else if key.ends_with(".full_attention_interval") {
        info.full_attention_interval = Some(n);
    }
}

// ---------------------------------------------------------------------------
// KV cache math
// ---------------------------------------------------------------------------

/// Full-attention layer count: for hybrid models only non-recurrent layers
/// grow KV per token.
fn full_attention_layers(info: &GgufInfo) -> Option<u64> {
    if let Some(rec) = &info.recurrent_layers {
        return Some(rec.iter().filter(|b| !**b).count() as u64);
    }
    let n = info.block_count?;
    if let Some(interval) = info.full_attention_interval {
        if interval > 1 && n >= interval {
            // layers i with i % interval == interval - 1
            return Some((n - interval) / interval + 1);
        }
    }
    Some(n)
}

/// Bytes per element of a quantized KV cache. `None` for unknown types.
fn kv_dtype_bytes(t: &str) -> Option<f64> {
    match t {
        "f16" | "bf16" => Some(2.0),
        "f32" => Some(4.0),
        "q8_0" => Some(33.0 / 32.0),
        "q4_0" => Some(17.0 / 32.0),
        _ => None,
    }
}

/// KV cache growth in bytes per token for the main model.
///
/// `k_bytes`/`v_bytes` are the per-element cache sizes (see
/// [`kv_dtype_bytes`]). The draft model's KV is excluded: spec-decoding
/// drafts (e.g. `dflash`) use a sliding window, so their KV is bounded by
/// the window rather than growing with the context.
pub fn kv_bytes_per_token(info: &GgufInfo, k_bytes: f64, v_bytes: f64) -> Option<u64> {
    let layers = full_attention_layers(info)?;
    if layers == 0 {
        return Some(0);
    }
    let heads = info.head_count?;
    let head_kv = info.head_count_kv.unwrap_or(heads);
    let key_count = info.key_count.unwrap_or(head_kv);
    let value_count = info.value_count.unwrap_or(key_count);
    let key_len = info
        .key_length
        .or(info.head_size)
        .or_else(|| {
            info.embedding_length
                .and_then(|e| info.head_count.map(|h| e / h.max(1)))
        })
        .filter(|&h| h > 0)?;
    let value_len = info.value_length.filter(|&h| h > 0).unwrap_or(key_len);
    let k = (key_count as f64 * key_len as f64 * k_bytes) as u64;
    let v = (value_count as f64 * value_len as f64 * v_bytes) as u64;
    Some(layers * (k + v))
}

/// Fraction of a model's file that lands on the GPU for a given
/// `--n-gpu-layers` value.
fn offload_ratio(ngl: Option<u32>, block_count: Option<u64>) -> f64 {
    match ngl {
        None => 1.0, // llama.cpp default: all layers on GPU
        Some(0) => 0.0,
        Some(n) => match block_count {
            Some(b) if b > 0 && (n as u64) < b => n as f64 / b as f64,
            // n >= 99 or unknown block count: assume full offload (the
            // conservative direction for a VRAM estimate).
            _ => 1.0,
        },
    }
}

/// Parse `<setups_dir>/<setup>.toml` without running the full
/// `validate_spec` (binary ownership checks etc. are irrelevant for the
/// slider, and a valid-but-oddly-owned setup should still be adjustable).
pub fn load_spec_raw(setup: &str) -> Result<SetupSpec, String> {
    let path = setups_dir().join(format!("{setup}.toml"));
    let content =
        fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    toml::from_str(&content).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}

/// Free VRAM of GPU 0 in MiB, via nvidia-smi.
pub fn query_free_vram_mib() -> Result<u64, String> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .output()
        .map_err(|e| format!("nvidia-smi not runnable: {e}"))?;
    if !out.status.success() {
        return Err("nvidia-smi exited with an error".into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().ok_or("nvidia-smi returned no output")?;
    first
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("nvidia-smi unexpected output: '{}'", first.trim()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct CtxEstimate {
    /// Slider maximum. Always >= [`MIN_CTX`].
    pub max_ctx: u32,
    /// `false` when the range is the generic fallback (estimation inputs
    /// missing) rather than a VRAM-derived number.
    pub estimated: bool,
    pub free_vram_mib: Option<u64>,
    pub weights_on_gpu_mib: u64,
    pub kv_bytes_per_token: Option<u64>,
    /// Non-fatal problems (unknown cache type, missing model file, …).
    pub warnings: Vec<String>,
}

/// Estimate the largest context window that fits for a setup, given the
/// currently free GPU VRAM.
///
/// Only hard errors (missing/unparseable setup file) return `Err`; missing
/// estimation inputs degrade to [`FALLBACK_MAX_CTX`] with a warning instead,
/// so the slider stays usable.
pub fn estimate_for_setup(setup: &str, free_vram_mib: Option<u64>) -> Result<CtxEstimate, String> {
    let spec = load_spec_raw(setup)?;
    let mut warnings = Vec::new();

    let free = match free_vram_mib {
        Some(f) => Some(f),
        None => {
            warnings.push("nvidia-smi unavailable — using generic max".into());
            None
        }
    };

    // --- main model: metadata + weight size ---
    let main_info = match read_gguf_info_cached(Path::new(&spec.model.main)) {
        Ok(i) => Some(i),
        Err(e) => {
            warnings.push(format!("main model metadata: {e}"));
            None
        }
    };
    let main_mib = file_mib(&spec.model.main, "main", &mut warnings);
    let main_offload = offload_ratio(
        spec.flags.ngl,
        main_info.as_ref().and_then(|i| i.block_count),
    );

    // --- draft model: weight size only (its KV is window-bounded) ---
    let draft_mib = spec
        .model
        .draft
        .as_deref()
        .map(|p| file_mib(p, "draft", &mut warnings))
        .unwrap_or(0);
    let draft_offload = spec
        .model
        .draft
        .as_ref()
        .and_then(|d| read_gguf_info_cached(Path::new(d)).ok())
        .and_then(|i| i.block_count)
        .map(|blocks| offload_ratio(spec.flags.ngld, Some(blocks)))
        .unwrap_or_else(|| offload_ratio(spec.flags.ngld, None));

    let weights_mib = (main_mib as f64 * main_offload + draft_mib as f64 * draft_offload) as u64;

    // --- per-token KV cost ---
    let cache_k = spec
        .flags
        .cache_type_k
        .clone()
        .unwrap_or_else(|| "f16".into());
    let cache_v = spec
        .flags
        .cache_type_v
        .clone()
        .unwrap_or_else(|| "f16".into());
    for (label, ty) in [
        ("cache_type_k", cache_k.as_str()),
        ("cache_type_v", cache_v.as_str()),
    ] {
        if kv_dtype_bytes(ty).is_none() {
            warnings.push(format!("{label} '{ty}' unknown — assuming f16"));
        }
    }
    let kb = kv_dtype_bytes(&cache_k).unwrap_or(2.0);
    let vb = kv_dtype_bytes(&cache_v).unwrap_or(2.0);
    let kv = main_info
        .as_ref()
        .and_then(|i| kv_bytes_per_token(i, kb, vb));
    if kv.is_none() && main_info.is_some() {
        warnings.push("KV size per token not derivable — using generic max".into());
    }

    let (max_ctx, estimated) = match (free, kv) {
        (Some(free), Some(kv)) if kv > 0 => {
            let usable = free
                .saturating_sub(SAFETY_MARGIN_MIB)
                .saturating_sub(weights_mib);
            let bytes = usable as u128 * MIB as u128;
            let ctx = (bytes / kv as u128) as u64;
            let rounded = ctx / CTX_ROUND * CTX_ROUND;
            if rounded < MIN_CTX as u64 {
                warnings.push(format!(
                    "estimated max ({rounded}) is below the {MIN_CTX} minimum — the model may not fit at the minimum context"
                ));
            }
            (rounded.max(MIN_CTX as u64) as u32, true)
        }
        _ => (FALLBACK_MAX_CTX, false),
    };

    Ok(CtxEstimate {
        max_ctx,
        estimated,
        free_vram_mib: free,
        weights_on_gpu_mib: weights_mib,
        kv_bytes_per_token: kv,
        warnings,
    })
}

fn file_mib(path: &str, label: &str, warnings: &mut Vec<String>) -> u64 {
    match fs::metadata(path) {
        Ok(md) => md.len().div_ceil(MIB),
        Err(_) => {
            warnings.push(format!("{label} model file not found: {path}"));
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Setup-file editing
// ---------------------------------------------------------------------------

/// Set `flags.ctx_size` in the setup TOML, preserving comments and the rest
/// of the file verbatim (a full re-serialization would drop comments).
///
/// Re-parses the result and verifies the new value before writing, so a
/// malformed file can never be clobbered.
pub fn update_ctx_size(setup: &str, ctx: u32) -> Result<(), String> {
    let path = setups_dir().join(format!("{setup}.toml"));
    let content =
        fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let mut replaced = false;

    for line in &mut lines {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("ctx_size") else {
            continue;
        };
        let Some(eq) = rest.find('=') else {
            continue;
        };
        let after = &rest[eq + 1..];
        // Keep a trailing comment if present.
        let (value_part, comment) = match after.find('#') {
            Some(ci) => (&after[..ci], Some(&after[ci..])),
            None => (after, None),
        };
        if value_part.trim().parse::<u32>().is_err() {
            continue; // not a plain integer; leave the line alone
        }
        let indent = &line[..line.len() - trimmed.len()];
        let mut new = format!("{indent}ctx_size = {ctx}");
        if let Some(c) = comment {
            new.push(' ');
            new.push_str(c);
        }
        *line = new;
        replaced = true;
        break;
    }

    if !replaced {
        // No `ctx_size` line: insert one after [flags], or append a section.
        if let Some(pos) = lines.iter().position(|l| l.trim() == "[flags]") {
            lines.insert(pos + 1, format!("ctx_size = {ctx}"));
        } else {
            if !lines.is_empty() && !content.ends_with('\n') {
                lines.push(String::new());
            }
            lines.push("[flags]".into());
            lines.push(format!("ctx_size = {ctx}"));
        }
    }

    let new_content = lines.join("\n") + "\n";

    // Verify before committing: the file must still parse and carry the value.
    let check: SetupSpec = toml::from_str(&new_content).map_err(|e| {
        format!(
            "Refusing to write {}: result does not parse: {e}",
            path.display()
        )
    })?;
    if check.flags.ctx_size != Some(ctx) {
        return Err(format!(
            "Refusing to write {}: ctx_size did not take effect",
            path.display()
        ));
    }
    // Atomic replace (write temp + rename): the GUI's background estimate
    // thread may be reading the file while the slider saves a new value, and
    // a plain truncate-in-place write could hand it a torn file.
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, &new_content).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Failed to replace {}: {e}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvSetGuard;

    // --- GGUF builder helpers ---

    fn str_body(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn gguf(entries: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // tensors
        v.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (k, t, b) in entries {
            v.extend_from_slice(&(k.len() as u64).to_le_bytes());
            v.extend_from_slice(k.as_bytes());
            v.extend_from_slice(&t.to_le_bytes());
            v.extend_from_slice(b);
        }
        v
    }

    fn u32_body(n: u32) -> Vec<u8> {
        n.to_le_bytes().to_vec()
    }

    fn bool_array_body(items: &[bool]) -> Vec<u8> {
        let mut v = 7u32.to_le_bytes().to_vec(); // elem type: bool
        v.extend_from_slice(&(items.len() as u64).to_le_bytes());
        for b in items {
            v.push(*b as u8);
        }
        v
    }

    fn llama_info_bytes() -> Vec<u8> {
        gguf(&[
            ("general.architecture", 8, str_body("llama")),
            ("llama.block_count", 4, u32_body(32)),
            ("llama.attention.head_count", 4, u32_body(32)),
            ("llama.attention.head_count_kv", 4, u32_body(8)),
            ("llama.attention.key_length", 4, u32_body(128)),
            ("llama.attention.value_length", 4, u32_body(128)),
            // unrelated key with a string value: must be skipped cleanly
            ("general.name", 8, str_body("Test Model")),
        ])
    }

    fn llama_info() -> GgufInfo {
        let bytes = llama_info_bytes();
        parse_gguf_info(&bytes[..]).unwrap()
    }

    #[test]
    fn test_parse_gguf_basic() {
        let info = llama_info();
        assert_eq!(info.arch.as_deref(), Some("llama"));
        assert_eq!(info.block_count, Some(32));
        assert_eq!(info.head_count, Some(32));
        assert_eq!(info.head_count_kv, Some(8));
        assert_eq!(info.key_length, Some(128));
        assert_eq!(info.value_length, Some(128));
        assert_eq!(info.recurrent_layers, None);
    }

    #[test]
    fn test_read_gguf_info_cached_hits_and_invalidates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        fs::write(&path, llama_info_bytes()).unwrap();
        let first = read_gguf_info_cached(&path).unwrap();
        // Same mtime → cache hit, identical result.
        let second = read_gguf_info_cached(&path).unwrap();
        assert_eq!(first, second);
        // Rewrite with block_count 32 → 24 (new mtime) → reparse must win.
        let key = b"llama.block_count";
        let mut changed = llama_info_bytes();
        let key_pos = changed
            .windows(key.len())
            .position(|w| w == key)
            .expect("block_count key in fixture");
        let val_pos = key_pos + key.len() + 4; // key bytes + vtype u32
        changed[val_pos..val_pos + 4].copy_from_slice(&24u32.to_le_bytes());
        fs::write(&path, changed).unwrap();
        let third = read_gguf_info_cached(&path).unwrap();
        assert_eq!(third.block_count, Some(24));
    }

    #[test]
    fn test_parse_gguf_rejects_bad_magic() {
        let mut v = vec![0u8; 32];
        v[0..4].copy_from_slice(b"XXXX");
        assert!(parse_gguf_info(&v[..]).is_err());
    }

    #[test]
    fn test_kv_bytes_f16() {
        // 32 layers × (8×128 K + 8×128 V) × 2 B = 131072 B/token
        assert_eq!(kv_bytes_per_token(&llama_info(), 2.0, 2.0), Some(131072));
    }

    #[test]
    fn test_kv_bytes_q8() {
        // 32 × 2048 elements × 33/32 B = 67584 B/token
        // 32 × 2048 elements × 33/32 B = 67584 B/token
        assert_eq!(
            kv_bytes_per_token(&llama_info(), 33.0 / 32.0, 33.0 / 32.0),
            Some(67584)
        );
    }

    #[test]
    fn test_kv_bytes_hybrid_counts_only_full_attention() {
        let bytes = gguf(&[
            ("general.architecture", 8, str_body("qwen35")),
            ("qwen35.block_count", 4, u32_body(8)),
            ("qwen35.attention.head_count", 4, u32_body(4)),
            ("qwen35.attention.head_count_kv", 4, u32_body(2)),
            ("qwen35.attention.key_length", 4, u32_body(64)),
            ("qwen35.attention.value_length", 4, u32_body(64)),
            // 8 layers, only indices 3 and 7 are full attention
            (
                "qwen35.attention.recurrent_layers",
                9,
                bool_array_body(&[true, true, true, false, true, true, true, false]),
            ),
        ]);
        let info = parse_gguf_info(&bytes[..]).unwrap();
        assert_eq!(full_attention_layers(&info), Some(2));
        // 2 full-attention layers × (2×64 K + 2×64 V) elements × 2 B = 1024 B/token
        assert_eq!(kv_bytes_per_token(&info, 2.0, 2.0), Some(1024));
    }

    #[test]
    fn test_full_attention_interval() {
        let bytes = gguf(&[
            ("general.architecture", 8, str_body("x")),
            ("x.block_count", 4, u32_body(65)),
            ("x.full_attention_interval", 4, u32_body(4)),
        ]);
        let info = parse_gguf_info(&bytes[..]).unwrap();
        // layers i with i % 4 == 3 → 3,7,…,63 = 16 layers
        assert_eq!(full_attention_layers(&info), Some(16));
    }

    #[test]
    fn test_offload_ratio() {
        assert_eq!(offload_ratio(None, Some(32)), 1.0);
        assert_eq!(offload_ratio(Some(0), Some(32)), 0.0);
        assert_eq!(offload_ratio(Some(16), Some(32)), 0.5);
        // 99 layers > 32 blocks: full offload
        assert_eq!(offload_ratio(Some(99), Some(32)), 1.0);
        // unknown block count: assume full offload
        assert_eq!(offload_ratio(Some(99), None), 1.0);
    }

    // --- estimation ---

    fn with_setup_env(dir: &std::path::Path, f: impl FnOnce()) {
        let _mutex = crate::test_support::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvSetGuard::set("LLM_SERVER_INDICATOR_SETUPS_DIR", dir);
        f();
    }

    #[test]
    fn test_estimate_from_vram() {
        let dir = tempfile::tempdir().unwrap();
        let gguf_path = dir.path().join("model.gguf");
        fs::write(
            &gguf_path,
            gguf(&[
                ("general.architecture", 8, str_body("llama")),
                ("llama.block_count", 4, u32_body(32)),
                ("llama.attention.head_count", 4, u32_body(32)),
                ("llama.attention.head_count_kv", 4, u32_body(8)),
                ("llama.attention.key_length", 4, u32_body(128)),
                ("llama.attention.value_length", 4, u32_body(128)),
            ]),
        )
        .unwrap();
        let toml = format!(
            r#"[server]
binary = "/bin/llama-server"
host = "127.0.0.1"
port = 8080

[model]
main = "{}"

[flags]
ngl = 99
ctx_size = 8192
"#,
            gguf_path.display()
        );
        with_setup_env(dir.path(), || {
            fs::write(dir.path().join("t1.toml"), toml).unwrap();
            // usable = free − safety − weights (the fake GGUF counts as its
            // rounded-up size), then / 131072 B/token, rounded down to 256.
            let file_size = fs::metadata(&gguf_path).unwrap().len();
            let weights_mib = file_size.div_ceil(1024 * 1024);
            let usable = 8192u64 - SAFETY_MARGIN_MIB - weights_mib;
            let expected = (usable as u128 * 1048576 / 131072) as u64 / 256 * 256;
            let est = estimate_for_setup("t1", Some(8192)).unwrap();
            assert!(est.estimated);
            assert_eq!(est.weights_on_gpu_mib, weights_mib);
            assert_eq!(est.max_ctx, expected as u32);
            assert_eq!(est.kv_bytes_per_token, Some(131072));
            assert!(est.warnings.is_empty(), "warnings: {:?}", est.warnings);
        });
    }

    #[test]
    fn test_estimate_no_vram_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        with_setup_env(dir.path(), || {
            fs::write(
                dir.path().join("t2.toml"),
                "[server]\nbinary = \"/bin/llama-server\"\nport = 8080\n\n[model]\nmain = \"/nonexistent.gguf\"\n",
            )
            .unwrap();
            let est = estimate_for_setup("t2", None).unwrap();
            assert!(!est.estimated);
            assert_eq!(est.max_ctx, FALLBACK_MAX_CTX);
            assert!(!est.warnings.is_empty());
        });
    }

    #[test]
    fn test_estimate_missing_setup_errors() {
        let dir = tempfile::tempdir().unwrap();
        with_setup_env(dir.path(), || {
            let err = estimate_for_setup("nope", Some(8192)).unwrap_err();
            assert!(err.contains("nope.toml"), "unexpected error: {err}");
        });
    }

    #[test]
    fn test_estimate_below_minimum_warns() {
        let dir = tempfile::tempdir().unwrap();
        let gguf_path = dir.path().join("model.gguf");
        // 1 layer, big heads → 32768 B/token; 2048 MiB free → tiny max
        fs::write(
            &gguf_path,
            gguf(&[
                ("general.architecture", 8, str_body("llama")),
                ("llama.block_count", 4, u32_body(1)),
                ("llama.attention.head_count", 4, u32_body(32)),
                ("llama.attention.head_count_kv", 4, u32_body(32)),
                ("llama.attention.key_length", 4, u32_body(512)),
                ("llama.attention.value_length", 4, u32_body(512)),
            ]),
        )
        .unwrap();
        let toml = format!(
            r#"[server]
binary = "/bin/llama-server"
port = 8080

[model]
main = "{}"

[flags]
ngl = 99
"#,
            gguf_path.display()
        );
        with_setup_env(dir.path(), || {
            fs::write(dir.path().join("t3.toml"), toml).unwrap();
            // 1 × (32×512 + 32×512) × 2 B = 65536 B/token
            // (2048 − 1024) MiB / 65536 = 16384 → fine, still above min.
            // Use 1100 MiB free: (1100−1024) MiB / 65536 = 1216 < 6400.
            let est = estimate_for_setup("t3", Some(1100)).unwrap();
            assert!(est.estimated);
            assert_eq!(est.max_ctx, MIN_CTX);
            assert!(
                est.warnings
                    .iter()
                    .any(|w| w.contains("below the 6400 minimum")),
                "warnings: {:?}",
                est.warnings
            );
        });
    }

    // --- update_ctx_size ---

    fn base_toml() -> String {
        r#"# header comment that must survive
[server]
binary = "/bin/llama-server"
host = "127.0.0.1"
port = 8080

[model]
main = "/models/x.gguf"

[flags]
ngl = 99
ctx_size = 131072 # keep me
jinja = true
"#
        .into()
    }

    #[test]
    fn test_update_ctx_size_replaces_in_place() {
        let dir = tempfile::tempdir().unwrap();
        with_setup_env(dir.path(), || {
            fs::write(dir.path().join("s1.toml"), base_toml()).unwrap();
            update_ctx_size("s1", 65536).unwrap();
            let out = fs::read_to_string(dir.path().join("s1.toml")).unwrap();
            assert!(out.contains("ctx_size = 65536 # keep me"), "file: {out}");
            assert!(out.contains("# header comment that must survive"));
            assert!(out.contains("ngl = 99"));
        });
    }

    #[test]
    fn test_update_ctx_size_inserts_after_flags() {
        let dir = tempfile::tempdir().unwrap();
        with_setup_env(dir.path(), || {
            fs::write(
                dir.path().join("s2.toml"),
                base_toml().replace("ctx_size = 131072 # keep me\n", ""),
            )
            .unwrap();
            update_ctx_size("s2", 40960).unwrap();
            let out = fs::read_to_string(dir.path().join("s2.toml")).unwrap();
            assert!(out.contains("ctx_size = 40960"), "file: {out}");
            // inserted directly after the [flags] line
            let lines: Vec<&str> = out.lines().collect();
            let pos = lines.iter().position(|l| l.contains("ctx_size")).unwrap();
            assert_eq!(lines[pos - 1].trim(), "[flags]");
        });
    }

    #[test]
    fn test_update_ctx_size_appends_flags_section() {
        let dir = tempfile::tempdir().unwrap();
        with_setup_env(dir.path(), || {
            fs::write(
                dir.path().join("s3.toml"),
                "[server]\nbinary = \"/bin/llama-server\"\nport = 8080\n\n[model]\nmain = \"/models/x.gguf\"\n",
            )
            .unwrap();
            update_ctx_size("s3", 8192).unwrap();
            let out = fs::read_to_string(dir.path().join("s3.toml")).unwrap();
            assert!(out.contains("ctx_size = 8192"), "file: {out}");
            let spec: SetupSpec = toml::from_str(&out).unwrap();
            assert_eq!(spec.flags.ctx_size, Some(8192));
        });
    }

    /// Time one GGUF header parse (the cost the GUI used to pay per poll):
    /// `cargo test vram::tests::bench_gguf_header_parse -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_gguf_header_parse() {
        let path = Path::new("/home/arido/Documents/hf-models/qwen-3.8/Qwen3.8-27B-Q6_K.gguf");
        if !path.exists() {
            eprintln!("model not present, skipping");
            return;
        }
        let start = std::time::Instant::now();
        let info = read_gguf_info(path).unwrap();
        let cold = start.elapsed();
        let _ = read_gguf_info_cached(path).unwrap(); // cold: populates the cache
        let start = std::time::Instant::now();
        let info2 = read_gguf_info_cached(path).unwrap();
        let cached = start.elapsed();
        assert_eq!(info, info2);
        println!(
            "cold parse: {:.2?} | cached read: {:.2?} (kv_bytes/token f16: {:?})",
            cold,
            cached,
            kv_bytes_per_token(&info, 2.0, 2.0)
        );
    }

    /// Manual sanity check against the real setups on this machine:
    /// `cargo test vram -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_estimate_real_setups() {
        let free = query_free_vram_mib().ok();
        println!("free VRAM: {:?} MiB", free);
        for setup in ["qwen", "muse", "qwen-unc", "qwen35-3d-coder"] {
            match estimate_for_setup(setup, free) {
                Ok(est) => println!("{setup}: {est:?}"),
                Err(e) => println!("{setup}: {e}"),
            }
        }
    }

    #[test]
    fn test_update_ctx_size_refuses_unparseable_result() {
        let dir = tempfile::tempdir().unwrap();
        with_setup_env(dir.path(), || {
            fs::write(
                dir.path().join("s4.toml"),
                "[server]\nport = 8080\n\n[model]\nmain = \"/models/x.gguf\"\n",
            )
            .unwrap();
            // missing [server].binary → reparse after edit must fail
            let err = update_ctx_size("s4", 8192).unwrap_err();
            assert!(err.contains("does not parse"), "unexpected: {err}");
        });
    }
}
