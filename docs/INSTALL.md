# 安装

## 一行安装（推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/Elysia5250/envoy/main/scripts/install.sh | sh
```

安装完成后**新开一个 shell**，`envoy` 就能直接用了：

```bash
envoy --version
envoy status          # 首次运行会自动拉起本机守护进程
envoy run "uname -a"
envoy ui
```

默认解析 `Elysia5250/envoy` 的最新 GitHub Release。装固定版本或换仓库：

```bash
curl -fsSL https://raw.githubusercontent.com/Elysia5250/envoy/main/scripts/install.sh \
  | sh -s -- --version 0.1.0
curl -fsSL ... | sh -s -- --repo your-org/envoy
```

## 安装器做了什么

1. 按 `uname` 选目标三元组（`aarch64-apple-darwin` / `x86_64-apple-darwin` /
   `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`）；
2. 默认查 GitHub Releases 的 `latest`，下载
   `releases/download/v<version>/envoy-v<version>-<target>.tar.gz`；
3. 有 `.sha256` 就校验；
4. 解压到 `~/.envoy/bin/envoy`（可用 `--dir` 改）；
5. 把 `~/.envoy/bin` 写进当前 shell 的启动文件（`--no-path` 可跳过）；
6. 打印后续步骤。

它**不会**自动开机自启。要的话：

```bash
envoy daemon install        # launchd / systemd --user
# 或者安装时一步到位：
curl -fsSL ... | sh -s -- --service
```

## 独立运行（不装 conduit）

envoy 是可以单独用的：本地守护进程 + job 模型 + TUI/MCP 都不依赖 conduit。

```bash
envoy status                 # conduit: not running
envoy run "echo hi"          # 照常工作
envoy ui                     # 机器列表里只有 local 和当前目标
```

`envoy serve` 默认会尝试挂 conduit 的 accept 池；conduit 不在时会静默退避重试，不影响本地使用。
完全不想让它尝试就加 `--local-only`。

## 发版（维护者）

推 tag 即可，`.github/workflows/release.yml` 会为 linux-x64、linux-arm64、
macOS-arm64、macOS-x64 各构建一个 tar.gz + sha256 并挂到 Release：

```bash
# 先把 Cargo.toml 里的 workspace version 改成 0.1.0 并提交
git tag v0.1.0
git push origin v0.1.0
```

工作流会校验 tag 与 workspace 版本一致。已有 release 后，安装一行命令立即生效。
不想用 CI 时也可以手动上传 `scripts/package.sh` 的产物：

```bash
gh release create v0.1.0 dist/v0.1.0/* --repo Elysia5250/envoy
```

## 自建静态镜像

发布产物由 `scripts/package.sh` 生成：

```bash
./scripts/package.sh                     # 当前平台
./scripts/package.sh --target aarch64-apple-darwin
```

产出布局（把这个目录原样上传到任意静态服务器 / GitHub Releases）：

```
dist/latest
dist/v0.1.0/envoy-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
dist/v0.1.0/envoy-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
```

然后：

```bash
curl -fsSL https://YOUR_HOST/envoy/install.sh | sh -s -- --base-url https://YOUR_HOST/envoy/releases
```

安装器里的默认地址是 `https://github.com/Elysia5250/envoy/releases/download`；仓库挪地方就改
`scripts/install.sh` 顶部的 `DEFAULT_BASE`，或始终传 `--base-url`。

## 从源码安装

不想用发布源时：

```bash
cargo build --release -p envoy-cli
install -m 0755 target/release/envoy ~/.local/bin/envoy
envoy --version
```

## 卸载

```bash
envoy daemon uninstall     # 如果装过自启动
rm -rf ~/.envoy/bin        # 二进制
rm -rf ~/.envoy            # 数据目录（agent.json、jobs/、日志）
```

## 平台说明

- 支持 **macOS** 与 **Linux**（x86_64 / aarch64）。
- 只要不是为了给用户工具跑虚拟内网，**不需要 TUN、不需要 root**：agent 流量走
  `control.sock` 的 WebSocket 桥。
- 守护进程按数据目录加 `flock`，同一台机器重复 `envoy serve` 会直接报错而不是互相覆盖。
