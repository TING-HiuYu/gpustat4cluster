# CLI compatibility with gpustat (Round 1)

## 对齐项

- 支持 `-n <filter>`：用于按 hostname/IP/connectionId 子串过滤节点。
- 支持 `-user <username>` 参数入口（当前后端接收该字段，渲染层尚未做真实进程级过滤）。
- 表格输出包含最小核心列：`hostname`、`GPU 利用率`、`显存使用/总量`。
- 当 backend 未运行时，CLI 提示先启动 `gpustat4cluster-client-backend`。

## 未对齐项（后续迭代）

- 尚未实现与 gpustat 完全一致的参数集合。
- 尚未接入真实 NVML 进程列表，`-user` 目前为协议透传。
- 尚未支持彩色输出、排序策略、watch 模式与增量局部刷新。
- 当前 GPU 数据来自 backend cache 占位字节解析，不代表真实节点状态。
