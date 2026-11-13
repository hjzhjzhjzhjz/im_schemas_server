# im_schemas_server（Rust 版）

把 [astrbot_plugin_im_schemas](https://github.com/hertz-hwang/astrbot_plugin_im_schemas)
（AstrBot 的输入法码表查询插件）改写成**不依赖 AstrBot 的独立 Web/HTTP 服务**，
使用 **Rust（axum + rusqlite + cosmic-text）** 实现，可直接在云服务器以单二进制运行。

核心能力：查汉字/词语在某输入法方案下的编码、编码反查、多方案对比（码长/选重/当量/缺字）、
词频查询，并以中文图文（含键盘热力图）形式返回。

## 特性

- **纯 Rust 单二进制**：无 Python 运行时、无需额外依赖，部署简单、并发安全。
- **码表内存缓存**：启动时预热全部方案到内存（`HashMap`），查询链路（DP 切分/反查/词频）
  纯内存查找，不再逐子串回查 SQLite；上传后自动重建缓存。阻塞式导入放在 `spawn_blocking`，不卡 async 线程。
- **查询路径零分配 + 切分正确性**：`CodeLookup` 返回借用切片（不再每次深拷贝整个 Vec），
  DP 内层复用子串缓冲、消除 O(n·L) 次临时 `String` 分配；探测上界取自码表**真实最长词条数**，
  不再拿「编码最大长度」硬猜「词条字数」——超长词条也能被整体切出。实测长文本查询吞吐 **+15%**。
- **输入保护 + 健康检查**：`MAX_QUERY_LEN` 限制查询字符数（防超长文本撑爆 JSON / 超宽 PNG），
  `GET /healthz` 供云负载均衡 / K8s 探针使用。
- **PNG 渲染缓存**：渲染结果（排版 + 逐像素绘制 + PNG 编码）按查询参数做 LRU 缓存，
  命中直接返回字节（用 `bytes::Bytes` 引用计数，响应零拷贝）；渲染放进 `spawn_blocking`，不占用 async 线程。
  上传码表/词频自动失效缓存，另提供命中率查询与手动清空接口。
- **渲染引擎池**：多个独立 `TextEngine`（各持 `FontSystem/SwashCache`），并发渲染独占借引擎、
  不再争一把全局字体锁；按需创建、上限可配，RAII 归还（panic 也不泄漏）。
  实测 4 引擎相对单引擎吞吐约 **3x**（1100 → 3000 req/s，禁用缓存、24 并发压测）。
- **中文图文渲染**：基于 `cosmic-text`（原生支持 CJK / ttc / 字体回退），
  复刻原版的配色：词组绿、简码橙、选重红、缺字紫、自编码蓝、透传灰；含键盘按键热力图。
- **SQLite 持久化**：码表/词频按方案名隔离，数据落盘到 `IM_DATA_DIR`，进程重启不丢。
- **上传安全**：可选 `AUTH_TOKEN` 令牌鉴权（仅上传接口，查询公开）；
  单文件 32MB 上限 + 请求体 64MB 上限，超限返回 413；流式解析支持大码表（无 2MB 截断）。
- **并发可靠性 + 优雅关闭**：上传全流程（DB 导入 → 内存缓存写回 → 渲染缓存失效）串行化；
  渲染缓存引入**代际号**，上传清空后在途的旧渲染不会回填污染缓存（实测修复「上传后仍返回旧图」）；
  `SIGINT/SIGTERM` 优雅关闭，等待在途请求完成再退出（实测 4MB 上传中被 SIGTERM 仍完整落盘）。
- **可观测**：`GET /api/metrics` 输出 Prometheus 文本（请求数、状态码分布、累计耗时、在途数）；
  `/healthz` 存活性探针；`/api/cache/stats` 含命中率、代际与过期拒绝数。
- **无头环境友好**：自动探测系统中文字体，部署前安装 `fonts-noto-cjk` 即可。

## 目录

```
im_schemas_server_rust/
├── Cargo.toml
├── pair_equivalence.txt        # 按键对当量表（当量计算）
├── src/
│   ├── main.rs                 # HTTP 路由（axum）
│   ├── algo.rs                 # 选重/码长/按键/当量 纯函数
│   ├── schema.rs               # 类型 + TSV 解析
│   ├── db.rs                   # SQLite 数据层（导入 + 批量读取）
│   ├── cache.rs                # 码表内存缓存 + CodeLookup trait
│   ├── render_cache.rs         # PNG 渲染 LRU 缓存 + 代际防污染 + 命中统计
│   ├── metrics.rs              # 请求指标（Prometheus 文本格式）
│   ├── segment.rs              # DP 最少按键切分 + 统计
│   └── render.rs               # cosmic-text 中文图文渲染 + EnginePool 渲染引擎池
└── static/index.html           # 内置网页 UI
```

## 本地运行

```bash
# 1) 系统需有中文字体（无则安装）
sudo apt-get install fonts-noto-cjk

# 2) 编译运行
cargo run --release
# 或
cargo build --release && ./target/release/im_schemas_server

# 打开 http://localhost:8000
```

环境变量：

| 变量 | 默认 | 说明 |
|------|------|------|
| `PORT` | `8000` | 监听端口 |
| `IM_DATA_DIR` | `data` | SQLite 数据目录（码表/词频） |
| `STATIC_DIR` | `static` | 前端 `index.html` 所在目录 |
| `AUTH_TOKEN` | 空（不启用） | 设置后，上传接口需带 `Authorization: Bearer <token>` 头或表单 `token` 字段 |
| `RENDER_CACHE_CAPACITY` | `256` | PNG 渲染缓存条数上限；设为 `0` 禁用缓存 |
| `RENDER_ENGINES` | CPU 核数封顶 8 | 渲染引擎池上限（并发渲染并行度）；每个引擎各持一份字体，内存紧可调小 |
| `MAX_QUERY_LEN` | `512` | 查询文本最大字符数，超限返回 `400`（保护 JSON / 渲染内存） |

> 上传鉴权保护 `POST /api/schemas`、`POST /api/freq`、`POST /api/cache/clear`；所有查询接口保持公开。
> 单文件上限 32MB，请求体上限 64MB，超限返回 `413`；查询文本超过 `MAX_QUERY_LEN` 返回 `400`。
> 上传码表/词频会**自动清空渲染缓存**（递增代际），避免返回旧图；在途渲染因代际过期会被拒绝回填。
> 重启服务（systemd `systemctl restart` 或直接 `kill -TERM`）发 `SIGTERM`，服务会等在途请求完成后再退出。

## 云服务器部署（直接二进制 + systemd）

编译产物是单个可执行文件，无需额外运行时。把二进制与运行时资源放到同一目录：

```bash
cargo build --release
sudo apt-get install -y fonts-noto-cjk
sudo mkdir -p /opt/im_schemas
sudo cp target/release/im_schemas_server /opt/im_schemas/
sudo cp -r static pair_equivalence.txt /opt/im_schemas/
```

> `pair_equivalence.txt`、`static/`、`data/` 均按**当前工作目录**解析，
> 因此运行时必须先 `cd` 到该目录（systemd 里用 `WorkingDirectory` 指定）。

用 systemd 托管，新建 `/etc/systemd/system/im_schemas.service`：

```ini
[Unit]
Description=im_schemas_server
After=network.target

[Service]
Type=simple
WorkingDirectory=/opt/im_schemas
ExecStart=/opt/im_schemas/im_schemas_server
Environment=PORT=8000
Environment=IM_DATA_DIR=data
Environment=STATIC_DIR=static
Environment=AUTH_TOKEN=换成强随机串
Restart=on-failure
# 发 SIGTERM 后等待在途上传/渲染完成再退出
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now im_schemas
sudo systemctl status im_schemas
# 访问 http://<服务器IP>:8000
```

数据持久化在 `WorkingDirectory` 下的 `data/`，重启服务不丢。前面「环境变量」表里的
`RENDER_CACHE_CAPACITY` / `RENDER_ENGINES` / `MAX_QUERY_LEN` 同样可用 `Environment=` 覆盖。

## API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/healthz` | 健康检查：`status`、方案数、词频表数、引擎池大小、查询上限 |
| GET | `/api/metrics` | Prometheus 文本指标：请求总数、按状态码分布、累计耗时、在途数 |
| GET | `/api/schemas` | 列出所有方案（含条目数） |
| POST | `/api/schemas` | 上传码表：`name`, `file`(TSV), 可选 `select_keys`, `max_len` |
| GET | `/api/query/{name}?q=...&fmt=json\|image&force_single=...&show_keyboard=...` | 查编码 |
| GET | `/api/reverse/{name}?code=...&fmt=json\|image` | 编码反查字词 |
| GET | `/api/all?q=...&schemas=逗号分隔&fmt=json\|image` | 多方案对比 |
| POST | `/api/freq` | 上传词频表：`name`, `file`(TSV) |
| GET | `/api/freq/{name}?q=词1 词2 ...` | 查词频 |
| GET | `/api/cache/stats` | 渲染缓存统计：命中率、条目数、占用字节、淘汰数、代际、过期拒绝数 |
| POST | `/api/cache/clear` | 手动清空渲染缓存（启用 `AUTH_TOKEN` 时需带令牌） |

### 码表 TSV 格式

```
<编码><TAB><字词>
```

例：

```
k	中
l	国
lh	中国
kwwl	中国人民
```

### 词频 TSV 格式

```
<字词><TAB><词频>
```

## 与原 Python 版差异

- 渲染由 Pillow 改为 `cosmic-text`（Rust 原生 CJK 支持，无需自带 PUA 字体）。
- 去掉私聊/多用户体系，改为网页/接口上传，用「方案名」隔离。
- 其余核心算法（DP 切分、选重/码长计费、按键当量、all 排序）一一移植。
