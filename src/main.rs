//! im_schemas_server —— 独立输入法码表查询服务（Rust 实现）。
//! 脱离 AstrBot，可直接在云服务器以单二进制运行：查编码、反查、多方案对比、词频、图文渲染。

mod algo;
mod cache;
mod db;
mod metrics;
mod render;
mod render_cache;
mod schema;
mod segment;

use crate::cache::{CodeLookup, SchemaCache};
use crate::db::Db;
use crate::metrics::Metrics;
use crate::render::{EnginePool, TextEngine};
use crate::render_cache::RenderCache;
use crate::segment::{compute_stats, query_all, query_segments, AllResult};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::RwLock;

/// 上传文件大小上限（32MB），防止超大码表把内存打爆。
const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

struct AppState {
    /// 包裹 std Mutex：导入是阻塞式的 SQLite 写入，放在 spawn_blocking 里执行，
    /// 避免占用 tokio 工作线程；std Mutex 可在阻塞闭包里直接加锁。
    db: Arc<StdMutex<Db>>,
    /// 渲染引擎池：多个独立 TextEngine，并发渲染各自独占引擎、不再争一把全局锁
    engines: Arc<EnginePool>,
    /// 码表缓存：方案名 -> 常驻内存的解析数据
    schemas: Arc<RwLock<HashMap<String, Arc<SchemaCache>>>>,
    /// 词频缓存：词频表名 -> (word -> freq)
    freqs: Arc<RwLock<HashMap<String, Arc<HashMap<String, String>>>>>,
    /// 渲染结果缓存：cache_key -> PNG 字节（LRU）
    render_cache: Arc<RenderCache>,
    /// 上传鉴权令牌（来自 AUTH_TOKEN 环境变量）；为空表示不启用鉴权。
    auth_token: Option<String>,
    /// 查询文本最大字符数（来自 MAX_QUERY_LEN，默认 512），防超长输入。
    max_query_len: usize,
    /// 上传串行锁：把「DB 导入 → 内存缓存重建/写回 → 渲染缓存失效」包成原子整体，
    /// 避免并发上传同名方案时缓存与 DB 落在不同版本上。
    upload_lock: Arc<tokio::sync::Mutex<()>>,
    /// 请求指标（请求数、状态码分布、累计耗时、在途数）。
    metrics: Arc<Metrics>,
}

/// 从 DB 读取某方案的码表，构建内存缓存（词频由独立的 freqs 注册表管理）。
fn build_schema_cache(db: &Db, name: &str) -> Option<Arc<SchemaCache>> {
    let info = db.get_schema(name).ok().flatten()?;
    let codes = db.load_codes(name).unwrap_or_default();
    let entry_count = codes.len();
    Some(Arc::new(SchemaCache::new(info, entry_count, codes)))
}

#[derive(Deserialize)]
struct QParams {
    q: Option<String>,
    fmt: Option<String>,
    force_single: Option<bool>,
    show_keyboard: Option<bool>,
    code: Option<String>,
    schemas: Option<String>,
}

/// 校验上传权限：未配置 AUTH_TOKEN 则放行；配置了则要求
/// `Authorization: Bearer <token>` 头或 `token` 表单/查询字段匹配。
fn auth_ok(st: &AppState, headers: &HeaderMap, form_token: Option<&str>) -> bool {
    match &st.auth_token {
        None => true,
        Some(expected) => {
            let header_token = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_start_matches("Bearer ").to_string());
            header_token.as_deref() == Some(expected.as_str())
                || form_token == Some(expected.as_str())
        }
    }
}

/// 查询文本长度保护：超长文本会生成巨型 JSON / 超宽 PNG，直接拒绝。
fn check_query_len(st: &AppState, q: &str) -> Option<Response> {
    if q.chars().count() > st.max_query_len {
        Some(err_resp(
            StatusCode::BAD_REQUEST,
            format!("查询文本过长（上限 {} 字）", st.max_query_len),
        ))
    } else {
        None
    }
}

/// 解析 multipart/form-data 上传体。
/// 直接用 multer 流式解析（axum 自带 Multipart 对单字段有 ~2MB 硬限制，大码表会被截断），
/// 并把每个字段的大小上限设为 MAX_UPLOAD_BYTES，超限即返回 413。
struct UploadFields {
    name: String,
    select_keys: String,
    max_len: i64,
    content: String,
    token: Option<String>,
}

async fn parse_upload(body: axum::body::Bytes, headers: &HeaderMap) -> Result<UploadFields, Response> {
    let ct = match headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        Some(c) => c.to_string(),
        None => return Err(err_resp(StatusCode::BAD_REQUEST, "缺少 Content-Type")),
    };
    let boundary = match multer::parse_boundary(&ct) {
        Ok(b) => b,
        Err(_) => return Err(err_resp(StatusCode::BAD_REQUEST, "无效的 multipart boundary")),
    };
    // multer 默认单字段 ~2MB，会截断大码表；显式放宽到 32MB/字段、40MB/整流
    let constraints = multer::Constraints::new().size_limit(
        multer::SizeLimit::new()
            .per_field(MAX_UPLOAD_BYTES as u64)
            .whole_stream((MAX_UPLOAD_BYTES + 8 * 1024 * 1024) as u64),
    );
    let stream = futures_util::stream::once(async move { Ok::<_, multer::Error>(body) });
    let mut multipart = multer::Multipart::with_constraints(stream, boundary, constraints);
    let mut f = UploadFields {
        name: String::new(),
        select_keys: "abcdefghijklmnopqrstuvwxyz".to_string(),
        max_len: 4,
        content: String::new(),
        token: None,
    };
    while let Some(field) = multipart.next_field().await.map_err(|_| {
        err_resp(StatusCode::PAYLOAD_TOO_LARGE, "上传文件过大（上限 32MB）")
    })? {
        let fname = field.name().unwrap_or("").to_string();
        match fname.as_str() {
            "name" => f.name = field.text().await.unwrap_or_default(),
            "select_keys" => {
                let v = field.text().await.unwrap_or_default();
                if !v.is_empty() {
                    f.select_keys = v;
                }
            }
            "max_len" => {
                if let Ok(n) = field.text().await.unwrap_or_default().parse::<i64>() {
                    f.max_len = n;
                }
            }
            "file" => {
                f.content = field.text().await.map_err(|_| {
                    err_resp(StatusCode::PAYLOAD_TOO_LARGE, "上传文件过大（上限 32MB）")
                })?;
            }
            "token" => f.token = Some(field.text().await.unwrap_or_default()),
            _ => {
                let _ = field.text().await;
            }
        }
    }
    Ok(f)
}

fn encode_png(img: image::RgbaImage) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut buf);
        let dyn_img = image::DynamicImage::ImageRgba8(img);
        dyn_img
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap_or_else(|e| panic!("png encode: {}", e));
    }
    buf
}

fn png_bytes_response(buf: Bytes) -> Response {
    // Bytes 实现 IntoResponse，引用计数直接作为 body，无需复制
    ([(header::CONTENT_TYPE, "image/png".to_string())], buf).into_response()
}

/// 查渲染缓存；未命中则在 spawn_blocking 里从引擎池独占借一个引擎做渲染 + PNG 编码
/// （CPU 密集，不占用 tokio 工作线程），完成后回填缓存。`job` 需 'static + Send，
/// 调用处把所需数据 clone 后 move 进去。
async fn render_or_cache<F>(
    st: &Arc<AppState>,
    key: String,
    job: F,
) -> Result<Bytes, Response>
where
    F: FnOnce(&TextEngine) -> image::RgbaImage + Send + 'static,
{
    if st.render_cache.enabled() {
        if let Some(png) = st.render_cache.get(&key) {
            return Ok(png); // Bytes clone 已零拷贝
        }
    }
    // 记录发起渲染时的代际：若渲染期间发生上传/清空，本次旧图不写回缓存
    let gen = st.render_cache.generation();
    let engines = st.engines.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let engine = engines.acquire();
        encode_png(job(&engine)) // guard 在此 drop，引擎归还池中
    })
    .await
    .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "渲染任务失败"))?;
    Ok(st.render_cache.insert_if_current(key, bytes, gen))
}

fn err_resp(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, msg.into()).into_response()
}

async fn index() -> Html<String> {
    let dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "static".to_string());
    let path = PathBuf::from(&dir).join("index.html");
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| "<h1>index.html not found</h1>".into());
    Html(body)
}

async fn list_schemas(State(st): State<Arc<AppState>>) -> Response {
    let schemas = st.schemas.read().await;
    let mut v: Vec<_> = schemas.values().map(|c| {
        let mut info = c.info.clone();
        info.count = c.entry_count as i64;
        info
    }).collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Json(v).into_response()
}

async fn upload_schema(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let f = match parse_upload(body, &headers).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    if f.name.trim().is_empty() {
        return err_resp(StatusCode::BAD_REQUEST, "缺少方案名 name");
    }
    if !auth_ok(&st, &headers, f.token.as_deref()) {
        return err_resp(StatusCode::UNAUTHORIZED, "未授权：缺少或错误的上传令牌");
    }
    // 串行化整段上传：DB 导入 → 缓存重建/写回 → 渲染缓存失效 必须一气呵成，
    // 否则两个并发上传同名方案会让内存缓存停留在某一版、与 DB 永久不一致。
    let _guard = st.upload_lock.lock().await;
    // std Mutex + spawn_blocking：导入是重 IO，别阻塞 tokio 工作线程
    let db = st.db.clone();
    let schemas = st.schemas.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut g = db.lock().unwrap();
        let res = g.import_schema(&f.name, &f.select_keys, f.max_len, &f.content);
        let cache = build_schema_cache(&*g, &f.name);
        (res, cache, f.name)
    });
    let (res, cache, name) = match handle.await {
        Ok(v) => v,
        Err(_) => return err_resp(StatusCode::INTERNAL_SERVER_ERROR, "导入任务失败"),
    };
    match res {
        Ok(cnt) => {
            if let Some(cache) = cache {
                schemas.write().await.insert(name.clone(), cache);
            }
            // 码表变了，旧的渲染图必须失效
            st.render_cache.clear();
            Json(serde_json::json!({ "count": cnt })).into_response()
        }
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn query(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(p): Query<QParams>,
) -> Response {
    let q = match p.q {
        Some(q) if !q.is_empty() => q,
        _ => return err_resp(StatusCode::BAD_REQUEST, "缺少查询文本 q"),
    };
    if let Some(r) = check_query_len(&st, &q) {
        return r;
    }
    let cache = {
        let schemas = st.schemas.read().await;
        match schemas.get(&name).cloned() {
            Some(c) => c,
            None => return err_resp(StatusCode::NOT_FOUND, "方案不存在"),
        }
    };
    let force = p.force_single.unwrap_or(false);
    let show_kb = p.show_keyboard.unwrap_or(false);
    let segs = query_segments(&*cache, &cache.info, &q, force);
    let stats = compute_stats(&segs);

    let fmt = p.fmt.as_deref().unwrap_or("json");
    if fmt == "image" {
        // key 含所有影响渲染结果的参数；用 JSON 元组编码避免用户输入里的分隔符碰撞
        let key = serde_json::to_string(&("q", &name, &q, force, show_kb))
            .unwrap_or_else(|_| format!("q:{}:{}", name, q));
        let single = segs.len() == 1 && segs[0].is_single && !segs[0].missing;
        let segs_c = segs.clone();
        let stats_c = stats.clone();
        let bytes = match render_or_cache(&st, key, move |eng| {
            if single && segs_c.len() == 1 {
                render::render_single_char(eng, &segs_c[0])
            } else {
                render::render_query(eng, &segs_c, &stats_c, show_kb)
            }
        })
        .await
        {
            Ok(b) => b,
            Err(r) => return r,
        };
        png_bytes_response(bytes)
    } else {
        Json(serde_json::json!({
            "schema": name,
            "query": q,
            "force_single": force,
            "segments": segs,
            "stats": stats,
        }))
        .into_response()
    }
}

async fn reverse(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(p): Query<QParams>,
) -> Response {
    let code = match p.code {
        Some(c) if !c.is_empty() => c,
        _ => return err_resp(StatusCode::BAD_REQUEST, "缺少编码 code"),
    };
    if let Some(r) = check_query_len(&st, &code) {
        return r;
    }
    let cache = {
        let schemas = st.schemas.read().await;
        match schemas.get(&name).cloned() {
            Some(c) => c,
            None => return err_resp(StatusCode::NOT_FOUND, "方案不存在"),
        }
    };
    let rows = cache.code_words(&code).to_vec();
    drop(cache);
    let fmt = p.fmt.as_deref().unwrap_or("json");
    if fmt == "image" {
        let key = serde_json::to_string(&("r", &name, &code))
            .unwrap_or_else(|_| format!("r:{}:{}", name, code));
        let code_c = code.clone();
        let rows_c = rows.clone();
        let bytes = match render_or_cache(&st, key, move |eng| {
            render::render_reverse(eng, &code_c, &rows_c)
        })
        .await
        {
            Ok(b) => b,
            Err(r) => return r,
        };
        png_bytes_response(bytes)
    } else {
        Json(serde_json::json!({ "schema": name, "code": code, "words": rows })).into_response()
    }
}

async fn all(State(st): State<Arc<AppState>>, Query(p): Query<QParams>) -> Response {
    let q = match p.q {
        Some(q) if !q.is_empty() => q,
        _ => return err_resp(StatusCode::BAD_REQUEST, "缺少查询文本 q"),
    };
    if let Some(r) = check_query_len(&st, &q) {
        return r;
    }
    let caches: Vec<Arc<SchemaCache>> = {
        let schemas = st.schemas.read().await;
        if let Some(names) = &p.schemas {
            names
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|n| schemas.get(n.trim()).cloned())
                .collect()
        } else {
            schemas.values().cloned().collect()
        }
    };
    let results: Vec<AllResult> = query_all(&caches, &q);
    let fmt = p.fmt.as_deref().unwrap_or("json");
    if fmt == "image" {
        // 参与对比的方案集合也影响成图，排序后并入 key，保证不同顺序命中同一缓存
        let mut names: Vec<&str> = caches.iter().map(|c| c.info.name.as_str()).collect();
        names.sort_unstable();
        let key = serde_json::to_string(&("a", &q, &names))
            .unwrap_or_else(|_| format!("a:{}:{}", q, names.join(",")));
        let results_c = results.clone();
        let q_c = q.clone();
        let bytes = match render_or_cache(&st, key, move |eng| {
            render::render_all(eng, &results_c, &q_c)
        })
        .await
        {
            Ok(b) => b,
            Err(r) => return r,
        };
        png_bytes_response(bytes)
    } else {
        Json(serde_json::json!({ "query": q, "results": results })).into_response()
    }
}

async fn upload_freq(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let f = match parse_upload(body, &headers).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    if f.name.trim().is_empty() {
        return err_resp(StatusCode::BAD_REQUEST, "缺少词频名 name");
    }
    if !auth_ok(&st, &headers, f.token.as_deref()) {
        return err_resp(StatusCode::UNAUTHORIZED, "未授权：缺少或错误的上传令牌");
    }
    // 与上传码表共用同一把锁，保证两类写操作与缓存失效互不穿插
    let _guard = st.upload_lock.lock().await;
    let db = st.db.clone();
    let freqs = st.freqs.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut g = db.lock().unwrap();
        let res = g.import_freq(&f.name, &f.content);
        let freq: HashMap<String, String> = g
            .load_freqs(&f.name)
            .unwrap_or_default()
            .into_iter()
            .collect();
        (res, freq, f.name)
    });
    let (res, freq, name) = match handle.await {
        Ok(v) => v,
        Err(_) => return err_resp(StatusCode::INTERNAL_SERVER_ERROR, "导入任务失败"),
    };
    match res {
        Ok(cnt) => {
            if !freq.is_empty() {
                freqs.write().await.insert(name.clone(), Arc::new(freq));
            }
            // 词频变了，相关渲染图失效（当前渲染不直接依赖词频，保持一致处理）
            st.render_cache.clear();
            Json(serde_json::json!({ "count": cnt })).into_response()
        }
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn query_freq(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(p): Query<QParams>,
) -> Response {
    let q = match p.q {
        Some(q) if !q.is_empty() => q,
        _ => return err_resp(StatusCode::BAD_REQUEST, "缺少查询词 q"),
    };
    if let Some(r) = check_query_len(&st, &q) {
        return r;
    }
    let words: Vec<String> = q.split_whitespace().map(|s| s.to_string()).collect();
    let freqs = st.freqs.read().await;
    let map = match freqs.get(&name) {
        Some(m) => m.clone(),
        None => return err_resp(StatusCode::NOT_FOUND, "词频表不存在"),
    };
    drop(freqs);
    let rows: Vec<(String, Option<String>)> = words
        .iter()
        .map(|w| (w.clone(), map.get(w).cloned()))
        .collect();
    Json(serde_json::json!({ "schema": name, "freq": rows })).into_response()
}

/// 渲染统计（公开）：缓存命中率、条目数、占用字节、引擎池上限等。
async fn cache_stats(State(st): State<Arc<AppState>>) -> Response {
    let mut v = st.render_cache.stats();
    v["engine_pool_size"] = serde_json::json!(st.engines.size());
    Json(v).into_response()
}

/// 手动清空渲染缓存（写操作，启用 AUTH_TOKEN 时需带令牌）。
async fn cache_clear(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !auth_ok(&st, &headers, None) {
        return err_resp(StatusCode::UNAUTHORIZED, "未授权：缺少或错误的令牌");
    }
    st.render_cache.clear();
    Json(serde_json::json!({ "cleared": true })).into_response()
}

/// 健康检查（公开）：供云负载均衡 / K8s 存活性探针使用。
async fn healthz(State(st): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "status": "ok",
        "schemas": st.schemas.read().await.len(),
        "freqs": st.freqs.read().await.len(),
        "engine_pool_size": st.engines.size(),
        "max_query_len": st.max_query_len,
    }))
    .into_response()
}

/// 请求指标中间件：统计每个请求的状态码分布与耗时。
async fn metrics_mw(
    State(st): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let start = std::time::Instant::now();
    st.metrics.begin();
    let resp = next.run(req).await;
    st.metrics.end(resp.status().as_u16(), start.elapsed());
    resp
}

/// Prometheus 文本格式指标（公开，供云监控抓取）。
async fn metrics_endpoint(State(st): State<Arc<AppState>>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        st.metrics.render(),
    )
        .into_response()
}

/// 优雅关闭：收到 SIGINT/SIGTERM 后停止接受新连接，等待在途请求完成再退出。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("收到关闭信号，等待在途请求完成…");
}

#[tokio::main]
async fn main() {
    let data_dir = std::env::var("IM_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    let db = Db::open(std::path::Path::new(&data_dir)).expect("无法打开数据库");
    // 每个引擎各持一份字体缓存，按需创建；默认取 CPU 核数但封顶 8，防止大字体占满内存
    let default_engines = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let engine_count: usize = std::env::var("RENDER_ENGINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_engines)
        .max(1);
    println!("渲染引擎池上限：{}（按需创建）", engine_count);

    // 启动时预热内存缓存：把所有已存在的码表与词频表载入内存
    let mut schemas_map: HashMap<String, Arc<SchemaCache>> = HashMap::new();
    for name in db.list_schema_names().unwrap_or_default() {
        if let Some(c) = build_schema_cache(&db, &name) {
            schemas_map.insert(name, c);
        }
    }
    let mut freqs_map: HashMap<String, Arc<HashMap<String, String>>> = HashMap::new();
    for name in db.list_freq_names().unwrap_or_default() {
        let freq: HashMap<String, String> = db
            .load_freqs(&name)
            .unwrap_or_default()
            .into_iter()
            .collect();
        if !freq.is_empty() {
            freqs_map.insert(name, Arc::new(freq));
        }
    }
    println!(
        "已预热内存缓存：码表方案 {} 个，词频表 {} 个",
        schemas_map.len(),
        freqs_map.len()
    );

    let render_cap: usize = std::env::var("RENDER_CACHE_CAPACITY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    println!("渲染缓存容量：{}（0=禁用）", render_cap);

    let max_query_len: usize = std::env::var("MAX_QUERY_LEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    println!("查询文本上限：{} 字", max_query_len);

    let state = Arc::new(AppState {
        db: Arc::new(StdMutex::new(db)),
        engines: Arc::new(EnginePool::new(engine_count)),
        schemas: Arc::new(RwLock::new(schemas_map)),
        freqs: Arc::new(RwLock::new(freqs_map)),
        render_cache: Arc::new(RenderCache::new(render_cap)),
        auth_token: std::env::var("AUTH_TOKEN").ok().filter(|s| !s.is_empty()),
        max_query_len,
        upload_lock: Arc::new(tokio::sync::Mutex::new(())),
        metrics: Arc::new(Metrics::new()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/schemas", get(list_schemas).post(upload_schema))
        .route("/api/query/:name", get(query))
        .route("/api/reverse/:name", get(reverse))
        .route("/api/all", get(all))
        .route("/api/freq", post(upload_freq))
        .route("/api/freq/:name", get(query_freq))
        .route("/api/cache/stats", get(cache_stats))
        .route("/api/cache/clear", post(cache_clear))
        .route("/api/metrics", get(metrics_endpoint))
        // layer 必须在所有 route 注册之后，否则不作用于后续路由（axum 陷阱）
        // 放行大码表上传：请求体上限 64MB（业务层另有 32MB 校验）
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        // 指标中间件放最外层，连被 body-limit 拒绝的请求也能计入
        .layer(middleware::from_fn_with_state(state.clone(), metrics_mw))
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    println!("im_schemas_server 监听于 http://{}", addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    println!("服务已优雅退出");
}
