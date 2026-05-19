# gpustat4cluster 运维安装指南

## 在线安装

```bash
curl -fsSL https://raw.githubusercontent.com/gpustat4cluster/gpustat4cluster/main/scripts/install.sh -o install.sh
bash install.sh --role server --version latest --libc gnu
```

支持 `--role server|client|both`，默认安装到 `/usr/local/bin`，并初始化 `/etc/gpustat4cluster/config.toml`。

## 离线安装

1. 在联网环境下载对应包：
   - `gpustat4cluster-server-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz`
   - `gpustat4cluster-client-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz`
2. 拷贝到目标机后解压：

```bash
sudo tar -xzf gpustat4cluster-server-<...>.tar.gz -C /
sudo tar -xzf gpustat4cluster-client-<...>.tar.gz -C /
sudo systemctl daemon-reload
sudo systemctl enable --now gpustat4cluster-server.service
sudo systemctl enable --now gpustat4cluster-client.service
```

## 回滚

1. 停止服务：

```bash
sudo systemctl disable --now gpustat4cluster-server.service gpustat4cluster-client.service
```

2. 恢复上一版本二进制（建议保留旧 tar 包或通过包管理系统版本锁定）。
3. `sudo systemctl daemon-reload` 后重新启动对应服务。

## 卸载

```bash
sudo systemctl disable --now gpustat4cluster-server.service gpustat4cluster-client.service || true
sudo rm -f /usr/local/bin/gpustat4cluster-server
sudo rm -f /usr/local/bin/gpustat4cluster-client-backend
sudo rm -f /usr/local/bin/gpustat4cluster-client
sudo rm -f /etc/systemd/system/gpustat4cluster-server.service
sudo rm -f /etc/systemd/system/gpustat4cluster-client.service
sudo systemctl daemon-reload
```

保留配置：`/etc/gpustat4cluster/config.toml`（如需彻底删除可手动移除）。
