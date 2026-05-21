# Packaging smoke

[中文](#packaging-smoke) | [English](#packaging-smoke-english)

本页用于本地检查 release tarball 内容，不替代 GitHub Actions tag release。

## 自动 package smoke

推荐优先执行脚本版检查。脚本不需要 sudo，不写 `/etc`，只在临时目录内组装并解包验证本地 release artifact：

```bash
bash -n scripts/smoke-package-artifact.sh
scripts/smoke-package-artifact.sh
```

脚本会：

- 构建 release 版 `server`，启用 `kcp-transport`。
- 构建 release 版 `gpustat4cluster-client-backend`，启用 `kcp-transport`。
- 构建 release 版 `gpustat4cluster` CLI。
- 生成本地 server/client tarball。
- 解包检查二进制存在且可执行。
- 检查 systemd service 存在。
- 检查 `etc/gpustat4cluster/gpustat4cluster.env.example` 存在并包含 KCP env 示例。

如果已经提前完成 release build，可用以下变量只验证打包布局：

```bash
GPUSTAT4CLUSTER_PACKAGE_SMOKE_SKIP_BUILD=1 scripts/smoke-package-artifact.sh
```

## 前置环境

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
```

## 本地 build release

当前 release workflow 会为 server 和 client-backend 启用 `kcp-transport` feature：

```bash
cargo build --locked --release -p server --features kcp-transport
cargo build --locked --release -p gpustat4cluster-client-backend --features kcp-transport
cargo build --locked --release -p gpustat4cluster-client-cli
```

## 本地组装 tarball

以下示例展示脚本内部等价的 amd64 gnu 形态本地 smoke tarball 结构，日常请优先使用 `scripts/smoke-package-artifact.sh`：

```bash
rm -rf /tmp/gpustat4cluster-package-root /tmp/gpustat4cluster-dist
mkdir -p /tmp/gpustat4cluster-package-root/usr/local/bin
mkdir -p /tmp/gpustat4cluster-package-root/etc/gpustat4cluster
mkdir -p /tmp/gpustat4cluster-package-root/etc/systemd/system
mkdir -p /tmp/gpustat4cluster-dist

cp target/release/server /tmp/gpustat4cluster-package-root/usr/local/bin/gpustat4cluster-server
cp target/release/gpustat4cluster-client-backend /tmp/gpustat4cluster-package-root/usr/local/bin/
cp target/release/gpustat4cluster /tmp/gpustat4cluster-package-root/usr/local/bin/gpustat4cluster-client
cp packaging/systemd/gpustat4cluster-server.service /tmp/gpustat4cluster-package-root/etc/systemd/system/
cp packaging/systemd/gpustat4cluster-client.service /tmp/gpustat4cluster-package-root/etc/systemd/system/
cat > /tmp/gpustat4cluster-package-root/etc/gpustat4cluster/gpustat4cluster.env.example <<'EOF'
# Optional gpustat4cluster runtime environment.
# GPUSTAT4CLUSTER_ENABLE_KCP=1
# GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:30000
# GPUSTAT4CLUSTER_COLLECTOR=mock
# GPUSTAT4CLUSTER_FORCE_MOCK=1
EOF

tar -C /tmp/gpustat4cluster-package-root -czf /tmp/gpustat4cluster-dist/gpustat4cluster-local-linux-amd64-gnu.tar.gz .
```

## 内容检查

```bash
tar -tzf /tmp/gpustat4cluster-dist/gpustat4cluster-local-linux-amd64-gnu.tar.gz | sort
```

必须包含：

- `./usr/local/bin/gpustat4cluster-server`
- `./usr/local/bin/gpustat4cluster-client-backend`
- `./usr/local/bin/gpustat4cluster-client`
- `./etc/systemd/system/gpustat4cluster-server.service`
- `./etc/systemd/system/gpustat4cluster-client.service`
- `./etc/gpustat4cluster/gpustat4cluster.env.example`

systemd service 必须包含：

```bash
grep -R 'EnvironmentFile=-/etc/gpustat4cluster/gpustat4cluster.env' packaging/systemd
```

## install dry-run

在线 URL dry-run：

```bash
bash scripts/install.sh --role server --version v0.0.0 --dry-run
bash scripts/install.sh --role client --version v0.0.0 --dry-run
bash scripts/install.sh --role both --version v0.0.0 --dry-run
```

本地 tarball dry-run 可确认脚本会选择相同命名：

```bash
LOCAL_TARBALL_DIR=/tmp/gpustat4cluster-dist bash scripts/install.sh --role both --version local --dry-run
```

真实安装验证请在可重置的 systemd 测试机执行，避免污染开发机。

---

# Packaging smoke (English)

This page describes the local release-artifact smoke test. It does not replace the GitHub Actions release workflow, but it catches packaging layout regressions before publishing.

Recommended command:

```bash
bash -n scripts/smoke-package-artifact.sh
scripts/smoke-package-artifact.sh
```

The script builds release binaries, assembles local server/client artifacts, extracts them into a temporary directory, and verifies that required binaries, config examples, and service files exist.

If release binaries are already built, skip the build phase:

```bash
GPUSTAT4CLUSTER_PACKAGE_SMOKE_SKIP_BUILD=1 scripts/smoke-package-artifact.sh
```

Expected checks:

- `gpustat4cluster-server` exists and is executable in the server package.
- `gpustat4cluster-client-backend` and `gpustat4cluster-client` exist and are executable in the client package.
- systemd service files are present for the relevant role.
- config examples are present and match the current runtime keys.
- obsolete env example files should not be shipped in final release packages unless they are intentionally restored.
