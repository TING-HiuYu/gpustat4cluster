# gpustat4cluster 开发文档

[中文](#gpustat4cluster-开发文档) | [English](#gpustat4cluster-developer-guide)

本文面向后续维护者，说明当前 crate 划分、collector 抽象、网络协议、缓存路径和性能设计。

## Workspace

```text
crates/common          公共配置、错误码、协议结构、rkyv payload、UDP chunk 编解码
crates/server          计算节点服务端，负责 GRES/NVML 采集、缓存、UDP/TCP 服务和组播 announce
crates/client-backend  登录节点/用户侧常驻后端，负责发现服务端、维护连接、缓存查询结果、提供 UDS API
crates/client-cli      用户命令行前端，负责参数解析、UDS 查询和 gpustat 风格渲染
```

## 运行时数据流

1. server 读取 `/etc/gpustat4cluster/server.toml`。
2. server 初始化 `GresCollector` 实现，目前生产实现是 `NvmlCollector`。
3. server 后台按 `services.collector_interval_ms` 刷新 collector cache。
4. server 同时监听 UDP 和 TCP，端口可由 `port_range` 自动选择。
5. server 在 `multicast_addr` 上发送 announce，announce 包含 hostname、UDP 端口、TCP 端口、协议版本。
6. client-backend 读取 `/etc/gpustat4cluster/client.toml`，通过组播发现服务端，也监听后续 announce。
7. client-backend 根据 `[connecting].protocol` 选择 UDP 或 TCP，并为每个 server 维护一条持久连接抽象。
8. CLI 通过 UDS 发送 `QUERY` 给 client-backend。
9. client-backend 检查本地 cache：未命中或 TTL 过期时才向对应 server 查询 runtime payload。
10. client-backend 将网络 payload 投影成前端 JSON view，CLI 渲染为 gpustat 风格表格。

## Collector 抽象

文件：`crates/server/src/collector.rs`

核心 trait：

```rust
pub trait GresCollector: Send + Sync {
    fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode>;

    fn collect_gres_snapshot(&self) -> Result<ServerGresSnapshot, ErrorCode> {
        self.collect_gres().map(GresNodeSnapshot::into_gres_snapshot)
    }
}
```

关键类型：

- `GresNodeSnapshot`: 一个节点的规范化 GRES 资源清单。
- `GresResource`: 单个 GRES 资源，目前 `GresResourceKind::Nvml` 是唯一生产实现。
- `GresProcess`: 资源关联进程，传输 `uid: u32`、`pid: u32`、显存占用。
- `NvmlCollector`: 生产 collector，通过 NVML 读取 GPU 数据。
- `TestGresCollector`: 仅在 `#[cfg(test)]` 下编译，用于单元测试。它不再作为 feature 或环境变量暴露到运行期。

### 为什么网络输出是 `ServerGresSnapshot`

内部 collector 边界是 `GresNodeSnapshot`，网络稳定结构是 `ServerGresSnapshot`。当前生产 GRES 实现是 NVML，因此 CLI 继续渲染为 gpustat 风格表格，但协议字段和缓存字段已经统一使用 `gres`。

这个设计有两个目的：

- 不破坏现有前端协议和 gpustat 风格渲染。
- 把可扩展点放在 collector 层，后续新增其他 GRES 实现时，从 `GresResourceKind` 增加实现类型并补对应 collector 即可。

后续如果要支持其他 GRES 实现（例如 AMD、Intel 或其他加速设备 collector），推荐路径是：

1. 在 `GresResourceKind` 中新增实现类型。
2. 新增对应 collector 实现。
3. 如果新实现需要额外字段，再扩展 `GresResource`、`HostMetadata`、`RuntimeSnapshot` 和 CLI view。
4. 保留当前 gpustat 兼容渲染路径，作为 NVML 实现的默认视图。

### GRES collector contract

新增 collector 时，需要在对应测试中复用 `assert_gres_collector_contract(&collector)`。这个 contract 会检查：

- hostname 非空，driver version 为空时必须用 `None` 而不是空字符串。
- resource index 必须从 0 开始连续递增。
- resource name 非空。
- total memory 大于 0，used memory 不超过 total memory。
- GRES utilization 和 memory utilization 都在 `0..=100`。
- UUID 为空时使用 `None`，非空 UUID 不能重复。
- process pid 不能为 0，process used memory 不能超过该 resource total memory。
- collector 输出可以拆成 `HostMetadata` + `RuntimeSnapshot`，再无损重建为 `ServerGresSnapshot`。
- collector 输出可以通过 rkyv payload 编码和解码 roundtrip。

## 网络协议

文件：`crates/common/src/protocol.rs`

TCP 和 UDP 使用同一套 frame 和 payload 类型。区别只在传输层：TCP 直接传完整 frame；UDP 在 datagram 层进行 chunk 编码、校验和重组。

Frame header：

```text
magic       4 bytes  "G4C1"
version     1 byte   PROTOCOL_VERSION
frame_type  1 byte   FrameType
request_id  8 bytes  big-endian u64
payload_len 4 bytes  big-endian u32
```

运行期使用的主要 frame：

- `HandshakeRequest`: client 请求 metadata。
- `MetadataPayload`: server 返回 `HostMetadata`，包含 hostname、driver_version、GRES 静态信息。
- `QueryRequest`: client 请求实时数据。
- `RuntimePayload`: server 返回 `RuntimeSnapshot`，只包含实时变化字段。
- `QueryResponse`: 错误响应。
- `Disconnect`: 温和断开通知。

旧 `DataPayload` 和 `HandshakeInfo` 已从 common 协议层移除；运行期 TCP/UDP 统一使用 `MetadataPayload` 和 `RuntimePayload`。

## UDP transport

文件：`crates/common/src/udp.rs`、`crates/server/src/udp_transport.rs`、`crates/client-backend/src/udp_client.rs`

UDP datagram header：

```text
magic          4 bytes  "G4U1"
version        1 byte
frame_type     1 byte
request_id     8 bytes
frame_len      4 bytes
chunk_id       2 bytes
total_chunks   2 bytes
payload_len    2 bytes
prev_checksum  4 bytes
next_checksum  4 bytes
```

设计点：

- 单节点 8 卡 payload 通常可以单包承载，但代码仍保留 chunk 机制，避免未来字段增加、进程数增多或 MTU 降低时失败。
- 每个 chunk 携带相邻 chunk 校验码；重组时能快速发现丢包、错包和乱序损坏。
- `udp_mtu = 0` 时按路由探测 MTU，失败 fallback 到 1200。
- 查询路径支持空 `QueryRequest` datagram，减少一次 JSON payload 构造。

## TCP transport

文件：`crates/server/src/main.rs`、`crates/client-backend/src/tcp_client.rs`

TCP 与 UDP 使用相同的 frame/payload：

- 握手返回 `MetadataPayload`。
- 查询返回 `RuntimePayload`。
- 同一 client-backend 和同一 server 之间保留一条持久 TCP 连接，不再每次前端 QUERY 重新建连。

## Cache 和刷新机制

server 侧 `GresCache`：

- 后台按 `collector_interval_ms` 轮询 collector。
- query 路径通过 `get_latest_or_refresh` 获取最新快照。
- 并发 stale 请求 coalesce，避免多个请求同时打到 NVML。
- server 输出的 process command 会在传输前清空，减少 payload 并避免把命令行传到客户端。

client-backend 侧 cache：

- 使用本地时间作为 `record_timestamp`，避免跨节点时钟漂移影响 TTL。
- 前端每次 `QUERY` 时检查 TTL，只有记录缺失或超过 `cache_ttl_ms` 才请求 server。
- `last_query_latency_us` 记录前端触发 refresh 时的实时网络查询耗时。

## 性能设计

- 静态信息和运行时信息拆分：握手阶段缓存 hostname、driver、GRES name、UUID、total memory；查询阶段只发送温度、利用率、used memory、进程 UID/PID/显存。
- UID 在客户端解析成 username，避免服务端传字符串并降低 payload 体积。
- `command` 不进入 runtime payload，进一步减少网络数据。
- UDP 单包快路径避免通用分片重组开销。
- TCP/UDP 都维持持久连接抽象，避免前端频繁 QUERY 时重复握手。
- CLI 到 backend 使用 UDS，避免本机 HTTP/TCP 的额外开销。

## 构建与测试

```bash
source /opt/shell_related/z00_lmod.sh && module load compiler/rust
cargo fmt --all
cargo test --workspace
cargo check -p server --features nvml
```

生产 server 构建通常启用真实 NVML：

```bash
cargo build --locked --release -p server --features nvml
cargo build --locked --release -p gpustat4cluster-client-backend
cargo build --locked --release -p gpustat4cluster-client-cli
```

---

# gpustat4cluster Developer Guide

[中文](#gpustat4cluster-开发文档) | [English](#gpustat4cluster-developer-guide)

This guide documents the current crate layout, collector abstraction, network protocol, cache flow, and performance design.

## Workspace

```text
crates/common          shared config, error codes, protocol structs, rkyv payloads, UDP chunk codec
crates/server          compute-node daemon: GRES/NVML collection, cache, UDP/TCP services, multicast announce
crates/client-backend  login/user-node daemon: discovery, persistent connections, cache, UDS API
crates/client-cli      command-line frontend: args, UDS query, gpustat-style rendering
```

## Collector Boundary

The server collector boundary is `GresCollector`. Production currently uses `NvmlCollector`; unit tests use `TestGresCollector`, which is compiled only under `#[cfg(test)]` and is not exposed through runtime features or environment variables.

The internal normalized model is `GresNodeSnapshot`. The stable network model is `ServerGresSnapshot`: metadata and runtime payloads use `gres` fields, while the current production implementation is NVML and the CLI renders it in a gpustat-compatible table. This keeps the user-facing experience stable while moving the extensibility point to the collector and protocol layers.

When other GRES implementations are added, add a `GresResourceKind` variant, add the collector implementation, then extend `GresResource`, `HostMetadata`, `RuntimeSnapshot`, and the CLI/frontend view only if the normalized fields are not enough. The existing gpustat-compatible rendering path should remain the default view for the NVML implementation.

### GRES Collector Contract

New collectors should add a unit test that calls `assert_gres_collector_contract(&collector)`. The contract verifies:

- Hostname is non-empty, and an empty driver version is represented as `None`.
- Resource indices are dense and zero-based.
- Resource names are non-empty.
- Total memory is greater than zero, and used memory does not exceed total memory.
- GRES utilization and memory utilization are both in `0..=100`.
- Empty UUIDs are represented as `None`, and non-empty UUIDs are unique.
- Process PID is non-zero, and process used memory does not exceed the resource total memory.
- The snapshot can be split into `HostMetadata` + `RuntimeSnapshot` and rebuilt without loss.
- The snapshot round-trips through the rkyv payload encoder/decoder.

## Transport

TCP and UDP now share the same payload types:

- Handshake: `HandshakeRequest` -> `MetadataPayload(HostMetadata)`.
- Query: `QueryRequest` -> `RuntimePayload(RuntimeSnapshot)`.
- Error: `QueryResponse`.
- Graceful close: `Disconnect`.

TCP writes complete binary frames directly to the stream. UDP wraps the same frame bytes in `G4U1` chunks with chunk id, total chunk count, payload length, and neighboring checksums.

## Cache Flow

- Server refreshes collector cache at `services.collector_interval_ms`.
- Client-backend refreshes from server only when frontend `QUERY` sees a missing or expired local record.
- Client freshness uses local client-backend time, not server time.
- Query latency is measured on the client-backend around the transport query and displayed by the CLI when enabled.

## Build

```bash
source /opt/shell_related/z00_lmod.sh && module load compiler/rust
cargo fmt --all
cargo test --workspace
cargo check -p server --features nvml
```
