# gpustat4cluster 开发文档

[中文](#gpustat4cluster-开发文档) | [English](#gpustat4cluster-developer-guide-english)

本文面向后续开发者，介绍 gpustat4cluster 的 crate 划分、运行时数据流、协议格式和关键接口设计。

## Workspace

```text
crates/common          公共配置、错误码、协议结构和 rkyv payload 编解码
crates/server          计算节点服务端，负责 NVML 采集、缓存、KCP/TCP 服务和组播 announce
crates/client-backend  登录节点/用户侧常驻后端，负责发现服务端、维护连接、缓存查询结果、提供 UDS API
crates/client-cli      用户命令行前端，负责参数解析、UDS 查询和 gpustat 风格渲染
```

## 运行时数据流

1. server 启动，读取 `/etc/gpustat4cluster/server.toml`。
2. server 初始化 NVML collector，启动后台 collector loop，监听 KCP 和 TCP。
3. server 在 `multicast_addr` 上发送 announce，announce 包含 hostname、KCP 端口、TCP 端口、协议版本。
4. client-backend 启动，读取 `/etc/gpustat4cluster/client.toml`。
5. client-backend 发送 multicast discovery query，也监听后续 server announce。
6. client-backend 根据 `[connecting].protocol` 选择 KCP 或 TCP，与发现的 server 建立连接或记录静态节点。
7. CLI 通过 UDS 发送 `QUERY` 给 client-backend。
8. client-backend 检查本地 cache：未命中或 TTL 过期时向对应 server 查询 GPU snapshot。
9. server 返回 rkyv snapshot payload；client-backend 解码并更新本地缓存。
10. CLI 将 JSON view 渲染为 gpustat 风格表格或 `--json` 输出。

## common crate

### 配置结构

文件：`crates/common/src/config.rs`

核心结构：

- `Config`: 根配置，包含 `connecting`、`log`、`services`、`runtime`。
- `ConnectingConfig`: 连接、发现、端口、心跳、重试配置。
- `ServicesConfig`: 缓存、collector、延迟显示、UDS 配置。
- `RuntimeConfig`: 运行时依赖配置，目前用于 `nvml_lib_path`。

接口：

```rust
pub struct Config {
    pub connecting: ConnectingConfig,
    pub log: LogConfig,
    pub services: ServicesConfig,
    pub runtime: RuntimeConfig,
}
```

设计约束：

- `protocol` 默认 `kcp`。
- `kcp_port` / `tcp_port` 默认 `0`，server 解释为从 `port_range` 自动选择。
- `max_connections` 默认 `64`，也兼容旧配置 key `connections`。
- client 记录 TTL 使用本地时间，避免不同节点 NTP 漂移影响刷新。

### 协议结构

文件：`crates/common/src/protocol.rs`

帧头：

```text
magic       4 bytes  "G4C1"
version     1 byte   PROTOCOL_VERSION
frame_type  1 byte   FrameType
request_id  8 bytes  big-endian u64
payload_len 4 bytes  big-endian u32
```

`FrameType`：

- `DiscoveryQuery = 1`
- `DiscoveryAnnounce = 2`
- `HandshakeRequest = 3`
- `HandshakeInfo = 4`
- `QueryRequest = 5`
- `QueryResponse = 6`
- `DataPayload = 7`
- `Heartbeat = 8`
- `Disconnect = 9`

核心接口：

```rust
pub fn encode_frame(header: FrameHeader, payload: &[u8]) -> Vec<u8>;
pub fn decode_frame(input: &[u8]) -> Result<(FrameHeader, &[u8]), FrameDecodeError>;
pub fn encode_snapshot_payload(snapshot: &ServerGpuSnapshot) -> Result<Vec<u8>, PayloadEncodeError>;
pub fn decode_snapshot_payload(payload: &[u8]) -> Result<ServerGpuSnapshot, PayloadDecodeError>;
```

Snapshot 模型：

- `ServerGpuSnapshot`: 一个 server/node 的完整 GPU 快照。
- `GpuInfo`: 单张 GPU 数据。
- `GpuMemory`: 显存 MiB 计数。
- `GpuUtilization`: GPU/memory 利用率百分比。
- `GpuProcessInfo`: GPU 上的进程，传输 `uid: u32` 而不是 username，前端本地用 `getent passwd UID` 渲染用户名。

## server crate

### 启动入口

文件：`crates/server/src/main.rs`

主流程：

```rust
fn main();
fn run() -> Result<(), StartupError>;
```

`run()` 负责：

- 读取 `GPUSTAT4CLUSTER_CONFIG` 或默认 `/etc/gpustat4cluster/server.toml`。
- 校验组播地址、出口 IP、端口范围。
- 自动选择 KCP/TCP 监听端口。
- 初始化 collector。
- 创建 `GpuCache`。
- 启动 collector loop、KCP listener、TCP listener、multicast listener/announce。

### Collector

文件：`crates/server/src/collector.rs`

核心 trait：

```rust
pub trait GpuCollector: Send + Sync {
    fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode>;
}
```

实现：

- `NvmlCollector`: 生产环境 NVML collector。
- `MockNvmlCollector`: `test` 或 `mock-nvml` feature 下用于本地模拟。
- `DegradedCollector`: 测试中用于模拟错误。

NVML 初始化：

```rust
impl NvmlCollector {
    pub fn new(hostname: impl Into<String>, nvml_lib_path: Option<&str>) -> Result<Self, ErrorCode>;
}
```

设计要点：

- 默认调用 `Nvml::init()`，也就是系统动态加载器寻找 `libnvidia-ml.so`。
- 如果配置 `[runtime].nvml_lib_path`，通过 `Nvml::builder().lib_path(...).init()` 加载指定库。
- 初始化失败会作为启动 fatal error 记录到日志并退出，不再静默 degraded。

### Cache

文件：`crates/server/src/cache.rs`

`GpuCache` 将 collector 结果编码为 rkyv payload 并缓存：

```rust
pub struct GpuCache;
impl GpuCache {
    pub fn new() -> Self;
    pub fn get_latest_or_refresh(&self, collector: &dyn GpuCollector, ttl_ms: u64) -> Result<CacheEntry, ErrorCode>;
    pub fn metrics(&self) -> CacheMetrics;
}
```

设计要点：

- `cache_ttl_ms` 控制 query 路径是否复用上一份 payload。
- 后台 collector loop 使用 `collector_interval_ms` 定期刷新最新 NVML 快照。
- 并发 stale 请求会 coalesce，避免多个请求同时打到 NVML。

### Transport

文件：`crates/server/src/transport.rs`

`TransportContext` 负责处理二进制协议帧：

```rust
pub struct TransportContext;
impl TransportContext {
    pub fn new(hostname: impl Into<String>, collector: Arc<dyn GpuCollector>, cache: Arc<GpuCache>, ttl_ms: u64) -> Self;
    pub fn handle_frame(&self, frame: &[u8]) -> Result<Vec<u8>, TransportError>;
    pub fn handle_decoded_frame(&self, decoded: DecodedFrame) -> Result<Vec<u8>, TransportError>;
}
```

支持请求：

- `HandshakeRequest`: 返回 `HandshakeInfo { hostname, gpu_num, payload_len }`。
- `QueryRequest`: 返回 `DataPayload` 或 `QueryResponse::error`。

### KCP

文件：`crates/server/src/kcp_transport.rs`

职责：

- 监听 UDP/KCP 端口。
- 接受 session。
- 每个 session 处理 handshake、query、heartbeat、disconnect。
- 温和退出时向对端发送 `Disconnect`。

关键配置：

- `heartbeat_interval`
- `connection_idle_timeout`
- `max_connections`
- `kcp_retry_limit`

### TCP

TCP 查询路径在 server main 中启动。协议简单：客户端连接 TCP 后发送 `QUERY\n`，server 返回 JSON：

```json
{
  "ok": true,
  "payload_b64": "..."
}
```

`payload_b64` 是与 KCP `DataPayload` 相同的 rkyv snapshot bytes，只是经 base64 包装。

### Multicast discovery

server announce 使用 JSON `DiscoveryAnnounce`：

```json
{
  "version": 1,
  "hostname": "node-a",
  "port": 30000,
  "kcp_port": 30000,
  "tcp_port": 30001,
  "ts_ms": 0
}
```

兼容字段：

- `port`: legacy 字段，旧 client 可继续使用。
- `kcp_port` / `tcp_port`: 新 client 根据协议选择端口。

`multicast_outbound_ip` 可配置多个本机 IPv4，server 会尝试在这些出口上 announce/join。

## client-backend crate

### 启动入口

文件：`crates/client-backend/src/main.rs`

主流程：

```rust
fn main();
fn run() -> Result<(), String>;
```

`run()` 负责：

- 读取 `GPUSTAT4CLUSTER_CONFIG` 或默认 `/etc/gpustat4cluster/client.toml`。
- 根据 `protocol` 判断 KCP/TCP。
- 组播 discovery + `GPUSTAT4CLUSTER_STATIC_NODES` 静态节点合并去重。
- 初始化 `LocalApiState`。
- KCP 模式下启动持久连接。
- 启动 announce listener，接收新 server 上线并建立连接。
- 启动 UDS local API。

### Discovery

文件：`crates/client-backend/src/discovery.rs`

核心接口：

```rust
pub fn discover_nodes(multicast_addr: &str, wait: Duration, outbound_ips: &[String], protocol: &str) -> Result<Vec<DiscoveredNode>, String>;
pub fn listen_for_announces(multicast_addr: &str, outbound_ips: &[String]) -> Result<UdpSocket, String>;
pub fn recv_announce_for_protocol(socket: &UdpSocket, protocol: &str) -> Result<Option<DiscoveredNode>, String>;
pub fn static_nodes_from_env() -> Result<Vec<DiscoveredNode>, String>;
pub fn merge_discovered_nodes(discovered: Vec<DiscoveredNode>, static_nodes: Vec<DiscoveredNode>) -> Vec<DiscoveredNode>;
```

设计要点：

- discovery query 使用 UDP 组播发送。
- 收到 announce 后用 UDP 来源 IP 作为 server IP，端口从 announce 的 `kcp_port` 或 `tcp_port` 选择。
- 静态节点来自 `GPUSTAT4CLUSTER_STATIC_NODES`，格式为逗号分隔的 `host:port`。

### Cache

文件：`crates/client-backend/src/cache.rs`

核心结构：

```rust
pub type CacheMap = HashMap<String, ConnectionCacheEntry>;
pub type SharedCache = Arc<Mutex<CacheMap>>;

pub struct ConnectionCacheEntry {
    pub connection_id: String,
    pub hostname: String,
    pub num: u8,
    pub record_timestamp: i64,
    pub addr: SocketAddr,
    pub last_snapshot: Option<ServerGpuSnapshot>,
    pub last_error: Option<String>,
    pub last_query_latency_us: Option<u64>,
}
```

设计要点：

- `record_timestamp` 使用 client-backend 本地时间。
- `last_query_latency_us` 用于 CLI hostname 旁的 `delay=...` 显示。
- `upsert_snapshot()` 在成功 query 后刷新 snapshot、timestamp 和 latency。
- `mark_stale()` 在查询失败时保留可诊断错误。

### Local API

文件：`crates/client-backend/src/local_api.rs`

CLI 和 backend 只通过 UDS 通信，默认：

```text
/run/gpustat4cluster/client.sock
```

核心状态：

```rust
pub struct LocalApiState;
impl LocalApiState {
    pub fn new(...) -> Self;
    pub fn add_discovered_nodes(&self, nodes: &[DiscoveredNode]);
    pub fn establish_kcp_connections(&self, nodes: &[DiscoveredNode]);
    pub fn shutdown(&self, reason: &str);
}
```

Local API 命令：

```text
QUERY {"filter":null,"user":null}\n
```

返回一行 JSON，结构来自 adapter 生成的 `QueryResponse`。

刷新策略：

- CLI 每次 QUERY 到 backend。
- backend 检查 cache entry 是否不存在或 `cache_ttl_ms` 过期。
- 只有过期或缺失时才向 server 发起 KCP/TCP query。
- KCP 连接是 backend 启动或收到 announce 时建立并保持；server GPU 数据不是后台周期 query，而是按 QUERY 触发。

### KCP client

文件：`crates/client-backend/src/kcp_client.rs`

核心接口：

```rust
pub async fn connect_node_with_timeout(addr: SocketAddr, connection_idle_timeout: Duration) -> Result<ConnectedKcpNode, KcpClientError>;
pub async fn heartbeat_connected(node: &ConnectedKcpNode) -> Result<(), KcpClientError>;
pub async fn query_connected(node: &ConnectedKcpNode) -> Result<ServerGpuSnapshot, KcpClientError>;
pub async fn disconnect_connected(node: &ConnectedKcpNode, reason: &str) -> Result<(), KcpClientError>;
pub fn close_connected(node: &ConnectedKcpNode);
```

设计要点：

- 一个 client-backend 和一个 server 之间只保留一个 KCP session。
- `ConnectedKcpNode` 内部用 mutex 串行化同一 session 上的 heartbeat/query/disconnect 写读，避免帧交错。
- `max_connections` 限制最多连接的 server 数。
- `kcp_retry_limit` 控制连接重试次数。

### TCP client

文件：`crates/client-backend/src/tcp_client.rs`

核心接口：

```rust
pub fn query_node(addr: SocketAddr, connection_idle_timeout: Duration) -> Result<ServerGpuSnapshot, TcpClientError>;
```

TCP 模式无持久连接池：每次 cache 过期 query 时连接 server TCP 端口，发送 `QUERY\n`，读取 JSON 响应并解码 `payload_b64`。

### Adapter

文件：`crates/client-backend/src/adapter.rs`

职责：

- 将 `CacheMap` 转为 CLI 需要的 JSON view。
- 将 `ServerGpuSnapshot` 转为 node/gpu/process view。
- 根据 `uid` 在 backend 侧或 CLI 侧保留可渲染字段。
- 保持空结果 schema 稳定，便于脚本消费。

## client-cli crate

### 参数解析

文件：`crates/client-cli/src/args.rs`

核心接口：

```rust
pub fn parse_args(args: Vec<String>) -> Result<CliOptions, String>;
pub fn help_text() -> &'static str;
```

关键行为：

- `-i` / `--interval` 无参数时进入 watch，默认 2s。
- `-i 0.05` 最小允许 50ms，低于 50ms 会 clamp 到 50ms。
- `--json` 与 watch 模式互斥。

### Backend UDS client

文件：`crates/client-cli/src/backend.rs`

核心接口：

```rust
pub fn connect_backend(opts: &CliOptions) -> Result<BackendConnection, String>;
pub fn query_backend(opts: &CliOptions) -> Result<QueryResponse, String>;
pub fn backend_socket_from_options(opts: &CliOptions) -> String;
pub fn latency_display_from_options(opts: &CliOptions) -> bool;
```

设计要点：

- Unix 平台通过 `UnixStream` 连接 backend。
- backend socket 来源优先级：CLI 参数 `--backend-socket` > `GPUSTAT4CLUSTER_BACKEND_SOCKET` > config `[services].uds_path` > 默认 `/run/gpustat4cluster/client.sock`。
- 非 Unix 平台目前只能编译 CLI，但实时 UDS 查询会返回 unsupported 错误。

### 渲染

文件：`crates/client-cli/src/render.rs`

核心接口：

```rust
pub fn render_table(resp: &QueryResponse, user_filter: Option<&str>, opts: &RenderOptions) -> String;
pub fn render_json(resp: &QueryResponse) -> Result<String, String>;
```

设计目标：

- 尽量贴近 `wookayin/gpustat` 的布局和颜色习惯。
- hostname 行显示本地时间、driver version、可选 delay、stale/error。
- GPU 行显示 index、GPU name、temperature、util、memory、process summary。
- `uid` 到 username 的转换通过 `getent passwd UID`，并做本地 cache。

## 性能优化设计

主要优化点：

- server 将 NVML 采样和 query 响应解耦：`collector_interval_ms` 控制后台采样频率，query 路径优先读取最新缓存，避免每次用户刷新都同步调用 NVML。
- server cache 保存已经编码好的 rkyv payload，减少 query 时重复序列化成本。
- client-backend 使用本地 `record_timestamp` 判断 TTL，避免跨节点 NTP 漂移导致缓存被错误判定为新鲜或过期。
- client-backend 只在 CLI `QUERY` 且记录缺失/过期时向 server 查询；不会盲目周期性 query 所有 server。
- KCP 模式在 backend 启动或收到 announce 后建立持久 session，query 时复用已有连接，降低握手和 socket 创建成本。
- 同一个 client-backend 和同一个 server 之间只保留一个 KCP session，`max_connections` 限制最多连接的 server 数，避免连接风暴。
- TCP 模式作为 UDP 不可用时的兼容 fallback，每次过期 query 临时连接，牺牲一点延迟换取部署兼容性。
- CLI 与 backend 使用 UDS，避免本机 local TCP 的额外协议栈开销和端口管理成本。
- GPU process 传输 `uid: u32` 而不是 username，CLI 本地通过 `getent passwd UID` 解析并缓存用户名，减少 payload 大小和 server 端字符串处理。
- Release multiarch deb/rpm 使用 selector 在安装时选择本机架构二进制，避免用户手动挑包。

## Packaging and CI

脚本：

- `scripts/package-deb.sh`: 生成 Ubuntu/Debian server/client deb 包，可 multiarch。
- `scripts/package-rpm.sh`: 生成 RHEL/CentOS server/client rpm 包，可 multiarch。
- `scripts/package-archlinux-client.sh`: 生成 Arch Linux client 包。
- `scripts/smoke-package-artifact.sh`: 验证发布包布局。

Workflow：

- `.github/workflows/build.yml`: 唯一发布 workflow，只支持手动 `workflow_dispatch`。
- dispatch 输入 `commit_target` 指定要发布的 commit、branch 或 tag。
- dispatch 输入 `release_type` 选择 `pre-release` 或 `release`。
- workflow 先 checkout target 并做格式、语法、workspace check、`release_note.md` 存在性和 tag/release 冲突检查。
- 构建全部平台产物后再运行测试；测试通过后从 Cargo.toml 读取版本，创建 `v<version>` tag 和 GitHub Release。
- Release body 来自 `release_note.md`。

Release attachments：

- `gpustat4cluster-server-multiarch.deb`
- `gpustat4cluster-client-multiarch.deb`
- `gpustat4cluster-server-multiarch.rpm`
- `gpustat4cluster-client-multiarch.rpm`
- `gpustat4cluster-client-archlinux-multiarch.pkg.tar.zst`
- `gpustat4cluster-client-macos-multiarch.tar.gz`
- `gpustat4cluster-client-windows-multiarch.zip`
- `gpustat4cluster-server&client-anylinux-x86_64.zip`
- `gpustat4cluster-server&client-anylinux-aarch64.zip`

## 测试建议

常规：

```bash
source /opt/shell_related/z00_lmod.sh && module load compiler/rust
cargo fmt --check
cargo test --workspace
cargo test -p server --features 'kcp-transport nvml'
cargo test -p gpustat4cluster-client-backend --features kcp-transport
```

本地 mock：

```bash
GPUSTAT4CLUSTER_COLLECTOR=mock \
GPUSTAT4CLUSTER_MOCK_HOSTNAME=node-a \
GPUSTAT4CLUSTER_MOCK_GPU_COUNT=8 \
cargo run -p server --features 'mock-nvml kcp-transport'
```

打包：

```bash
GPUSTAT4CLUSTER_DEB_MULTIARCH=1 scripts/package-deb.sh
GPUSTAT4CLUSTER_RPM_MULTIARCH=1 scripts/package-rpm.sh
scripts/package-archlinux-client.sh
```

---

# gpustat4cluster Developer Guide (English)

This guide describes the workspace layout, runtime architecture, protocol format, and important interfaces for future development.

## Workspace

```text
crates/common          Shared config, error codes, protocol structures, and rkyv payload encoding
crates/server          Compute-node daemon: NVML collection, cache, KCP/TCP services, multicast announce
crates/client-backend  Login/user-node daemon: server discovery, connection management, result cache, UDS API
crates/client-cli      User-facing CLI: argument parsing, UDS queries, gpustat-style rendering
```

## Runtime data flow

1. The server starts and reads `/etc/gpustat4cluster/server.toml`.
2. The server initializes the NVML collector, starts the background collector loop, and listens on KCP and TCP.
3. The server sends multicast announce messages containing hostname, KCP port, TCP port, and protocol version.
4. The client backend starts and reads `/etc/gpustat4cluster/client.toml`.
5. The client backend sends multicast discovery queries and also listens for later server announces.
6. The client backend chooses KCP or TCP from `[connecting].protocol`, then connects to discovered servers or records static nodes.
7. The CLI sends `QUERY` requests to the client backend over UDS.
8. The client backend checks its local cache and only queries the server when a record is missing or expired.
9. The server returns an rkyv snapshot payload; the client backend decodes it and updates the local cache.
10. The CLI renders the JSON view as a `gpustat`-style table or prints JSON with `--json`.

## common crate

### Config model

File: `crates/common/src/config.rs`

Important types:

- `Config`: root config containing `connecting`, `log`, `services`, and `runtime`.
- `ConnectingConfig`: ports, multicast, heartbeat, retry, and connection limits.
- `ServicesConfig`: cache TTL, collector polling interval, latency display, and UDS path.
- `RuntimeConfig`: runtime dependency settings, currently `nvml_lib_path`.

Interface:

```rust
pub struct Config {
    pub connecting: ConnectingConfig,
    pub log: LogConfig,
    pub services: ServicesConfig,
    pub runtime: RuntimeConfig,
}
```

Design notes:

- `protocol` defaults to `kcp`.
- `kcp_port` and `tcp_port` default to `0`; the server interprets `0` as auto-pick from `port_range`.
- `max_connections` defaults to `64` and still accepts the legacy key `connections`.
- Client cache freshness is measured with the client-backend local clock to avoid refresh delays from cluster clock drift.

### Protocol model

File: `crates/common/src/protocol.rs`

Frame header:

```text
magic       4 bytes  "G4C1"
version     1 byte   PROTOCOL_VERSION
frame_type  1 byte   FrameType
request_id  8 bytes  big-endian u64
payload_len 4 bytes  big-endian u32
```

`FrameType`:

- `DiscoveryQuery = 1`
- `DiscoveryAnnounce = 2`
- `HandshakeRequest = 3`
- `HandshakeInfo = 4`
- `QueryRequest = 5`
- `QueryResponse = 6`
- `DataPayload = 7`
- `Heartbeat = 8`
- `Disconnect = 9`

Core functions:

```rust
pub fn encode_frame(header: FrameHeader, payload: &[u8]) -> Vec<u8>;
pub fn decode_frame(input: &[u8]) -> Result<(FrameHeader, &[u8]), FrameDecodeError>;
pub fn encode_snapshot_payload(snapshot: &ServerGpuSnapshot) -> Result<Vec<u8>, PayloadEncodeError>;
pub fn decode_snapshot_payload(payload: &[u8]) -> Result<ServerGpuSnapshot, PayloadDecodeError>;
```

Snapshot model:

- `ServerGpuSnapshot`: complete GPU snapshot for one server/node.
- `GpuInfo`: one GPU row.
- `GpuMemory`: framebuffer memory counters in MiB.
- `GpuUtilization`: GPU and memory utilization percentages.
- `GpuProcessInfo`: process attribution. It transmits `uid: u32` instead of username; the frontend resolves usernames locally through `getent passwd UID`.

## server crate

### Startup

File: `crates/server/src/main.rs`

Entry points:

```rust
fn main();
fn run() -> Result<(), StartupError>;
```

`run()` is responsible for:

- Reading `GPUSTAT4CLUSTER_CONFIG` or `/etc/gpustat4cluster/server.toml`.
- Validating multicast address, outbound IPs, and port range.
- Picking KCP/TCP listen ports.
- Initializing the collector.
- Creating `GpuCache`.
- Starting collector, KCP listener, TCP listener, multicast listener, and startup announce loops.

### Collector

File: `crates/server/src/collector.rs`

Core trait:

```rust
pub trait GpuCollector: Send + Sync {
    fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode>;
}
```

Implementations:

- `NvmlCollector`: production NVML collector.
- `MockNvmlCollector`: mock data under tests or the `mock-nvml` feature.
- `DegradedCollector`: test-only failure collector.

NVML constructor:

```rust
impl NvmlCollector {
    pub fn new(hostname: impl Into<String>, nvml_lib_path: Option<&str>) -> Result<Self, ErrorCode>;
}
```

Design notes:

- Without config, it calls `Nvml::init()` and lets the dynamic loader find `libnvidia-ml.so`.
- With `[runtime].nvml_lib_path`, it calls `Nvml::builder().lib_path(...).init()`.
- Startup fails fast when NVML cannot be initialized; the server logs a fatal startup error instead of continuing silently in degraded mode.

### Cache

File: `crates/server/src/cache.rs`

`GpuCache` collects snapshots and caches encoded rkyv payloads:

```rust
pub struct GpuCache;
impl GpuCache {
    pub fn new() -> Self;
    pub fn get_latest_or_refresh(&self, collector: &dyn GpuCollector, ttl_ms: u64) -> Result<CacheEntry, ErrorCode>;
    pub fn metrics(&self) -> CacheMetrics;
}
```

Design notes:

- `cache_ttl_ms` controls whether a query reuses the cached payload.
- `collector_interval_ms` controls the background NVML polling loop.
- Concurrent stale requests are coalesced so that multiple requests do not hit NVML at the same time.

### Transport

File: `crates/server/src/transport.rs`

`TransportContext` handles binary protocol frames:

```rust
pub struct TransportContext;
impl TransportContext {
    pub fn new(hostname: impl Into<String>, collector: Arc<dyn GpuCollector>, cache: Arc<GpuCache>, ttl_ms: u64) -> Self;
    pub fn handle_frame(&self, frame: &[u8]) -> Result<Vec<u8>, TransportError>;
    pub fn handle_decoded_frame(&self, decoded: DecodedFrame) -> Result<Vec<u8>, TransportError>;
}
```

Supported requests:

- `HandshakeRequest`: returns `HandshakeInfo { hostname, gpu_num, payload_len }`.
- `QueryRequest`: returns `DataPayload` or `QueryResponse::error`.

### KCP

File: `crates/server/src/kcp_transport.rs`

Responsibilities:

- Listen on the UDP/KCP port.
- Accept sessions.
- Handle handshake, query, heartbeat, and disconnect frames per session.
- Send a `Disconnect` frame during graceful shutdown.

Important config keys:

- `heartbeat_interval`
- `connection_idle_timeout`
- `max_connections`
- `kcp_retry_limit`

### TCP

The TCP query path is started from server main. The wire protocol is intentionally simple: the client connects, sends `QUERY\n`, and the server returns JSON:

```json
{
  "ok": true,
  "payload_b64": "..."
}
```

`payload_b64` contains the same rkyv snapshot bytes used by KCP `DataPayload`, wrapped in base64 for TCP JSON transport.

### Multicast discovery

Server announce JSON uses `DiscoveryAnnounce`:

```json
{
  "version": 1,
  "hostname": "node-a",
  "port": 30000,
  "kcp_port": 30000,
  "tcp_port": 30001,
  "ts_ms": 0
}
```

Compatibility fields:

- `port`: legacy field for older clients.
- `kcp_port` / `tcp_port`: protocol-specific ports for newer clients.

`multicast_outbound_ip` can contain multiple local IPv4 addresses. The server attempts announce/join on each configured egress address.

## client-backend crate

### Startup

File: `crates/client-backend/src/main.rs`

Entry points:

```rust
fn main();
fn run() -> Result<(), String>;
```

`run()` is responsible for:

- Reading `GPUSTAT4CLUSTER_CONFIG` or `/etc/gpustat4cluster/client.toml`.
- Selecting KCP or TCP from `protocol`.
- Merging multicast discovery results with `GPUSTAT4CLUSTER_STATIC_NODES`.
- Initializing `LocalApiState`.
- Establishing persistent KCP sessions when KCP mode is enabled.
- Starting an announce listener to connect newly announced servers.
- Serving the local UDS API.

### Discovery

File: `crates/client-backend/src/discovery.rs`

Core functions:

```rust
pub fn discover_nodes(multicast_addr: &str, wait: Duration, outbound_ips: &[String], protocol: &str) -> Result<Vec<DiscoveredNode>, String>;
pub fn listen_for_announces(multicast_addr: &str, outbound_ips: &[String]) -> Result<UdpSocket, String>;
pub fn recv_announce_for_protocol(socket: &UdpSocket, protocol: &str) -> Result<Option<DiscoveredNode>, String>;
pub fn static_nodes_from_env() -> Result<Vec<DiscoveredNode>, String>;
pub fn merge_discovered_nodes(discovered: Vec<DiscoveredNode>, static_nodes: Vec<DiscoveredNode>) -> Vec<DiscoveredNode>;
```

Design notes:

- Discovery queries are sent over UDP multicast.
- The UDP source IP is used as the server IP; the selected port comes from `kcp_port` or `tcp_port` based on client protocol.
- Static nodes come from `GPUSTAT4CLUSTER_STATIC_NODES`, a comma-separated list of `host:port` entries.

### Cache

File: `crates/client-backend/src/cache.rs`

Core structures:

```rust
pub type CacheMap = HashMap<String, ConnectionCacheEntry>;
pub type SharedCache = Arc<Mutex<CacheMap>>;

pub struct ConnectionCacheEntry {
    pub connection_id: String,
    pub hostname: String,
    pub num: u8,
    pub record_timestamp: i64,
    pub addr: SocketAddr,
    pub last_snapshot: Option<ServerGpuSnapshot>,
    pub last_error: Option<String>,
    pub last_query_latency_us: Option<u64>,
}
```

Design notes:

- `record_timestamp` uses the client-backend local clock.
- `last_query_latency_us` is rendered by the CLI next to the node hostname.
- `upsert_snapshot()` refreshes snapshot, timestamp, and latency after successful queries.
- `mark_stale()` keeps a diagnosable error when a query fails.

### Local API

File: `crates/client-backend/src/local_api.rs`

The CLI and backend communicate only through UDS. The default path is:

```text
/run/gpustat4cluster/client.sock
```

Core state:

```rust
pub struct LocalApiState;
impl LocalApiState {
    pub fn new(...) -> Self;
    pub fn add_discovered_nodes(&self, nodes: &[DiscoveredNode]);
    pub fn establish_kcp_connections(&self, nodes: &[DiscoveredNode]);
    pub fn shutdown(&self, reason: &str);
}
```

Local API command:

```text
QUERY {"filter":null,"user":null}\n
```

The backend returns one JSON line produced by the adapter layer.

Refresh policy:

- The CLI sends a QUERY to the backend each time it renders.
- The backend checks whether a cache entry is missing or older than `cache_ttl_ms`.
- Only missing or expired entries trigger a server query.
- KCP sessions are established at backend startup or when a server announce is received; server GPU data is still fetched on demand by frontend QUERY, not by periodic backend polling.

### KCP client

File: `crates/client-backend/src/kcp_client.rs`

Core functions:

```rust
pub async fn connect_node_with_timeout(addr: SocketAddr, connection_idle_timeout: Duration) -> Result<ConnectedKcpNode, KcpClientError>;
pub async fn heartbeat_connected(node: &ConnectedKcpNode) -> Result<(), KcpClientError>;
pub async fn query_connected(node: &ConnectedKcpNode) -> Result<ServerGpuSnapshot, KcpClientError>;
pub async fn disconnect_connected(node: &ConnectedKcpNode, reason: &str) -> Result<(), KcpClientError>;
pub fn close_connected(node: &ConnectedKcpNode);
```

Design notes:

- A client-backend keeps at most one KCP session per server.
- `ConnectedKcpNode` serializes heartbeat/query/disconnect I/O with a mutex to avoid frame interleaving on the same session.
- `max_connections` limits the number of connected servers.
- `kcp_retry_limit` limits connection retry attempts.

### TCP client

File: `crates/client-backend/src/tcp_client.rs`

Core function:

```rust
pub fn query_node(addr: SocketAddr, connection_idle_timeout: Duration) -> Result<ServerGpuSnapshot, TcpClientError>;
```

TCP mode does not keep a persistent connection pool. When a cache entry expires, the backend opens a TCP connection, sends `QUERY\n`, reads the JSON response, and decodes `payload_b64`.

### Adapter

File: `crates/client-backend/src/adapter.rs`

Responsibilities:

- Convert `CacheMap` into the JSON view consumed by the CLI.
- Convert `ServerGpuSnapshot` into node/gpu/process views.
- Keep process UID fields available for username rendering.
- Keep empty response schemas stable for scripts.

## client-cli crate

### Argument parsing

File: `crates/client-cli/src/args.rs`

Core functions:

```rust
pub fn parse_args(args: Vec<String>) -> Result<CliOptions, String>;
pub fn help_text() -> &'static str;
```

Important behavior:

- `-i` / `--interval` without a value enables watch mode with the default 2s interval.
- `-i 0.05` is supported; lower values are clamped to 50ms.
- `--json` is mutually exclusive with watch mode.

### Backend UDS client

File: `crates/client-cli/src/backend.rs`

Core functions:

```rust
pub fn connect_backend(opts: &CliOptions) -> Result<BackendConnection, String>;
pub fn query_backend(opts: &CliOptions) -> Result<QueryResponse, String>;
pub fn backend_socket_from_options(opts: &CliOptions) -> String;
pub fn latency_display_from_options(opts: &CliOptions) -> bool;
```

Design notes:

- Unix platforms use `UnixStream` to connect to client-backend.
- Backend socket priority: CLI `--backend-socket` > `GPUSTAT4CLUSTER_BACKEND_SOCKET` > config `[services].uds_path` > `/run/gpustat4cluster/client.sock`.
- Non-Unix platforms can build the CLI, but live UDS querying returns an unsupported error.

### Rendering

File: `crates/client-cli/src/render.rs`

Core functions:

```rust
pub fn render_table(resp: &QueryResponse, user_filter: Option<&str>, opts: &RenderOptions) -> String;
pub fn render_json(resp: &QueryResponse) -> Result<String, String>;
```

Rendering goals:

- Match the layout and color style of `wookayin/gpustat` where practical.
- Show local time, driver version, optional query latency, stale state, and errors on node header lines.
- Show GPU index, GPU name, temperature, utilization, memory, and process summaries on GPU rows.
- Resolve UID to username with `getent passwd UID`, with a local cache.

## Performance design

Main optimization points:

- The server decouples NVML polling from query handling. `collector_interval_ms` controls the background polling loop, and query handling prefers the latest cached snapshot instead of calling NVML synchronously for every CLI refresh.
- The server cache stores already encoded rkyv payloads, reducing repeated serialization on hot query paths.
- The client backend measures freshness with local `record_timestamp` values so cluster clock drift does not delay refreshes or make records expire too early.
- The client backend only queries a server when the CLI sends `QUERY` and the local record is missing or expired; it does not blindly poll every server in the background.
- KCP mode establishes persistent sessions when the backend starts or receives a server announce, then reuses those sessions for queries.
- Each client-backend keeps at most one KCP session per server. `max_connections` caps the number of connected servers and prevents connection storms.
- TCP mode is the compatibility fallback when UDP is unavailable. It uses one short-lived TCP connection per expired query.
- The CLI talks to the backend over UDS, avoiding local TCP overhead and local port management.
- GPU process records transmit `uid: u32` instead of username. The CLI resolves and caches usernames locally through `getent passwd UID`, which keeps payloads smaller and avoids server-side string work.
- Release multiarch deb/rpm packages include an install-time selector so users do not need to choose architecture-specific packages manually.

## Packaging and CI

Scripts:

- `scripts/package-deb.sh`: Ubuntu/Debian server/client deb packages, optionally multiarch.
- `scripts/package-rpm.sh`: RHEL/CentOS server/client rpm packages, optionally multiarch.
- `scripts/package-archlinux-client.sh`: Arch Linux client package.
- `scripts/smoke-package-artifact.sh`: validates release package layout.

Workflow:

- `.github/workflows/build.yml`: the only release workflow; it supports manual `workflow_dispatch` only.
- The `commit_target` input selects the commit, branch, or tag to release.
- The `release_type` input selects `pre-release` or `release`.
- The workflow checks out the target, validates formatting, shell syntax, workflow YAML, workspace buildability, `release_note.md`, and tag/release conflicts.
- It builds every platform target first, then runs tests. If tests pass, it reads the Cargo version, creates a `v<version>` tag, creates a GitHub Release, and uploads attachments.
- The release body is read from `release_note.md`.

Release attachments:

- `gpustat4cluster-server-multiarch.deb`
- `gpustat4cluster-client-multiarch.deb`
- `gpustat4cluster-server-multiarch.rpm`
- `gpustat4cluster-client-multiarch.rpm`
- `gpustat4cluster-client-archlinux-multiarch.pkg.tar.zst`
- `gpustat4cluster-client-macos-multiarch.tar.gz`
- `gpustat4cluster-client-windows-multiarch.zip`
- `gpustat4cluster-server&client-anylinux-x86_64.zip`
- `gpustat4cluster-server&client-anylinux-aarch64.zip`

## Testing

Common checks:

```bash
source /opt/shell_related/z00_lmod.sh && module load compiler/rust
cargo fmt --check
cargo test --workspace
cargo test -p server --features 'kcp-transport nvml'
cargo test -p gpustat4cluster-client-backend --features kcp-transport
```

Local mock server:

```bash
GPUSTAT4CLUSTER_COLLECTOR=mock \
GPUSTAT4CLUSTER_MOCK_HOSTNAME=node-a \
GPUSTAT4CLUSTER_MOCK_GPU_COUNT=8 \
cargo run -p server --features 'mock-nvml kcp-transport'
```

Packaging:

```bash
GPUSTAT4CLUSTER_DEB_MULTIARCH=1 scripts/package-deb.sh
GPUSTAT4CLUSTER_RPM_MULTIARCH=1 scripts/package-rpm.sh
scripts/package-archlinux-client.sh
```
