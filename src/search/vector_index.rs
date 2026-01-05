//! CVVI (Cass Vector Index) binary format definitions.
//!
//! Format overview (little-endian):
//!
//! Header (variable size):
//!   Magic: "CVVI" (4 bytes)
//!   Version: u16
//!   EmbedderID length: u16
//!   EmbedderID: bytes
//!   EmbedderRevision length: u16
//!   EmbedderRevision: bytes
//!   Dimension: u32
//!   Quantization: u8 (0=f32, 1=f16)
//!   Count: u32
//!   HeaderCRC32: u32 (CRC32 of header bytes before this field)
//!
//! Rows (fixed size per entry):
//!   MessageID: u64
//!   CreatedAtMs: i64
//!   AgentID: u32
//!   WorkspaceID: u32
//!   SourceID: u32
//!   Role: u8 (0=user, 1=assistant, 2=system, 3=tool)
//!   ChunkIdx: u8 (0 for single-chunk)
//!   VecOffset: u64 (offset into vector slab)
//!   ContentHash: [u8; 32] (SHA256 of canonical content)
//!
//! Vector slab:
//!   Count × Dimension × bytes_per_quant, contiguous, 32-byte aligned.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use half::f16;
use memmap2::Mmap;
use rusqlite::Connection;

use crate::search::query::SearchFilters;
use crate::sources::provenance::{LOCAL_SOURCE_ID, SourceFilter, SourceKind};
use crate::storage::sqlite::SqliteStorage;

pub const CVVI_MAGIC: [u8; 4] = *b"CVVI";
pub const CVVI_VERSION: u16 = 1;
pub const VECTOR_ALIGN_BYTES: usize = 32;
pub const ROW_SIZE_BYTES: usize = 70;
pub const VECTOR_INDEX_DIR: &str = "vector_index";

pub fn vector_index_path(data_dir: &Path, embedder_id: &str) -> PathBuf {
    data_dir
        .join(VECTOR_INDEX_DIR)
        .join(format!("index-{embedder_id}.cvvi"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantization {
    F32,
    F16,
}

impl Quantization {
    pub fn to_u8(self) -> u8 {
        match self {
            Quantization::F32 => 0,
            Quantization::F16 => 1,
        }
    }

    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Quantization::F32),
            1 => Ok(Quantization::F16),
            other => bail!("unknown quantization value: {other}"),
        }
    }

    pub fn bytes_per_component(self) -> usize {
        match self {
            Quantization::F32 => 4,
            Quantization::F16 => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CvviHeader {
    pub version: u16,
    pub embedder_id: String,
    pub embedder_revision: String,
    pub dimension: u32,
    pub quantization: Quantization,
    pub count: u32,
}

impl CvviHeader {
    pub fn new(
        embedder_id: impl Into<String>,
        embedder_revision: impl Into<String>,
        dimension: u32,
        quantization: Quantization,
        count: u32,
    ) -> Result<Self> {
        let header = Self {
            version: CVVI_VERSION,
            embedder_id: embedder_id.into(),
            embedder_revision: embedder_revision.into(),
            dimension,
            quantization,
            count,
        };
        header.validate()?;
        Ok(header)
    }

    pub fn validate(&self) -> Result<()> {
        let id_len = self.embedder_id.len();
        let rev_len = self.embedder_revision.len();
        if id_len > u16::MAX as usize {
            bail!("embedder_id is too long: {id_len}");
        }
        if rev_len > u16::MAX as usize {
            bail!("embedder_revision is too long: {rev_len}");
        }
        if self.dimension == 0 {
            bail!("dimension must be non-zero");
        }
        Ok(())
    }

    pub fn header_len_bytes(&self) -> Result<usize> {
        self.validate()?;
        let id_len = self.embedder_id.len();
        let rev_len = self.embedder_revision.len();
        let base = 4 + 2 + 2 + id_len + 2 + rev_len + 4 + 1 + 4 + 4;
        Ok(base)
    }

    pub fn write_to<W: Write>(&self, mut writer: W) -> Result<usize> {
        self.validate()?;
        let mut buf = Vec::new();

        buf.extend_from_slice(&CVVI_MAGIC);
        buf.extend_from_slice(&self.version.to_le_bytes());

        let id_bytes = self.embedder_id.as_bytes();
        let id_len = u16::try_from(id_bytes.len())
            .map_err(|_| anyhow!("embedder_id length out of range"))?;
        buf.extend_from_slice(&id_len.to_le_bytes());
        buf.extend_from_slice(id_bytes);

        let rev_bytes = self.embedder_revision.as_bytes();
        let rev_len = u16::try_from(rev_bytes.len())
            .map_err(|_| anyhow!("embedder_revision length out of range"))?;
        buf.extend_from_slice(&rev_len.to_le_bytes());
        buf.extend_from_slice(rev_bytes);

        buf.extend_from_slice(&self.dimension.to_le_bytes());
        buf.push(self.quantization.to_u8());
        buf.extend_from_slice(&self.count.to_le_bytes());

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&buf);
        let crc = hasher.finalize();

        writer.write_all(&buf)?;
        writer.write_all(&crc.to_le_bytes())?;
        Ok(buf.len() + 4)
    }

    pub fn read_from<R: Read>(mut reader: R) -> Result<Self> {
        let mut header_bytes = Vec::new();

        let magic =
            read_exact_array::<4, _>(&mut reader, &mut header_bytes).context("read CVVI magic")?;
        if magic != CVVI_MAGIC {
            bail!("invalid CVVI magic: {:?}", magic);
        }

        let version = read_u16_le(&mut reader, &mut header_bytes).context("read CVVI version")?;
        if version != CVVI_VERSION {
            bail!("unsupported CVVI version: {version}");
        }

        let id_len = read_u16_le(&mut reader, &mut header_bytes)
            .context("read embedder id length")? as usize;
        let id_bytes =
            read_exact_vec(&mut reader, id_len, &mut header_bytes).context("read embedder id")?;
        let embedder_id = String::from_utf8(id_bytes).context("embedder id is not valid UTF-8")?;

        let rev_len = read_u16_le(&mut reader, &mut header_bytes)
            .context("read embedder revision length")? as usize;
        let rev_bytes = read_exact_vec(&mut reader, rev_len, &mut header_bytes)
            .context("read embedder revision")?;
        let embedder_revision =
            String::from_utf8(rev_bytes).context("embedder revision is not valid UTF-8")?;

        let dimension = read_u32_le(&mut reader, &mut header_bytes).context("read dimension")?;
        let quantization_raw =
            read_u8(&mut reader, &mut header_bytes).context("read quantization")?;
        let quantization = Quantization::from_u8(quantization_raw)?;
        let count = read_u32_le(&mut reader, &mut header_bytes).context("read count")?;

        let crc_expected = read_u32_le_no_accum(&mut reader).context("read header crc")?;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_bytes);
        let crc_actual = hasher.finalize();
        if crc_actual != crc_expected {
            bail!("header CRC mismatch (expected {crc_expected:#010x}, got {crc_actual:#010x})");
        }

        let header = Self {
            version,
            embedder_id,
            embedder_revision,
            dimension,
            quantization,
            count,
        };
        header.validate()?;
        Ok(header)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorRow {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub chunk_idx: u8,
    pub vec_offset: u64,
    pub content_hash: [u8; 32],
}

impl VectorRow {
    pub fn to_bytes(&self) -> [u8; ROW_SIZE_BYTES] {
        let mut buf = [0u8; ROW_SIZE_BYTES];
        let mut offset = 0usize;

        buf[offset..offset + 8].copy_from_slice(&self.message_id.to_le_bytes());
        offset += 8;
        buf[offset..offset + 8].copy_from_slice(&self.created_at_ms.to_le_bytes());
        offset += 8;
        buf[offset..offset + 4].copy_from_slice(&self.agent_id.to_le_bytes());
        offset += 4;
        buf[offset..offset + 4].copy_from_slice(&self.workspace_id.to_le_bytes());
        offset += 4;
        buf[offset..offset + 4].copy_from_slice(&self.source_id.to_le_bytes());
        offset += 4;
        buf[offset] = self.role;
        offset += 1;
        buf[offset] = self.chunk_idx;
        offset += 1;
        buf[offset..offset + 8].copy_from_slice(&self.vec_offset.to_le_bytes());
        offset += 8;
        buf[offset..offset + 32].copy_from_slice(&self.content_hash);

        buf
    }

    pub fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        writer.write_all(&self.to_bytes())?;
        Ok(())
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        if buf.len() != ROW_SIZE_BYTES {
            bail!(
                "vector row size mismatch: expected {ROW_SIZE_BYTES}, got {}",
                buf.len()
            );
        }
        let mut offset = 0usize;
        let message_id = u64::from_le_bytes(buf[offset..offset + 8].try_into()?);
        offset += 8;
        let created_at_ms = i64::from_le_bytes(buf[offset..offset + 8].try_into()?);
        offset += 8;
        let agent_id = u32::from_le_bytes(buf[offset..offset + 4].try_into()?);
        offset += 4;
        let workspace_id = u32::from_le_bytes(buf[offset..offset + 4].try_into()?);
        offset += 4;
        let source_id = u32::from_le_bytes(buf[offset..offset + 4].try_into()?);
        offset += 4;
        let role = buf[offset];
        offset += 1;
        let chunk_idx = buf[offset];
        offset += 1;
        let vec_offset = u64::from_le_bytes(buf[offset..offset + 8].try_into()?);
        offset += 8;
        let content_hash = buf[offset..offset + 32].try_into()?;

        Ok(Self {
            message_id,
            created_at_ms,
            agent_id,
            workspace_id,
            source_id,
            role,
            chunk_idx,
            vec_offset,
            content_hash,
        })
    }

    pub fn read_from<R: Read>(mut reader: R) -> Result<Self> {
        let mut buf = [0u8; ROW_SIZE_BYTES];
        reader.read_exact(&mut buf)?;
        Self::from_bytes(&buf)
    }
}

#[derive(Debug, Clone)]
pub struct VectorEntry {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub chunk_idx: u8,
    pub content_hash: [u8; 32],
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticFilter {
    pub agents: Option<HashSet<u32>>,
    pub workspaces: Option<HashSet<u32>>,
    pub sources: Option<HashSet<u32>>,
    pub roles: Option<HashSet<u8>>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
}

impl SemanticFilter {
    pub fn matches(&self, row: &VectorRow) -> bool {
        if let Some(agents) = &self.agents
            && !agents.contains(&row.agent_id)
        {
            return false;
        }
        if let Some(workspaces) = &self.workspaces
            && !workspaces.contains(&row.workspace_id)
        {
            return false;
        }
        if let Some(sources) = &self.sources
            && !sources.contains(&row.source_id)
        {
            return false;
        }
        if let Some(roles) = &self.roles
            && !roles.contains(&row.role)
        {
            return false;
        }
        if let Some(from) = self.created_from
            && row.created_at_ms < from
        {
            return false;
        }
        if let Some(to) = self.created_to
            && row.created_at_ms > to
        {
            return false;
        }
        true
    }

    pub fn from_search_filters(filters: &SearchFilters, maps: &SemanticFilterMaps) -> Result<Self> {
        let agents = map_filter_set(&filters.agents, &maps.agent_slug_to_id);
        let workspaces = map_filter_set(&filters.workspaces, &maps.workspace_path_to_id);
        let sources = maps.sources_from_filter(&filters.source_filter)?;

        Ok(Self {
            agents,
            workspaces,
            sources,
            roles: None,
            created_from: filters.created_from,
            created_to: filters.created_to,
        })
    }

    pub fn with_roles(mut self, roles: Option<HashSet<u8>>) -> Self {
        self.roles = roles;
        self
    }
}

pub const ROLE_USER: u8 = 0;
pub const ROLE_ASSISTANT: u8 = 1;
pub const ROLE_SYSTEM: u8 = 2;
pub const ROLE_TOOL: u8 = 3;

pub fn role_code_from_str(role: &str) -> Option<u8> {
    match role.trim().to_lowercase().as_str() {
        "user" => Some(ROLE_USER),
        "assistant" | "agent" => Some(ROLE_ASSISTANT),
        "system" => Some(ROLE_SYSTEM),
        "tool" => Some(ROLE_TOOL),
        _ => None,
    }
}

pub fn parse_role_codes<I, S>(roles: I) -> Result<HashSet<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut set = HashSet::new();
    for role in roles {
        let role_str = role.as_ref();
        let code =
            role_code_from_str(role_str).ok_or_else(|| anyhow!("unknown role: {role_str}"))?;
        set.insert(code);
    }
    Ok(set)
}

#[derive(Debug, Clone)]
pub struct SemanticFilterMaps {
    agent_slug_to_id: HashMap<String, u32>,
    workspace_path_to_id: HashMap<String, u32>,
    source_id_to_id: HashMap<String, u32>,
    remote_source_ids: HashSet<u32>,
}

impl SemanticFilterMaps {
    pub fn from_storage(storage: &SqliteStorage) -> Result<Self> {
        Self::from_connection(storage.raw())
    }

    pub fn from_connection(conn: &Connection) -> Result<Self> {
        let mut agent_slug_to_id = HashMap::new();
        let mut stmt = conn.prepare("SELECT id, slug FROM agents")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let slug: String = row.get(1)?;
            Ok((id, slug))
        })?;
        for row in rows {
            let (id, slug) = row?;
            let id_u32 = u32::try_from(id).map_err(|_| anyhow!("agent id out of range"))?;
            agent_slug_to_id.insert(slug, id_u32);
        }

        let mut workspace_path_to_id = HashMap::new();
        let mut stmt = conn.prepare("SELECT id, path FROM workspaces")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let path: String = row.get(1)?;
            Ok((id, path))
        })?;
        for row in rows {
            let (id, path) = row?;
            let id_u32 = u32::try_from(id).map_err(|_| anyhow!("workspace id out of range"))?;
            workspace_path_to_id.insert(path, id_u32);
        }

        let mut source_id_to_id = HashMap::new();
        let mut remote_source_ids = HashSet::new();
        let mut stmt = conn.prepare("SELECT id, kind FROM sources")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let kind: String = row.get(1)?;
            Ok((id, kind))
        })?;
        for row in rows {
            let (id, kind) = row?;
            let id_u32 = source_id_hash(&id);
            if SourceKind::parse(&kind).is_none_or(|k| k.is_remote()) {
                remote_source_ids.insert(id_u32);
            }
            source_id_to_id.insert(id, id_u32);
        }

        Ok(Self {
            agent_slug_to_id,
            workspace_path_to_id,
            source_id_to_id,
            remote_source_ids,
        })
    }

    fn sources_from_filter(&self, filter: &SourceFilter) -> Result<Option<HashSet<u32>>> {
        let result = match filter {
            SourceFilter::All => None,
            SourceFilter::Local => Some(HashSet::from([self.source_id(LOCAL_SOURCE_ID)])),
            SourceFilter::Remote => Some(self.remote_source_ids.clone()),
            SourceFilter::SourceId(id) => Some(HashSet::from([self.source_id(id)])),
        };
        Ok(result)
    }

    fn source_id(&self, source_id: &str) -> u32 {
        self.source_id_to_id
            .get(source_id)
            .copied()
            .unwrap_or_else(|| source_id_hash(source_id))
    }
}

#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub message_id: u64,
    pub chunk_idx: u8,
    pub score: f32,
}

#[derive(Debug)]
pub struct VectorIndex {
    header: CvviHeader,
    rows: Vec<VectorRow>,
    vectors: VectorStorage,
}

#[derive(Debug)]
enum VectorStorage {
    F32(Vec<f32>),
    F16(Vec<f16>),
    Mmap {
        mmap: Mmap,
        offset: usize,
        len: usize,
    },
}

impl VectorIndex {
    pub fn build<I>(
        embedder_id: impl Into<String>,
        embedder_revision: impl Into<String>,
        dimension: usize,
        quantization: Quantization,
        entries: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = VectorEntry>,
    {
        if dimension == 0 {
            bail!("dimension must be non-zero");
        }
        let dimension_u32 =
            u32::try_from(dimension).map_err(|_| anyhow!("dimension out of range"))?;

        let entries: Vec<VectorEntry> = entries.into_iter().collect();
        let count_u32 =
            u32::try_from(entries.len()).map_err(|_| anyhow!("entry count out of range"))?;

        let mut rows = Vec::with_capacity(entries.len());
        let mut offset_bytes: usize = 0;
        let bytes_per = quantization.bytes_per_component();
        let vector_bytes = dimension
            .checked_mul(bytes_per)
            .ok_or_else(|| anyhow!("vector size overflow"))?;

        let vectors = match quantization {
            Quantization::F32 => {
                let mut slab = Vec::with_capacity(entries.len() * dimension);
                for entry in &entries {
                    if entry.vector.len() != dimension {
                        bail!(
                            "vector dimension mismatch: expected {}, got {}",
                            dimension,
                            entry.vector.len()
                        );
                    }
                    let vec_offset = u64::try_from(offset_bytes)
                        .map_err(|_| anyhow!("vector offset out of range"))?;
                    rows.push(VectorRow {
                        message_id: entry.message_id,
                        created_at_ms: entry.created_at_ms,
                        agent_id: entry.agent_id,
                        workspace_id: entry.workspace_id,
                        source_id: entry.source_id,
                        role: entry.role,
                        chunk_idx: entry.chunk_idx,
                        vec_offset,
                        content_hash: entry.content_hash,
                    });
                    slab.extend(entry.vector.iter().copied());
                    offset_bytes = offset_bytes
                        .checked_add(vector_bytes)
                        .ok_or_else(|| anyhow!("vector slab size overflow"))?;
                }
                VectorStorage::F32(slab)
            }
            Quantization::F16 => {
                let mut slab = Vec::with_capacity(entries.len() * dimension);
                for entry in &entries {
                    if entry.vector.len() != dimension {
                        bail!(
                            "vector dimension mismatch: expected {}, got {}",
                            dimension,
                            entry.vector.len()
                        );
                    }
                    let vec_offset = u64::try_from(offset_bytes)
                        .map_err(|_| anyhow!("vector offset out of range"))?;
                    rows.push(VectorRow {
                        message_id: entry.message_id,
                        created_at_ms: entry.created_at_ms,
                        agent_id: entry.agent_id,
                        workspace_id: entry.workspace_id,
                        source_id: entry.source_id,
                        role: entry.role,
                        chunk_idx: entry.chunk_idx,
                        vec_offset,
                        content_hash: entry.content_hash,
                    });
                    slab.extend(entry.vector.iter().map(|v| f16::from_f32(*v)));
                    offset_bytes = offset_bytes
                        .checked_add(vector_bytes)
                        .ok_or_else(|| anyhow!("vector slab size overflow"))?;
                }
                VectorStorage::F16(slab)
            }
        };

        let header = CvviHeader::new(
            embedder_id,
            embedder_revision,
            dimension_u32,
            quantization,
            count_u32,
        )?;

        let index = Self {
            header,
            rows,
            vectors,
        };
        index.validate()?;
        Ok(index)
    }

    pub fn load(path: &Path) -> Result<Self> {
        if cfg!(target_endian = "big") {
            bail!("CVVI load is only supported on little-endian targets");
        }

        let file = File::open(path).with_context(|| format!("open CVVI file {path:?}"))?;
        let metadata = file.metadata().context("read CVVI metadata")?;
        let file_len = metadata.len();
        if file_len == 0 {
            bail!("CVVI file is empty");
        }

        let mmap = unsafe { Mmap::map(&file).context("mmap CVVI file")? };
        let mut cursor = Cursor::new(&mmap[..]);
        let header = CvviHeader::read_from(&mut cursor).context("read CVVI header")?;
        let header_len = header.header_len_bytes()?;
        let rows_len = rows_size_bytes(header.count)?;
        let slab_offset = vector_slab_offset_bytes(header_len, header.count)?;
        let slab_size =
            vector_slab_size_bytes(header.count, header.dimension, header.quantization)?;

        let expected_len = slab_offset
            .checked_add(slab_size)
            .ok_or_else(|| anyhow!("CVVI file size overflow"))?;
        if file_len != expected_len as u64 {
            bail!(
                "CVVI file size mismatch (expected {}, got {})",
                expected_len,
                file_len
            );
        }

        let rows_start = header_len;
        let rows_end = rows_start
            .checked_add(rows_len)
            .ok_or_else(|| anyhow!("rows offset overflow"))?;
        let rows_bytes = mmap
            .get(rows_start..rows_end)
            .ok_or_else(|| anyhow!("rows out of bounds"))?;
        let mut rows = Vec::with_capacity(header.count as usize);
        for chunk in rows_bytes.chunks_exact(ROW_SIZE_BYTES) {
            rows.push(VectorRow::from_bytes(chunk)?);
        }
        if rows.len() != header.count as usize {
            bail!(
                "row count mismatch: expected {}, got {}",
                header.count,
                rows.len()
            );
        }

        validate_row_offsets(
            &rows,
            header.dimension as usize,
            header.quantization,
            slab_size,
        )?;

        let index = Self {
            header,
            rows,
            vectors: VectorStorage::Mmap {
                mmap,
                offset: slab_offset,
                len: slab_size,
            },
        };
        index.validate()?;
        Ok(index)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temp_path = path.with_extension("cvvi.tmp");
        let mut file = File::create(&temp_path)
            .with_context(|| format!("create temp CVVI file {temp_path:?}"))?;
        self.write_to(&mut file)?;
        file.sync_all().context("fsync CVVI temp file")?;
        sync_dir(parent).context("fsync CVVI directory")?;
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("rename CVVI temp file {temp_path:?}"))?;
        sync_dir(parent).context("fsync CVVI directory post-rename")?;
        Ok(())
    }

    pub fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        self.validate()?;
        let header_len = self.header.header_len_bytes()?;
        let written = self.header.write_to(&mut writer)?;
        if written != header_len {
            bail!("header length mismatch: expected {header_len}, wrote {written}");
        }

        for row in &self.rows {
            row.write_to(&mut writer)?;
        }

        let rows_len = rows_size_bytes(self.header.count)?;
        let slab_offset = vector_slab_offset_bytes(header_len, self.header.count)?;
        let padding_len = slab_offset
            .checked_sub(header_len + rows_len)
            .ok_or_else(|| anyhow!("padding length underflow"))?;
        if padding_len > 0 {
            writer.write_all(&vec![0u8; padding_len])?;
        }

        self.write_vectors_to(&mut writer)?;
        Ok(())
    }

    pub fn search_top_k(
        &self,
        query_vec: &[f32],
        k: usize,
        filter: Option<&SemanticFilter>,
    ) -> Result<Vec<VectorSearchResult>> {
        if query_vec.len() != self.header.dimension as usize {
            bail!(
                "query dimension mismatch: expected {}, got {}",
                self.header.dimension,
                query_vec.len()
            );
        }
        if k == 0 {
            return Ok(Vec::new());
        }

        let mut heap = std::collections::BinaryHeap::with_capacity(k + 1);
        for row in &self.rows {
            if let Some(filter) = filter
                && !filter.matches(row)
            {
                continue;
            }
            let score = self.dot_product_at(row.vec_offset, query_vec)?;
            heap.push(std::cmp::Reverse(ScoredEntry {
                score,
                message_id: row.message_id,
                chunk_idx: row.chunk_idx,
            }));
            if heap.len() > k {
                heap.pop();
            }
        }

        let mut results: Vec<VectorSearchResult> = heap
            .into_iter()
            .map(|entry| VectorSearchResult {
                message_id: entry.0.message_id,
                chunk_idx: entry.0.chunk_idx,
                score: entry.0.score,
            })
            .collect();
        results.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.message_id.cmp(&b.message_id))
        });
        Ok(results)
    }

    pub fn search_top_k_collapsed(
        &self,
        query_vec: &[f32],
        k: usize,
        filter: Option<&SemanticFilter>,
    ) -> Result<Vec<VectorSearchResult>> {
        if query_vec.len() != self.header.dimension as usize {
            bail!(
                "query dimension mismatch: expected {}, got {}",
                self.header.dimension,
                query_vec.len()
            );
        }
        if k == 0 {
            return Ok(Vec::new());
        }

        let mut best_by_message: HashMap<u64, VectorSearchResult> = HashMap::new();
        for row in &self.rows {
            if let Some(filter) = filter
                && !filter.matches(row)
            {
                continue;
            }
            let score = self.dot_product_at(row.vec_offset, query_vec)?;
            best_by_message
                .entry(row.message_id)
                .and_modify(|entry| {
                    if score > entry.score {
                        entry.score = score;
                        entry.chunk_idx = row.chunk_idx;
                    }
                })
                .or_insert(VectorSearchResult {
                    message_id: row.message_id,
                    chunk_idx: row.chunk_idx,
                    score,
                });
        }

        let mut results: Vec<VectorSearchResult> = best_by_message.into_values().collect();
        results.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.message_id.cmp(&b.message_id))
        });
        if results.len() > k {
            results.truncate(k);
        }
        Ok(results)
    }

    pub fn vector_at_f32(&self, row: &VectorRow) -> Result<Vec<f32>> {
        let dimension = self.header.dimension as usize;
        match &self.vectors {
            VectorStorage::F32(values) => {
                let start = vector_offset_to_index(row.vec_offset, 4)?;
                let end = start
                    .checked_add(dimension)
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let slice = values
                    .get(start..end)
                    .ok_or_else(|| anyhow!("vector slice out of bounds"))?;
                Ok(slice.to_vec())
            }
            VectorStorage::F16(values) => {
                let start = vector_offset_to_index(row.vec_offset, 2)?;
                let end = start
                    .checked_add(dimension)
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let slice = values
                    .get(start..end)
                    .ok_or_else(|| anyhow!("vector slice out of bounds"))?;
                Ok(slice.iter().map(|v| f32::from(*v)).collect())
            }
            VectorStorage::Mmap { mmap, offset, .. } => {
                let bytes_per = self.header.quantization.bytes_per_component();
                let base = offset
                    .checked_add(
                        usize::try_from(row.vec_offset)
                            .map_err(|_| anyhow!("vector offset out of range"))?,
                    )
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let byte_len = dimension
                    .checked_mul(bytes_per)
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let end = base
                    .checked_add(byte_len)
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let bytes = mmap
                    .get(base..end)
                    .ok_or_else(|| anyhow!("vector slice out of bounds"))?;
                match self.header.quantization {
                    Quantization::F32 => {
                        let slice = bytes_as_f32(bytes)?;
                        Ok(slice.to_vec())
                    }
                    Quantization::F16 => {
                        let slice = bytes_as_f16(bytes)?;
                        Ok(slice.iter().map(|v| f32::from(*v)).collect())
                    }
                }
            }
        }
    }

    pub fn header(&self) -> &CvviHeader {
        &self.header
    }

    pub fn rows(&self) -> &[VectorRow] {
        &self.rows
    }

    fn validate(&self) -> Result<()> {
        self.header.validate()?;
        if self.rows.len() != self.header.count as usize {
            bail!(
                "row count mismatch: expected {}, got {}",
                self.header.count,
                self.rows.len()
            );
        }

        let expected_slab = vector_slab_size_bytes(
            self.header.count,
            self.header.dimension,
            self.header.quantization,
        )?;
        let actual_slab = self.vectors.len_bytes(self.header.quantization)?;
        if expected_slab != actual_slab {
            bail!(
                "vector slab size mismatch: expected {}, got {}",
                expected_slab,
                actual_slab
            );
        }

        validate_row_offsets(
            &self.rows,
            self.header.dimension as usize,
            self.header.quantization,
            expected_slab,
        )?;
        Ok(())
    }

    fn write_vectors_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        if cfg!(target_endian = "big") {
            bail!("CVVI write is only supported on little-endian targets");
        }
        match &self.vectors {
            VectorStorage::F32(values) => {
                let bytes = f32_as_bytes(values);
                writer.write_all(bytes)?;
            }
            VectorStorage::F16(values) => {
                let bytes = f16_as_bytes(values);
                writer.write_all(bytes)?;
            }
            VectorStorage::Mmap { mmap, offset, len } => {
                let bytes = mmap
                    .get(*offset..offset + len)
                    .ok_or_else(|| anyhow!("vector slab out of bounds"))?;
                writer.write_all(bytes)?;
            }
        }
        Ok(())
    }

    fn dot_product_at(&self, vec_offset: u64, query: &[f32]) -> Result<f32> {
        match &self.vectors {
            VectorStorage::F32(values) => {
                let start = vector_offset_to_index(vec_offset, 4)?;
                let end = start
                    .checked_add(query.len())
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let slice = values
                    .get(start..end)
                    .ok_or_else(|| anyhow!("vector slice out of bounds"))?;
                Ok(dot_product(slice, query))
            }
            VectorStorage::F16(values) => {
                let start = vector_offset_to_index(vec_offset, 2)?;
                let end = start
                    .checked_add(query.len())
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let slice = values
                    .get(start..end)
                    .ok_or_else(|| anyhow!("vector slice out of bounds"))?;
                Ok(dot_product_f16(slice, query))
            }
            VectorStorage::Mmap { mmap, offset, len } => {
                let bytes_per = self.header.quantization.bytes_per_component();
                let base = offset
                    .checked_add(
                        usize::try_from(vec_offset)
                            .map_err(|_| anyhow!("vector offset out of range"))?,
                    )
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let byte_len = query
                    .len()
                    .checked_mul(bytes_per)
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let end = base
                    .checked_add(byte_len)
                    .ok_or_else(|| anyhow!("vector slice overflow"))?;
                let bytes = mmap
                    .get(base..end)
                    .ok_or_else(|| anyhow!("vector slice out of bounds"))?;
                if base + byte_len > offset + len {
                    bail!("vector slice out of bounds");
                }
                match self.header.quantization {
                    Quantization::F32 => {
                        let slice = bytes_as_f32(bytes)?;
                        Ok(dot_product(slice, query))
                    }
                    Quantization::F16 => {
                        let slice = bytes_as_f16(bytes)?;
                        Ok(dot_product_f16(slice, query))
                    }
                }
            }
        }
    }
}

pub fn rows_size_bytes(count: u32) -> Result<usize> {
    (count as usize)
        .checked_mul(ROW_SIZE_BYTES)
        .ok_or_else(|| anyhow!("row size overflow for count {count}"))
}

pub fn vector_slab_offset_bytes(header_len: usize, count: u32) -> Result<usize> {
    let rows_len = rows_size_bytes(count)?;
    let end = header_len
        .checked_add(rows_len)
        .ok_or_else(|| anyhow!("offset overflow"))?;
    Ok(align_up(end, VECTOR_ALIGN_BYTES))
}

pub fn vector_slab_size_bytes(
    count: u32,
    dimension: u32,
    quantization: Quantization,
) -> Result<usize> {
    let components = (count as usize)
        .checked_mul(dimension as usize)
        .ok_or_else(|| anyhow!("vector slab size overflow"))?;
    components
        .checked_mul(quantization.bytes_per_component())
        .ok_or_else(|| anyhow!("vector slab size overflow"))
}

fn align_up(value: usize, align: usize) -> usize {
    if align == 0 {
        return value;
    }
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

fn map_filter_set(keys: &HashSet<String>, map: &HashMap<String, u32>) -> Option<HashSet<u32>> {
    if keys.is_empty() {
        return None;
    }
    let mut set = HashSet::new();
    for key in keys {
        if let Some(id) = map.get(key) {
            set.insert(*id);
        }
    }
    Some(set)
}

fn source_id_hash(source_id: &str) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(source_id.as_bytes());
    hasher.finalize()
}

#[derive(Debug, Clone)]
struct ScoredEntry {
    score: f32,
    message_id: u64,
    chunk_idx: u8,
}

impl PartialEq for ScoredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score) == Ordering::Equal
            && self.message_id == other.message_id
            && self.chunk_idx == other.chunk_idx
    }
}

impl Eq for ScoredEntry {}

impl PartialOrd for ScoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.message_id.cmp(&other.message_id))
            .then_with(|| self.chunk_idx.cmp(&other.chunk_idx))
    }
}

impl VectorStorage {
    fn len_bytes(&self, quantization: Quantization) -> Result<usize> {
        match self {
            VectorStorage::F32(values) => {
                if quantization != Quantization::F32 {
                    bail!("vector storage quantization mismatch (expected f32)");
                }
                values
                    .len()
                    .checked_mul(4)
                    .ok_or_else(|| anyhow!("vector slab size overflow"))
            }
            VectorStorage::F16(values) => {
                if quantization != Quantization::F16 {
                    bail!("vector storage quantization mismatch (expected f16)");
                }
                values
                    .len()
                    .checked_mul(2)
                    .ok_or_else(|| anyhow!("vector slab size overflow"))
            }
            VectorStorage::Mmap { len, .. } => Ok(*len),
        }
    }
}

fn validate_row_offsets(
    rows: &[VectorRow],
    dimension: usize,
    quantization: Quantization,
    slab_size: usize,
) -> Result<()> {
    let bytes_per = quantization.bytes_per_component();
    let vector_bytes = dimension
        .checked_mul(bytes_per)
        .ok_or_else(|| anyhow!("vector size overflow"))?;
    for (idx, row) in rows.iter().enumerate() {
        let offset = usize::try_from(row.vec_offset)
            .map_err(|_| anyhow!("row {idx} vector offset out of range"))?;
        if offset % bytes_per != 0 {
            bail!("row {idx} vector offset not aligned");
        }
        let end = offset
            .checked_add(vector_bytes)
            .ok_or_else(|| anyhow!("row {idx} vector offset overflow"))?;
        if end > slab_size {
            bail!("row {idx} vector offset out of bounds");
        }
    }
    Ok(())
}

fn vector_offset_to_index(offset: u64, bytes_per: usize) -> Result<usize> {
    if bytes_per == 0 {
        bail!("bytes_per_component must be non-zero");
    }
    let bytes_per_u64 = bytes_per as u64;
    if !offset.is_multiple_of(bytes_per_u64) {
        bail!("vector offset is not aligned to component size");
    }
    let index = offset / bytes_per_u64;
    usize::try_from(index).map_err(|_| anyhow!("vector offset out of range"))
}

fn bytes_as_f32(bytes: &[u8]) -> Result<&[f32]> {
    if !bytes.len().is_multiple_of(4) {
        bail!("f32 byte slice length is not a multiple of 4");
    }
    // SAFETY: we validate length and alignment before using the slice as f32.
    let (prefix, aligned, suffix) = unsafe { bytes.align_to::<f32>() };
    if !prefix.is_empty() || !suffix.is_empty() {
        bail!("f32 byte slice is not aligned");
    }
    Ok(aligned)
}

fn bytes_as_f16(bytes: &[u8]) -> Result<&[f16]> {
    if !bytes.len().is_multiple_of(2) {
        bail!("f16 byte slice length is not a multiple of 2");
    }
    // SAFETY: we validate length and alignment before using the slice as f16.
    let (prefix, aligned, suffix) = unsafe { bytes.align_to::<f16>() };
    if !prefix.is_empty() || !suffix.is_empty() {
        bail!("f16 byte slice is not aligned");
    }
    Ok(aligned)
}

fn f32_as_bytes(values: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(values.as_ptr() as *const u8, values.len() * 4) }
}

fn f16_as_bytes(values: &[f16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(values.as_ptr() as *const u8, values.len() * 2) }
}

#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[inline]
fn dot_product_f16(a: &[f16], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| f32::from(*x) * y).sum()
}

fn sync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path)?;
    dir.sync_all()?;
    Ok(())
}

fn read_u8<R: Read>(reader: &mut R, header_bytes: &mut Vec<u8>) -> Result<u8> {
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf)?;
    header_bytes.extend_from_slice(&buf);
    Ok(buf[0])
}

fn read_u16_le<R: Read>(reader: &mut R, header_bytes: &mut Vec<u8>) -> Result<u16> {
    let buf = read_exact_array::<2, _>(reader, header_bytes)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32_le<R: Read>(reader: &mut R, header_bytes: &mut Vec<u8>) -> Result<u32> {
    let buf = read_exact_array::<4, _>(reader, header_bytes)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u32_le_no_accum<R: Read>(reader: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_exact_vec<R: Read>(
    reader: &mut R,
    len: usize,
    header_bytes: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    header_bytes.extend_from_slice(&buf);
    Ok(buf)
}

fn read_exact_array<const N: usize, R: Read>(
    reader: &mut R,
    header_bytes: &mut Vec<u8>,
) -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    reader.read_exact(&mut buf)?;
    header_bytes.extend_from_slice(&buf);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn sample_entries() -> Vec<VectorEntry> {
        vec![
            VectorEntry {
                message_id: 1,
                created_at_ms: 1000,
                agent_id: 1,
                workspace_id: 10,
                source_id: 100,
                role: 0,
                chunk_idx: 0,
                content_hash: [0x11; 32],
                vector: vec![1.0, 0.0, 0.0],
            },
            VectorEntry {
                message_id: 2,
                created_at_ms: 2000,
                agent_id: 1,
                workspace_id: 10,
                source_id: 100,
                role: 1,
                chunk_idx: 0,
                content_hash: [0x22; 32],
                vector: vec![0.0, 1.0, 0.0],
            },
            VectorEntry {
                message_id: 3,
                created_at_ms: 3000,
                agent_id: 2,
                workspace_id: 10,
                source_id: 100,
                role: 1,
                chunk_idx: 0,
                content_hash: [0x33; 32],
                vector: vec![0.0, 0.0, 1.0],
            },
        ]
    }

    #[test]
    fn header_roundtrip_and_crc() -> Result<()> {
        let header = CvviHeader::new("minilm-384", "e4ce9877", 384, Quantization::F16, 42)?;
        let mut bytes = Vec::new();
        header.write_to(&mut bytes)?;

        let parsed = CvviHeader::read_from(bytes.as_slice())?;
        assert_eq!(parsed, header);
        Ok(())
    }

    #[test]
    fn header_crc_detects_corruption() -> Result<()> {
        let header = CvviHeader::new("hash-256", "rev", 256, Quantization::F32, 1)?;
        let mut bytes = Vec::new();
        header.write_to(&mut bytes)?;

        // Flip a byte in the embedder id to break CRC.
        let mut corrupted = bytes.clone();
        if corrupted.len() > 8 {
            corrupted[8] ^= 0b0001_0000;
        }

        let result = CvviHeader::read_from(corrupted.as_slice());
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn row_roundtrip() -> Result<()> {
        let row = VectorRow {
            message_id: 7,
            created_at_ms: 1234,
            agent_id: 2,
            workspace_id: 3,
            source_id: 4,
            role: 1,
            chunk_idx: 0,
            vec_offset: 128,
            content_hash: [0xAB; 32],
        };

        let bytes = row.to_bytes();
        let parsed = VectorRow::from_bytes(&bytes)?;
        assert_eq!(parsed, row);
        Ok(())
    }

    #[test]
    fn vector_slab_offset_is_aligned() -> Result<()> {
        let header = CvviHeader::new("id", "rev", 128, Quantization::F16, 3)?;
        let header_len = header.header_len_bytes()?;
        let offset = vector_slab_offset_bytes(header_len, header.count)?;
        assert_eq!(offset % VECTOR_ALIGN_BYTES, 0);
        Ok(())
    }

    #[test]
    fn index_roundtrip_save_load() -> Result<()> {
        let entries = sample_entries();
        let index = VectorIndex::build("hash-3", "rev", 3, Quantization::F32, entries)?;
        let dir = tempdir()?;
        let path = dir.path().join("index.cvvi");
        index.save(&path)?;

        let loaded = VectorIndex::load(&path)?;
        assert_eq!(loaded.header(), index.header());
        assert_eq!(loaded.rows(), index.rows());
        for row in loaded.rows() {
            let original = index.vector_at_f32(row)?;
            let roundtrip = loaded.vector_at_f32(row)?;
            assert_eq!(original, roundtrip);
        }
        Ok(())
    }

    #[test]
    fn search_respects_filter() -> Result<()> {
        let entries = sample_entries();
        let index = VectorIndex::build("hash-3", "rev", 3, Quantization::F32, entries)?;
        let filter = SemanticFilter {
            agents: Some(HashSet::from([2])),
            ..Default::default()
        };
        let results = index.search_top_k(&[0.0, 0.0, 1.0], 5, Some(&filter))?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, 3);
        Ok(())
    }

    #[test]
    fn f16_and_f32_rankings_match() -> Result<()> {
        let entries = sample_entries();
        let index_f32 = VectorIndex::build("hash-3", "rev", 3, Quantization::F32, entries.clone())?;
        let index_f16 = VectorIndex::build("hash-3", "rev", 3, Quantization::F16, entries)?;
        let query = [0.9, 0.1, -0.2];
        let results_f32 = index_f32.search_top_k(&query, 3, None)?;
        let results_f16 = index_f16.search_top_k(&query, 3, None)?;
        let ids_f32: Vec<u64> = results_f32.iter().map(|r| r.message_id).collect();
        let ids_f16: Vec<u64> = results_f16.iter().map(|r| r.message_id).collect();
        assert_eq!(ids_f16, ids_f32);
        Ok(())
    }

    #[test]
    fn semantic_filter_from_search_filters_maps_ids() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE agents (id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
            CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT NOT NULL);
            CREATE TABLE sources (id TEXT PRIMARY KEY, kind TEXT NOT NULL);
            INSERT INTO agents (id, slug) VALUES (1, 'codex'), (2, 'claude');
            INSERT INTO workspaces (id, path) VALUES (10, '/ws/alpha');
            INSERT INTO sources (id, kind) VALUES ('local', 'local'), ('laptop', 'ssh');
            "#,
        )?;

        let maps = SemanticFilterMaps::from_connection(&conn)?;
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".to_string());
        filters.workspaces.insert("/ws/alpha".to_string());
        filters.source_filter = SourceFilter::Remote;

        let semantic = SemanticFilter::from_search_filters(&filters, &maps)?;
        assert_eq!(semantic.agents, Some(HashSet::from([1])));
        assert_eq!(semantic.workspaces, Some(HashSet::from([10])));
        assert_eq!(
            semantic.sources,
            Some(HashSet::from([maps.source_id("laptop")]))
        );
        Ok(())
    }

    #[test]
    fn role_code_parsing_accepts_known_roles() -> Result<()> {
        let roles = parse_role_codes(["user", "assistant", "system", "tool"])?;
        assert!(roles.contains(&ROLE_USER));
        assert!(roles.contains(&ROLE_ASSISTANT));
        assert!(roles.contains(&ROLE_SYSTEM));
        assert!(roles.contains(&ROLE_TOOL));
        Ok(())
    }

    #[test]
    fn role_code_parsing_rejects_unknown_roles() {
        let err = parse_role_codes(["unknown"]);
        assert!(err.is_err());
    }

    #[test]
    fn search_respects_role_filter() -> Result<()> {
        // Sample entries have: msg1=role 0 (user), msg2=role 1 (assistant), msg3=role 1 (assistant)
        let entries = sample_entries();
        let index = VectorIndex::build("hash-3", "rev", 3, Quantization::F32, entries)?;

        // Filter to user role only (role 0)
        let filter = SemanticFilter {
            roles: Some(HashSet::from([ROLE_USER])),
            ..Default::default()
        };
        let results = index.search_top_k(&[1.0, 0.0, 0.0], 5, Some(&filter))?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, 1); // Only message with role 0

        // Filter to assistant role only (role 1)
        let filter = SemanticFilter {
            roles: Some(HashSet::from([ROLE_ASSISTANT])),
            ..Default::default()
        };
        let results = index.search_top_k(&[0.0, 1.0, 0.0], 5, Some(&filter))?;
        assert_eq!(results.len(), 2); // Messages 2 and 3 have role 1

        // Filter to both roles - should get all 3
        let filter = SemanticFilter {
            roles: Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT])),
            ..Default::default()
        };
        let results = index.search_top_k(&[0.5, 0.5, 0.0], 5, Some(&filter))?;
        assert_eq!(results.len(), 3);

        Ok(())
    }
}
