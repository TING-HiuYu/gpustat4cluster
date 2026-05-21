# CLI compatibility with gpustat

[中文](#对齐项) | [English](#english-summary)

## 对齐项

- 支持 `-n <filter>`：用于按 hostname/IP/addr/connectionId 过滤节点。
  - 支持逗号分隔：`-n node-a,node-b`
  - 支持方括号数字范围：`-n node[01-04]`
  - 支持方括号列表：`-n node[1,3,5]`
  - 支持星号通配符：`-n gpu-*`、`-n *.cluster`、`-n 10.0.0.*`
  - 支持 IP 精确匹配：`-n 10.0.0.1` 不会匹配 `10.0.0.10`
  - 支持 connection id 匹配，例如 `-n conn-007`
  - 逗号组合会在展开后去重，例如 `-n node1,node[1,3]`
- 支持 `-user <username>` 渲染层过滤：
  - 当 backend response 的 GPU 记录包含 `processes` 字段时，仅展示含匹配用户进程的 GPU 行，并只渲染匹配用户的进程摘要。
  - 未传 `-user` 时会在 GPU 行下方渲染简洁进程摘要：`proc <user> pid=<pid> mem=<MB> <command>`。
  - 当当前 response 没有 `processes` 字段时保持 no-op，继续展示 GPU 行。
- 支持 watch/刷新入口：
  - `--watch` / `-w`：持续刷新，当前先使用全量重绘。
  - `--watch <secs>` / `-w <secs>`：持续刷新并设置刷新间隔。
  - `--interval <secs>` / `--refresh <secs>` / `--refresh-interval <secs>` / `-i <secs>`：设置刷新间隔。
  - 当前 watch 没有启用 raw mode，不修改终端输入模式；Ctrl-C 退出后无需额外恢复 terminal 状态。
- 支持 `--json` 输出入口，面向 smoke/stress 自动化解析；默认表格输出不变。
  - JSON 顶层格式为 `{"meta":{...},"nodes":[...]}`。
  - `meta.status` 取值当前为 `ok`、`empty` 或兼容旧 backend/fake backend 的 `unknown`。
  - `meta.timestamp_ms` 为 backend 生成 response 的时间戳；旧 backend/fake backend 缺字段时为 `0`。
  - `meta.node_count` 为 backend 返回节点数；旧 backend/fake backend 缺字段时为 `0`。
  - `meta.errors` 预留 backend 级错误列表，目前 KCP/static/discovery 错误主要写入 backend warning 日志。
  - 每个 node 包含 `hostname`、`stale`、`error` 和 `gpus`。
  - `stale=true` 表示当前行来自 fallback/占位 cache，未被最新 common snapshot 刷新；KCP/common snapshot 成功接入时为 `false`。
  - `error` 为节点级错误预留字段，目前未设置时为 `null` 或省略。
  - 每个 GPU 包含 `index`、`util`、`mem_used_mb`、`mem_total_mb`、`processes`。
  - `processes` 在缺字段时可为 `null`，有数据时为进程数组，包含 `username`、`pid`、`command`、`used_memory_mb`。
- 支持 client-backend frontend API 使用 UDS，TCP 保留为开发和兼容 fallback。
  - backend 设置 `GPUSTAT4CLUSTER_BACKEND_SOCKET=/tmp/gpustat4cluster.sock` 时监听 UDS。
  - CLI 查询 UDS 优先级：`--backend-socket` / `--backend-uds` > `GPUSTAT4CLUSTER_BACKEND_SOCKET`。
- 支持 client-backend TCP local API 地址覆盖，生产默认仍为 `127.0.0.1:4521`。
  - backend 监听地址优先读取 `GPUSTAT4CLUSTER_BACKEND_ADDR`，兼容 `GPUSTAT4CLUSTER_LOCAL_API_ADDR`。
  - CLI 查询地址优先级：`--backend-addr` > `GPUSTAT4CLUSTER_BACKEND_ADDR` > `GPUSTAT4CLUSTER_LOCAL_API_ADDR` > `127.0.0.1:4521`。
  - 多 backend 并行测试示例：backend A 设置 `GPUSTAT4CLUSTER_BACKEND_ADDR=127.0.0.1:4521`，backend B 设置 `GPUSTAT4CLUSTER_BACKEND_ADDR=127.0.0.1:4522`；CLI 分别使用 `--backend-addr 127.0.0.1:4521` 和 `--backend-addr 127.0.0.1:4522` 查询。
- 表格输出按 gpustat 风格显示：节点 hostname 作为区块标题，每张 GPU 显示 index、name、temperature、util、显存和进程摘要。
- 当 backend 未运行时，CLI 提示先启动 `gpustat4cluster-client-backend`。
- client backend 兼容当前 legacy server 发现通告中暂缺 `version` 字段的 JSON，同时仍会拒绝显式不匹配的协议版本。
- backend 内部已拆分 discovery、filter、cache view、local API、JSON adapter，后续 KCP/rkyv 接入点集中在 adapter/cache 层。
- backend adapter payload 解析优先级：
  - 优先解析 server JSON 中的 `payload_b64`，base64 decode 后调用 `common::decode_snapshot_payload()`。
  - 兼容解析 server JSON 中的 `payload` 字节数组，同样调用 `common::decode_snapshot_payload()`。
  - 如果没有 current snapshot payload 字段，则回退 legacy JSON `{gpu_num, avg_utilization}`。
  - 最后回退 legacy CSV payload：`index,util,mem_used_mb,mem_total_mb;...`。
- KCP client connector 已 behind feature gate：
  - 构建时启用 `--features kcp-transport` 后，设置 `GPUSTAT4CLUSTER_ENABLE_KCP=1` 会在每次 frontend `QUERY` 时检查连接私有缓存；缓存为空或超过 `services.cache_ttl_ms` 才向对应 server 发 KCP `QueryRequest`。
  - 可用 `GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:30000` 指定静态节点；支持单节点或逗号列表，例如 `GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:39400,127.0.0.1:39401`，并会与 discovery 结果按地址去重合并。
  - 静态节点会 trim 空白、按地址去重；非法地址会打印 warning 并跳过，不会阻止 backend 启动。
  - 默认不启用该 env，继续使用 discovery 生成的 fallback cache 与本地 JSON local API。
  - 如果 env 启用但未使用 `kcp-transport` feature 构建，backend 会打印 warning 并继续 fallback。
  - 如果 KCP query 失败，backend 会打印 warning，保留已有 fallback cache，不会崩溃或清空 local API 数据。
  - multicast discovery 无结果但有 static nodes 时继续使用 static nodes；两者都为空时 backend 仍启动，CLI 表格为空、`--json` 返回 `{"meta":{"status":"empty",...},"nodes":[]}`，并打印 warning。

## 未对齐项

- 尚未实现与 gpustat 完全一致的参数集合。
- 尚未接入真实 NVML 进程列表；`-user` 的真实过滤依赖 backend response 提供进程字段。
- 尚未支持彩色输出、排序策略与真正的增量局部刷新。
- 当前 GPU 数据来自 backend cache 占位字节解析，不代表真实节点状态。
- backend 尚未建立真实 KCP 连接池/连接私有 Bytes 缓存；当前 cache 由发现节点生成占位记录。
- backend 本地查询响应仍为兼容 JSON response；KCP 接入后在 cache 层保存 common snapshot payload，adapter 会转换到 CLI view。
- KCP connector 当前会顺序查询 discovered/static 节点并 upsert 到 backend cache；连接池、重连和请求合并等待下一轮。

---

## English Summary

This page tracks CLI compatibility with `gpustat` for the current implementation.

Aligned behavior:

- `-n <filter>` filters nodes by hostname, IP address, advertised address, or connection id. It supports comma-separated values, numeric ranges such as `node[01-04]`, lists such as `node[1,3,5]`, wildcard patterns, exact IP matching, and deduplication after expansion.
- `-user <username>` filters rendered GPU rows by process owner when process data is present. Without `-user`, process summaries are rendered below GPU rows when available.
- Watch mode supports `--watch`, `-w`, `--interval`, `--refresh`, `--refresh-interval`, and `-i`; the current implementation uses full-screen redraws instead of raw terminal mode.
- `--json` emits a machine-readable response shaped as `{"meta": {...}, "nodes": [...]}` for smoke and stress automation.
- The frontend-to-backend local API uses UDS in production. Older local TCP paths were retained only during development and should not be used for new deployments.
- Table output intentionally follows the `gpustat` style: each node is rendered as a hostname block and each GPU row shows index, model name, temperature, utilization, memory, and processes.
- KCP transport is feature-gated. When enabled, the client backend checks its private cache on each frontend `QUERY` and only asks the server when the cache is missing or expired.

Known gaps:

- The full `gpustat` argument surface is not implemented yet.
- Color rendering, sorting parity, and true incremental redraw are still not implemented yet.
- Some development-only compatibility paths are documented here for historical context and may be removed before a stable release.
