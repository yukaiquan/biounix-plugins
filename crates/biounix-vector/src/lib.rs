// BioUnix 向量数据库模块（LanceDB + arrow-rs）
// 由 Rust 编写，通过 napi-rs 暴露给 Node.js / Electron 主进程
// 设计：每个知识库独立 LanceDB 目录（A 方案），LRU 连接池 + 路径分片，支持公司部署
#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use lancedb::{
    connect, Connection,
    index::Index,
    query::{ExecutableQuery, QueryBase},
    table::OptimizeAction,
};
use arrow_array::{
    types::Float32Type, Array, FixedSizeListArray, Float32Array, RecordBatch,
    RecordBatchIterator, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use napi::Result as NapiResult;

// ============ 全局状态 ============
//
// 设计要点（A 方案：每库独立目录）：
// 1. BASE_PATH 是根目录（如 appData/rust-vector-data），所有知识库在其下分片存放
// 2. 每个知识库独立 LanceDB Connection，路径 = base/shard_xx/space_<id>/
// 3. 连接池用 HashMap + 容量上限，LRU 淘汰冷库连接（避免数千库内存膨胀）
// 4. VECTOR_DIM 按 space_id 记录（不同库可用不同 embedding 模型/维度）

const MAX_OPEN_CONNECTIONS: usize = 32;

// shard 数量：256 个分片目录，单 shard 内知识库数可控
const SHARD_MASK: u64 = 0xff;
const SHARD_COUNT: usize = 256;

/// FNV-1a 64bit 哈希（无外部依赖，稳定快）
fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn shard_dir(space_id: &str) -> String {
    let h = fnv1a64(space_id);
    format!("shard_{:02x}", h & SHARD_MASK)
}

/// 知识库的 LanceDB 目录路径：<base>/shard_xx/space_<id>/
fn space_db_path(base: &str, space_id: &str) -> String {
    Path::new(base)
        .join(shard_dir(space_id))
        .join(format!("space_{space_id}"))
        .to_string_lossy()
        .into_owned()
}

struct State {
    base_path: String,
    /// space_id → Connection，LRU 简化：超容量时清空全部（LanceDB connect 很轻，<1ms）
    conns: HashMap<String, Connection>,
    /// space_id → vector_dim（建表时记录，upsert/search 时校验）
    dims: HashMap<String, u32>,
}

impl State {
    fn new() -> Self {
        Self {
            base_path: String::new(),
            conns: HashMap::new(),
            dims: HashMap::new(),
        }
    }
}

// 全局 tokio runtime + 状态（与 biounix-core 的 OnceLock 单例模式一致）
lazy_static::lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败")
    };
    static ref STATE: Mutex<State> = Mutex::new(State::new());
}

// ============ NAPI 对象 ============
//
// 注意：napi object 与 serde derive 不能同时用于同一 struct（napi-derive 会生成
// 冲突的 impl）。因此跨 JS 边界且需要 JSON 序列化的类型，拆成两个职责：
//   - serde struct：用于 Rust 内部 JSON 解析/序列化（VectorRecordInput / SearchResultOutput）
//   - napi object：用于直接返回给 JS 的对象（VectorDbStats）

/// upsert 输入记录（JS 传 JSON 字符串，serde 解析）
#[derive(serde::Deserialize, serde::Serialize)]
struct VectorRecordInput {
    id: String,
    chunk_id: String,
    space_id: String,
    vector: Vec<f32>,
    text: String,
    metadata: String, // JSON string
}

/// 搜索结果（Rust 序列化为 JSON 字符串返回 JS）
#[derive(serde::Serialize, serde::Deserialize)]
struct SearchResultOutput {
    chunk_id: String,
    score: f64,
    text: String,
    metadata: String,
    distance: f64,
}

#[napi(object)]
pub struct VectorDbStats {
    pub path: String,
    pub space_id: String,
    pub table_rows: i64,
    pub indexed: bool,
    pub vector_dim: Option<u32>,
    pub open_connections: u32,
    pub shard_count: u32,
}

// ============ 辅助错误构造 ============

fn err(msg: impl Into<String>) -> napi::Error {
    napi::Error::new(napi::Status::GenericFailure, msg.into())
}

// ============ 核心 NAPI 函数 ============

/// 初始化根目录（Node 侧 app ready 后调用一次，传入 appData/rust-vector-data）
/// 多次调用安全：相同 path 复用，不同 path 更新（已打开的连接会被清空）
#[napi]
pub fn vector_db_init(base_path: String) -> NapiResult<()> {
    let mut st = STATE.lock().unwrap();
    if st.base_path != base_path && !st.conns.is_empty() {
        // 根目录变了，清空旧连接
        st.conns.clear();
    }
    st.base_path = base_path;
    // 预创建 256 个 shard 目录（best effort，失败不致命）
    for i in 0..SHARD_COUNT {
        let dir = Path::new(&st.base_path).join(format!("shard_{i:02x}"));
        let _ = std::fs::create_dir_all(&dir);
    }
    Ok(())
}

/// 打开（或复用）某知识库的 Connection
/// 内部函数，所有公开 NAPI 函数调用前先 ensure_conn
fn ensure_conn(space_id: &str) -> NapiResult<Connection> {
    let mut st = STATE.lock().unwrap();
    if st.base_path.is_empty() {
        return Err(err("vector_db 未初始化，请先调用 vector_db_init"));
    }
    // LRU 简化：超容量清空全部（LanceDB connect <1ms，重建成本低）
    if st.conns.len() >= MAX_OPEN_CONNECTIONS && !st.conns.contains_key(space_id) {
        st.conns.clear();
    }
    let path = space_db_path(&st.base_path, space_id);
    std::fs::create_dir_all(&path).map_err(|e| err(format!("创建知识库目录失败 {path}: {e}")))?;
    if let Some(conn) = st.conns.get(space_id) {
        return Ok(conn.clone());
    }
    let conn = RUNTIME
        .block_on(async {
            connect(&path)
                .execute()
                .await
                .map_err(|e| err(format!("LanceDB connect 失败 {path}: {e}")))
        })?;
    st.conns.insert(space_id.to_string(), conn.clone());
    Ok(conn)
}

/// 确保表存在；vector_dim 决定 FixedSizeList 维度
/// 表名固定 "chunks"（每库独立目录，无需用 spaceId 区分表名）
#[napi]
pub fn vector_ensure_table(space_id: String, vector_dim: u32) -> NapiResult<()> {
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        let tables = conn
            .table_names()
            .execute()
            .await
            .map_err(|e| err(format!("列举表失败: {e}")))?;
        if !tables.contains(&"chunks".to_string()) {
            let schema = build_schema(vector_dim);
            let empty = build_empty_batch(&schema, vector_dim);
            // RecordBatchIterator 已实现 arrow_array::RecordBatchReader，
            // lancedb 的 IntoArrow blanket impl 自动满足，直接传 reader 无需 Box
            let batches: Vec<Result<RecordBatch, arrow_schema::ArrowError>> = vec![Ok(empty)];
            let reader = RecordBatchIterator::new(batches.into_iter(), Arc::new(schema));
            conn.create_table("chunks", reader)
                .execute()
                .await
                .map_err(|e| err(format!("创建表失败: {e}")))?;
        }
        Ok::<(), napi::Error>(())
    })?;
    STATE.lock().unwrap().dims.insert(space_id, vector_dim);
    Ok(())
}

/// 批量 upsert（先按 chunk_id 删除旧记录，再插入新记录）
/// records_json: 序列化的 VectorRecordInput 数组
#[napi]
pub fn vector_upsert_batch(space_id: String, records_json: String) -> NapiResult<u32> {
    let records: Vec<VectorRecordInput> =
        serde_json::from_str(&records_json).map_err(|e| err(format!("解析 records JSON 失败: {e}")))?;
    if records.is_empty() {
        return Ok(0);
    }
    let conn = ensure_conn(&space_id)?;
    let dim = STATE
        .lock()
        .unwrap()
        .dims
        .get(&space_id)
        .copied()
        .ok_or_else(|| err("vector_dim 未设置，请先调用 vector_ensure_table"))?;

    RUNTIME.block_on(async {
        let table = conn
            .open_table("chunks")
            .execute()
            .await
            .map_err(|e| err(format!("打开表失败: {e}")))?;

        // 1. 删除待 upsert 的 chunk_id（SQL IN 子句）
        let ids: Vec<String> = records.iter().map(|r| r.chunk_id.replace('\'', "''")).collect();
        let in_clause = ids
            .iter()
            .map(|i| format!("'{i}'"))
            .collect::<Vec<_>>()
            .join(",");
        table
            .delete(&format!("chunk_id IN ({in_clause})"))
            .await
            .map_err(|e| err(format!("删除旧记录失败: {e}")))?;

        // 2. 构造 RecordBatch 并插入
        let schema = build_schema(dim);
        let batch = build_batch(&records, dim, &schema)?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), Arc::new(schema));
        table
            .add(reader)
            .execute()
            .await
            .map_err(|e| err(format!("插入失败: {e}")))?;

        Ok(records.len() as u32)
    })
}

/// 创建向量索引（IVF-PQ 自动选择）
/// 建议在批量插入完成后调用，而非每次 upsert 都调
#[napi]
pub fn vector_create_index(space_id: String) -> NapiResult<()> {
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        let table = conn
            .open_table("chunks")
            .execute()
            .await
            .map_err(|e| err(format!("打开表失败: {e}")))?;
        // Index::Auto 让 LanceDB 根据数据量/维度自动选 IVF-PQ 或 HNSW
        table
            .create_index(&["vector"], Index::Auto)
            .execute()
            .await
            .map_err(|e| err(format!("创建索引失败: {e}")))?;
        Ok(())
    })
}

/// 优化索引（将未索引的新数据并入索引，批量写入后建议调用）
#[napi]
pub fn vector_optimize_index(space_id: String) -> NapiResult<()> {
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        let table = conn
            .open_table("chunks")
            .execute()
            .await
            .map_err(|e| err(format!("打开表失败: {e}")))?;
        // OptimizeAction::All = compact + prune + 重建索引（全量优化）
        // 注意：OptimizeAction::Index 需要 OptimizeOptions 参数，非空结构体
        table
            .optimize(OptimizeAction::All)
            .await
            .map_err(|e| err(format!("优化索引失败: {e}")))?;
        Ok(())
    })
}

/// 向量搜索：返回 top_k 最相似的 chunk
/// query_vector_json: 查询向量的 JSON 字符串（如 "[0.1,0.2,...]"）；top_k: 返回数量；filter: 可选 SQL 过滤
/// 返回 JSON 字符串（SearchResultOutput 数组）
#[napi]
pub fn vector_search(
    space_id: String,
    query_vector_json: String,
    top_k: u32,
    filter: Option<String>,
) -> NapiResult<String> {
    let query_vec: Vec<f32> =
        serde_json::from_str(&query_vector_json).map_err(|e| err(format!("解析 query_vector 失败: {e}")))?;
    if query_vec.is_empty() {
        return Err(err("query_vector 不能为空"));
    }
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        let table = conn
            .open_table("chunks")
            .execute()
            .await
            .map_err(|e| err(format!("打开表失败: {e}")))?;

        // Vec<f32> 实现 IntoQueryVector（lancedb query.rs:320），可直接传给 nearest_to
        let mut q = table
            .query()
            .nearest_to(query_vec)
            .map_err(|e| err(format!("构建 nearest_to 失败: {e}")))?
            .limit(top_k as usize);
        if let Some(f) = filter {
            q = q.only_if(f);
        }
        let stream = q
            .execute()
            .await
            .map_err(|e| err(format!("搜索失败: {e}")))?;
        // try_collect 需显式标注集合类型（futures 0.3 的 TryStreamExt）
        let batches: Vec<RecordBatch> = stream
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| err(format!("收集结果失败: {e}")))?;

        let mut out: Vec<SearchResultOutput> = Vec::new();
        for batch in batches {
            let n = batch.num_rows();
            let chunk_ids = batch
                .column_by_name("chunk_id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or_else(|| err("结果缺少 chunk_id 列"))?;
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let metas = batch
                .column_by_name("metadata")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // _distance 是 LanceDB 自动追加的 Float32 列
            let distances = batch
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

            for i in 0..n {
                let dist = distances.map(|d| d.value(i) as f64).unwrap_or(0.0);
                out.push(SearchResultOutput {
                    chunk_id: chunk_ids.value(i).to_string(),
                    score: 1.0 - dist,
                    text: texts.map(|t| t.value(i).to_string()).unwrap_or_default(),
                    metadata: metas.map(|m| m.value(i).to_string()).unwrap_or_default(),
                    distance: dist,
                });
            }
        }
        serde_json::to_string(&out).map_err(|e| err(format!("序列化结果失败: {e}")))
    })
}

/// 删除某 chunk_id 的记录（单条删除，比 upsert 轻量）
#[napi]
pub fn vector_delete_chunks(space_id: String, chunk_ids: Vec<String>) -> NapiResult<u32> {
    if chunk_ids.is_empty() {
        return Ok(0);
    }
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        let table = conn
            .open_table("chunks")
            .execute()
            .await
            .map_err(|e| err(format!("打开表失败: {e}")))?;
        let escaped: Vec<String> = chunk_ids
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect();
        let predicate = format!("chunk_id IN ({})", escaped.join(","));
        table
            .delete(&predicate)
            .await
            .map_err(|e| err(format!("删除失败: {e}")))?;
        Ok(chunk_ids.len() as u32)
    })
}

/// 删除整张表（知识库删除时调用），并关闭该库的 Connection
#[napi]
pub fn vector_drop_table(space_id: String) -> NapiResult<()> {
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        // drop_table 是 async fn（非 builder），直接 await，不带 .execute()
        conn.drop_table("chunks")
            .await
            .map_err(|e| err(format!("删除表失败: {e}")))?;
        Ok::<(), napi::Error>(())
    })?;
    // 清理状态
    let mut st = STATE.lock().unwrap();
    st.conns.remove(&space_id);
    st.dims.remove(&space_id);
    // 删除磁盘目录（LanceDB drop_table 只删表数据，目录残留需手动清理）
    let path = space_db_path(&st.base_path, &space_id);
    let _ = std::fs::remove_dir_all(&path);
    Ok(())
}

/// 统计信息（调试/前端显示）
#[napi]
pub fn vector_db_stats(space_id: String) -> NapiResult<VectorDbStats> {
    let conn = ensure_conn(&space_id)?;
    let st = STATE.lock().unwrap();
    let path = space_db_path(&st.base_path, &space_id);
    let dim = st.dims.get(&space_id).copied();
    let open_conns = st.conns.len() as u32;
    drop(st);

    RUNTIME.block_on(async {
        let table = conn
            .open_table("chunks")
            .execute()
            .await
            .map_err(|e| err(format!("打开表失败: {e}")))?;
        let rows = table.count_rows(None).await.unwrap_or(0) as i64;
        // 索引名约定：vector_idx（LanceDB Auto 索引默认命名）
        let idx_stats = table.index_stats("vector_idx").await.ok().flatten();
        let indexed = idx_stats.map(|s| s.num_indexed_rows > 0).unwrap_or(false);
        Ok(VectorDbStats {
            path,
            space_id,
            table_rows: rows,
            indexed,
            vector_dim: dim,
            open_connections: open_conns,
            shard_count: SHARD_COUNT as u32,
        })
    })
}

/// 测试连接（Node 侧诊断用）
#[napi]
pub fn vector_db_ping(space_id: String) -> NapiResult<bool> {
    let conn = ensure_conn(&space_id)?;
    RUNTIME.block_on(async {
        let _ = conn
            .table_names()
            .execute()
            .await
            .map_err(|e| err(format!("ping 失败: {e}")))?;
        Ok(true)
    })
}

// ============ 内部辅助 ============

fn build_schema(dim: u32) -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("chunk_id", DataType::Utf8, false),
        Field::new("space_id", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
        Field::new("text", DataType::Utf8, true),
        Field::new("metadata", DataType::Utf8, true),
    ])
}

/// 构造 0 行空 batch（仅用于建表时定义 schema）
fn build_empty_batch(schema: &Schema, dim: u32) -> RecordBatch {
    // arrow 54：RecordBatch::try_new 需要 Vec<ArrayRef> = Vec<Arc<dyn Array>>
    let arrays: Vec<Arc<dyn Array>> = vec![
        Arc::new(StringArray::new_null(0)),
        Arc::new(StringArray::new_null(0)),
        Arc::new(StringArray::new_null(0)),
        Arc::new(FixedSizeListArray::new_null(
            Arc::new(Field::new("item", DataType::Float32, true)),
            dim as i32,
            0,
        )),
        Arc::new(StringArray::new_null(0)),
        Arc::new(StringArray::new_null(0)),
    ];
    RecordBatch::try_new(Arc::new(schema.clone()), arrays).expect("构造空 batch 失败")
}

fn build_batch(
    records: &[VectorRecordInput],
    dim: u32,
    schema: &Schema,
) -> Result<RecordBatch, napi::Error> {
    let _n = records.len();

    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    let chunk_ids: Vec<&str> = records.iter().map(|r| r.chunk_id.as_str()).collect();
    let space_ids: Vec<&str> = records.iter().map(|r| r.space_id.as_str()).collect();
    let texts: Vec<Option<&str>> = records.iter().map(|r| Some(r.text.as_str())).collect();
    let metas: Vec<Option<&str>> = records.iter().map(|r| Some(r.metadata.as_str())).collect();

    // vector 列：FixedSizeList<Float32, dim>
    let vectors: Vec<Option<Vec<Option<f32>>>> = records
        .iter()
        .map(|r| {
            if r.vector.len() != dim as usize {
                None
            } else {
                Some(r.vector.iter().map(|v| Some(*v)).collect())
            }
        })
        .collect();
    let vector_array =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vectors, dim as i32);

    let arrays: Vec<Arc<dyn Array>> = vec![
        Arc::new(StringArray::from(ids)),
        Arc::new(StringArray::from(chunk_ids)),
        Arc::new(StringArray::from(space_ids)),
        Arc::new(vector_array),
        Arc::new(StringArray::from(texts)),
        Arc::new(StringArray::from(metas)),
    ];
    RecordBatch::try_new(Arc::new(schema.clone()), arrays)
        .map_err(|e| err(format!("构造 batch 失败: {e}")))
}

// ============ 单元测试 ============

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::tempdir;

    static SPACE_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_space(label: &str) -> String {
        let n = SPACE_SEQ.fetch_add(1, Ordering::SeqCst);
        format!("{label}-{n}")
    }

    /// 初始化临时 base 并返回 (TempDir, 唯一 space_id)
    fn setup() -> (tempfile::TempDir, String) {
        let dir = tempdir().unwrap();
        let base = dir.path().to_str().unwrap().to_string();
        vector_db_init(base).unwrap();
        (dir, unique_space("sp"))
    }

    fn one_rec(
        id: &str,
        chunk_id: &str,
        space_id: &str,
        vector: Vec<f32>,
        text: &str,
    ) -> String {
        serde_json::to_string(&VectorRecordInput {
            id: id.to_string(),
            chunk_id: chunk_id.to_string(),
            space_id: space_id.to_string(),
            vector,
            text: text.to_string(),
            metadata: "{}".to_string(),
        })
        .unwrap()
    }

    fn recs_json(space_id: &str, items: &[(Vec<f32>, &str)]) -> String {
        let arr: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, (v, t))| {
                one_rec(&format!("id{i}"), &format!("c{i}"), space_id, v.clone(), t)
            })
            .collect();
        format!("[{}]", arr.join(","))
    }

    #[test]
    fn fnv1a_stable() {
        // 相同输入相同输出
        assert_eq!(fnv1a64("space-abc"), fnv1a64("space-abc"));
        // 不同输入大概率不同
        assert_ne!(fnv1a64("space-abc"), fnv1a64("space-abd"));
    }

    #[test]
    fn shard_dir_format() {
        let d = shard_dir("test-space");
        assert!(d.starts_with("shard_"));
        assert_eq!(d.len(), 8); // "shard_" + 2 hex
    }

    #[test]
    fn space_db_path_structure() {
        let p = space_db_path("/data", "my-space");
        assert!(p.contains("shard_"));
        assert!(p.contains("space_my-space"));
        assert!(p.starts_with("/data/"));
    }

    #[test]
    fn shard_distribution() {
        // 验证 256 个分片能均匀分布一组模拟 space_id
        let mut shards = std::collections::HashSet::new();
        for i in 0..1000 {
            shards.insert(shard_dir(&format!("space-{i}")));
        }
        // 至少应分散到 50+ 个 shard（均匀性粗略检查）
        assert!(shards.len() >= 50, "分片分散不足: {}", shards.len());
    }

    // ============ 集成测试（真实 LanceDB 临时目录） ============

    #[test]
    #[serial]
    fn smoke_init_ping() {
        let (_dir, space) = setup();
        assert!(vector_db_ping(space).unwrap());
    }

    #[test]
    #[serial]
    fn ensure_table_stats_empty() {
        let (_dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        let s = vector_db_stats(space).unwrap();
        assert_eq!(s.table_rows, 0);
        assert_eq!(s.vector_dim, Some(4));
        assert!(!s.indexed);
    }

    #[test]
    #[serial]
    fn upsert_search_roundtrip() {
        let (_dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        let json = recs_json(
            &space,
            &[
                (vec![1.0, 0.0, 0.0, 0.0], "a"),
                (vec![0.0, 1.0, 0.0, 0.0], "b"),
                (vec![1.0, 1.0, 0.0, 0.0], "c"),
            ],
        );
        assert_eq!(vector_upsert_batch(space.clone(), json).unwrap(), 3);
        let q = serde_json::to_string(&vec![1.0f32, 0.0, 0.0, 0.0]).unwrap();
        let out = vector_search(space, q, 2, None).unwrap();
        let res: Vec<SearchResultOutput> = serde_json::from_str(&out).unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].chunk_id, "c0"); // [1,0,0,0] 最相似自身
    }

    #[test]
    #[serial]
    fn upsert_overwrite_same_chunk() {
        let (_dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        let j1 = recs_json(&space, &[(vec![1.0, 0.0, 0.0, 0.0], "v1")]);
        vector_upsert_batch(space.clone(), j1).unwrap();
        let j2 = one_rec("id0", "c0", &space, vec![0.0, 1.0, 0.0, 0.0], "v2");
        vector_upsert_batch(space.clone(), format!("[{j2}]")).unwrap();
        let s = vector_db_stats(space).unwrap();
        assert_eq!(s.table_rows, 1);
    }

    #[test]
    #[serial]
    fn dimension_mismatch_null_vector() {
        let (_dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        // vector.len=3 != dim=4 → build_batch 置 null，不应 panic
        let bad = one_rec("id0", "c0", &space, vec![1.0, 0.0, 0.0], "bad");
        vector_upsert_batch(space.clone(), format!("[{bad}]")).unwrap();
        let s = vector_db_stats(space).unwrap();
        assert_eq!(s.table_rows, 1);
    }

    #[test]
    #[serial]
    fn delete_chunks_verification() {
        let (_dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        let json = recs_json(
            &space,
            &[
                (vec![1.0, 0.0, 0.0, 0.0], "a"),
                (vec![0.0, 1.0, 0.0, 0.0], "b"),
                (vec![0.0, 0.0, 1.0, 0.0], "c"),
            ],
        );
        vector_upsert_batch(space.clone(), json).unwrap();
        assert_eq!(
            vector_delete_chunks(space.clone(), vec!["c1".to_string()]).unwrap(),
            1
        );
        let q = serde_json::to_string(&vec![0.0f32, 1.0, 0.0, 0.0]).unwrap();
        let out = vector_search(space, q, 10, None).unwrap();
        let res: Vec<SearchResultOutput> = serde_json::from_str(&out).unwrap();
        let ids: Vec<String> = res.iter().map(|r| r.chunk_id.clone()).collect();
        assert!(!ids.contains(&"c1".to_string()), "已删除的 chunk 仍被检索到");
        assert_eq!(res.len(), 2);
    }

    #[test]
    #[serial]
    fn drop_table_cleans_disk() {
        let (dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        vector_drop_table(space.clone()).unwrap();
        let base = dir.path().to_str().unwrap();
        let p = space_db_path(base, &space);
        assert!(!Path::new(&p).exists(), "drop_table 后目录未清理: {p}");
        assert!(STATE.lock().unwrap().conns.get(&space).is_none());
        assert!(STATE.lock().unwrap().dims.get(&space).is_none());
    }

    #[test]
    #[serial]
    fn create_index_and_optimize() {
        let (_dir, space) = setup();
        vector_ensure_table(space.clone(), 4).unwrap();
        let mut items = Vec::new();
        for i in 0..300 {
            let v = vec![i as f32 / 300.0, (i * 2) as f32 / 300.0, 0.0, 0.0];
            items.push((v, "x"));
        }
        let json = recs_json(&space, &items);
        vector_upsert_batch(space.clone(), json).unwrap();
        vector_create_index(space.clone()).unwrap();
        vector_optimize_index(space.clone()).unwrap();
        let q = serde_json::to_string(&vec![0.5f32, 0.5, 0.0, 0.0]).unwrap();
        let out = vector_search(space, q, 5, None).unwrap();
        let res: Vec<SearchResultOutput> = serde_json::from_str(&out).unwrap();
        assert_eq!(res.len(), 5);
    }

    #[test]
    #[serial]
    fn lru_eviction_no_panic() {
        let (_dir, _space) = setup();
        // 超过 MAX_OPEN_CONNECTIONS=32 触发 LRU 清空，不应 panic
        for i in 0..(MAX_OPEN_CONNECTIONS + 5) {
            let s = unique_space("lru");
            assert!(vector_db_ping(s).unwrap(), "LRU 清空时 panic at i={i}");
        }
    }

    #[test]
    #[serial]
    fn concurrent_upsert_different_spaces() {
        let (_dir, _space) = setup();
        let mut handles = vec![];
        for t in 0..4 {
            let s = unique_space("conc");
            handles.push(std::thread::spawn(move || {
                vector_ensure_table(s.clone(), 4).unwrap();
                let v = vec![t as f32, 0.0, 0.0, 0.0];
                let json = one_rec("id0", "c0", &s, v, "c");
                vector_upsert_batch(s, format!("[{json}]")).unwrap();
            }));
        }
        for h in handles {
            h.join().expect("并发 upsert 线程 panic");
        }
    }
}
