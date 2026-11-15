# Conduit ↔ Envoy 运行时联动与集成契约（v2）

> 本文档定义 **独立 VPN 网络底座 `conduit`** 与 **独立 Agent 产品 `envoy`** 之间的运行时通信契约。
>
> **核心原则**：
> 1. **零编译期耦合**：`conduit` 与 `envoy` 各自作为独立项目演进，Workspace 之间**没有任何 Cargo 依赖**。
> 2. **单一网络守护进程**：由 `conduit run` 独占管理设备密钥、QUIC/Relay 组网、TUN 虚拟网卡（`100.64.0.0/10`）、本地域名解析（`*.conduit`）与本地控制套接字 `~/.conduit/control.sock`。
> 3. **双通道互补**：`envoy` 同时支持 **通过 `control.sock` 进行服务流桥接（`service_id = 16`，无 TUN 也可用且自带 mTLS 身份）** 与 **通过虚拟 IP / 短域名进行 L3 直连（需开启 TUN）**。

---

## 1. 为什么不再使用旧版 `envoy/bridge`

旧版 `envoy/bridge` 通过 Cargo path 依赖引入 `conduit-*` crate，并在独立进程内启动第二个 `conduit_fabric::Engine`。在 `conduit` 转为独立 VPN 产品后，这种做法存在根本冲突：

- `conduit run` 已经通过 `~/.conduit/control.sock` 实行数据目录单实例锁，并独占管理 QUIC 端口与 TUN 网卡；
- 如果再启动 `envoy-bridge`，会与 `conduit run` 争抢设备身份、重复向 `coord` 注册端点，且无法共享同一个 mesh 会话与虚拟网卡。

因此，新的集成方式改为 **`envoy` 直接在运行时连接 `conduit run` 暴露的 `~/.conduit/control.sock`**（Unix Domain Socket）。旧 `envoy/bridge/` 可在迁移完成后下线或归档。

---

## 2. 路径与权限约定（Linux / macOS）

| 项目 | 约定值 | 说明 |
| :--- | :--- | :--- |
| 控制套接字默认路径 | `$CONDUIT_DATA_DIR/control.sock`，否则 `$HOME/.conduit/control.sock` | `envoy` 应支持通过 `--conduit-socket <PATH>` 或环境变量 `CONDUIT_CONTROL_SOCK` 覆盖 |
| 跨权限访问 | `conduit run` 若以 `sudo` 启动 TUN，会将 `control.sock` 属主/权限保持为原用户可读写（`0660`） | `envoy serve` 与 `envoy` CLI 始终以普通用户运行，无需 root |
| 注册服务号 | `AGENT_SERVICE_ID = 16` | `conduit` 保留 `1..=15` 给核心能力，`16..=1023` 为注册应用服务段 |
| 传输层协议 | Unix Domain Socket 上的 **HTTP/1.1 + JSON** 与 **WebSocket（RFC 6455）** | 复用标准 HTTP/WS 协议，无自定义二进制信封 |

---

## 3. 节点发现与实时事件（`envoy` 与 `conduit tui` 共用）

### 3.1 `GET /v1/status`
返回当前本机与活跃对端状态快照（含虚拟 IP 与已挂接本地服务摘要）：

```json
{
  "account": "acct_...",
  "device_id": "3fa9c1d2e4b56789",
  "device_name": "workstation",
  "dns_name": "workstation",
  "virtual_ip": "100.64.0.1",
  "tun": {
    "enabled": true,
    "interface": "tun0",
    "cidr": "100.64.0.0/10"
  },
  "connection": "direct",
  "peers": [
    {
      "device_id": "91ff0e2c88aabb11",
      "name": "laptop",
      "dns_name": "laptop",
      "virtual_ip": "100.64.0.2",
      "path": "direct",
      "rtt_ms": 14
    }
  ],
  "services": [
    {
      "service_id": 16,
      "name": "envoy",
      "online": true,
      "listeners": 2
    }
  ]
}
```

> 字段兼容性：`dns_name`、`virtual_ip` 在尚未分配或离线/旧数据时可能为 `null`；`tun` 在 `--no-tun` 模式下 `enabled` 为 `false`。

### 3.2 `GET /v1/devices`
返回账号下全部已验证证书设备（含离线/已吊销设备）：

```json
{
  "account": "acct_...",
  "devices": [
    {
      "device_id": "3fa9c1d2e4b56789",
      "name": "workstation",
      "dns_name": "workstation",
      "virtual_ip": "100.64.0.1",
      "status": "active",
      "issuer": "recovery code",
      "me": true
    },
    {
      "device_id": "91ff0e2c88aabb11",
      "name": "laptop",
      "dns_name": "laptop",
      "virtual_ip": "100.64.0.2",
      "status": "active",
      "issuer": "3fa9c1d2e4b56789",
      "me": false
    }
  ]
}
```

### 3.3 `GET /v1/events`（WebSocket 实时推送）
- 客户端通过 `control.sock` 升级到 WebSocket；
- 连接成功后**立即收到一条初始快照**，之后每当节点列表、直连/中继状态、RTT、虚拟 IP 或本地服务注册状态变化时收到推送帧（`Message::Text` JSON）：

```json
{
  "type": "status",
  "status": { /* 与 GET /v1/status 相同的 JSON 对象 */ }
}
```

---

## 4. 方式一：`control.sock` Service 流桥接（`service_id = 16`，推荐默认）

即使未启动 TUN（如 `--no-tun`、容器内或无 root 权限），`envoy` 也可以通过 `control.sock` 的 WebSocket 桥接复用 `conduit` 的 QUIC/Relay 多路复用流，并由 `conduit` 直接用 **mTLS 证书**担保对端 `device_id`。

`envoy` 只需封装一个极小的 `WsByteStream`（将 WebSocket `Message::Binary` 适配为 `AsyncRead + AsyncWrite + Unpin + Send`；drop / `poll_shutdown` 时发送 WS `Close`，`conduit` 侧随即半闭底层 `Channel`）。

### 4.1 出站开流（Client / `AgentDialer` 侧）

- **请求**：`GET /v1/services/16/open/{target}`（WebSocket Upgrade）
- **`{target}` 解析优先级（由 `conduit` 在当前 `active` 设备中匹配，不含本机）**：
  1. 精确匹配 `device_id`
  2. 精确匹配 `dns_name`（忽略大小写，允许带或不带 `.conduit` 后缀）
  3. 精确匹配 `virtual_ip`（如 `100.64.0.2`）
  4. 唯一匹配 `DeviceCert.name`（忽略大小写）
- **升级失败（直接返回 HTTP 错误与 JSON `{"error": "..."}`，方便 CLI 直接打印原因）**：
  - `400 Bad Request`：非法 `service_id`（如 `0`）或空目标
  - `404 Not Found`：找不到该目标设备（或已被吊销）
  - `409 Conflict`：设备名不唯一，需改用 `device_id` 或 `dns_name`
  - `503 Service Unavailable`：对端离线、拨号失败或开流超时
- **升级成功（`101 Switching Protocols`）**：
  - 响应头附带：`X-Conduit-Peer-Id`、`X-Conduit-Peer-Name`、以及可选的 `X-Conduit-Peer-Ip`；
  - 升级后即为**纯净的全双工透明字节流**，双向透传 `envoy-proto` v2 帧（`Op` / `Reply` / `ClientMsg`），无需任何前缀帧。

### 4.2 入站接单（`envoy serve` 侧 — Accept Pool 模型）

- **请求**：`GET /v1/services/16/accept?name=envoy`（WebSocket Upgrade）
- **连接即注册**：只要 `envoy serve` 至少维持 1 条已升级的 `/v1/services/16/accept` 连接，`conduit` 即认为本地 `service_id = 16` 在线（并在 `/v1/status` 的 `services` 中体现）。
- **推荐池大小**：`envoy serve` 维持 **2~4 条**空闲等待连接（类似 `accept` 队列），断线后按指数退避自动重连；每当一条连接被分配给入站流，立刻补充一条新的连接。
- **交接协议（非常关键）**：
  1. 当远端设备向本机发起 `service_id = 16` 的流时，`conduit` 从空闲池取出一条 `/v1/services/16/accept` WebSocket 连接；
  2. `conduit` 发送**唯一一帧 `Message::Text`** 作为调用方身份交接元数据：
     ```json
     {
       "event": "accepted",
       "service_id": 16,
       "peer_device_id": "3fa9c1d2e4b56789",
       "peer_name": "workstation",
       "peer_dns_name": "workstation",
       "peer_virtual_ip": "100.64.0.1",
       "connection": "direct"
     }
     ```
  3. 从**下一帧开始**，该 WebSocket 连接进入**纯净全双工二进制透传模式**，直接桥接对端的 `conduit_net::Channel`；
  4. `envoy serve` 收到 `Message::Text` 解析出 `peer_device_id`（由 `conduit` mTLS 严格认证，不可伪造）后，包装剩余二进制流为 `WsByteStream`，`tokio::spawn(agent_server.serve_stream(&peer_device_id, stream))` 并立刻补齐 accept 池。
- **异常处理**：如果远端开流时本地没有任何挂起的 `/v1/services/16/accept`，`conduit` 会立即关闭该入站 `Channel`（不阻塞网元循环，远端会干净地收到 EOF / 连接被拒）。

```text
envoy --on laptop                 conduit (local)              conduit (laptop)             envoy serve (laptop)
      │                                 │                             │                             │
      │                                 │                             │◀── WS /v1/services/16/accept (空闲等待)
      │── WS /v1/services/16/open/laptop│                             │                             │
      │                                 │── EngineHandle::open(16) ──▶│                             │
      │◀─ 101 Switching Protocols ──────│                             │── Text {"event":"accepted", │
      │                                 │                             │         "peer_device_id"} ─▶│
      │                                 │                             │                             │── tokio::spawn(serve_stream)
      │◀══════ Binary <-> Channel ═════▶│◀═════ QUIC / Relay ════════▶│◀═════ Binary <-> Channel ══▶│
```

---

## 5. 方式二：L3 虚拟内网直连（TUN + `*.conduit` DNS）

当 `conduit` 开启 TUN 模式且 `/v1/status` 中 `tun.enabled == true` 时，`envoy` 也可以像普通局域网服务一样直接使用 TCP/HTTP/gRPC 走 `100.64.0.0/10` 网段互通（适合大文件高吞吐传输或第三方工具直连）：

1. **服务端绑定**：`envoy serve` 可以查询 `/v1/status` 获取本机 `virtual_ip`，仅绑定 `<self_virtual_ip>:<port>`（例如 `100.64.0.2:7420`），避免监听在物理局域网网卡上。
2. **目标寻址**：`envoy --on <target>` 可先查询 `GET /v1/devices` 找到目标的 `virtual_ip`，或直接连接 `<dns_name>.conduit:<port>`。
3. **身份校验规则（必读）**：
   - `conduit` 的 TUN 数据面在入口处强制校验 **源 IP 防伪**：从对端 `peer_device_id` 的会话收到的 IP 包，其源 IP 必须等于该设备被分配的 `virtual_ip`，否则直接丢弃；
   - 因此在 TUN 内网中，TCP 连接的 `peer_addr().ip()` 可信地对应唯一的活跃设备；
   - `envoy` 在接收直连 TCP 时，**必须**通过本地缓存的 `/v1/devices`（或 `/v1/events`）把 `peer_addr().ip()` 反查回 `device_id`，再送入 `AgentConfig.allowlist` 鉴权（切勿跳过 `allowlist` 直接放行）。

---

## 6. 给 `envoy` 仓库的实现清单

> **当前进度同步**：
> - `conduit` 侧已完成底层数据报与并发 Mesh 底座（Step 1–2），接下来将按顺序落地：
>   - **Step 3**：`conduit-coord` IPAM（`100.64.0.0/10` + 唯一 `dns_name`）与状态字段输出
>   - **Step 4**：`conduit-vpn`（Linux/macOS TUN 与 `*.conduit` 本地 DNS）
>   - **Step 5**：`control.sock` 上的 `/v1/events` 与 `/v1/services/{id}/open|accept` 桥接
>   - **Step 6**：`conduit tui`
> - `envoy` 侧**无需等待 `conduit` 全部落完**，现在就可以先基于本契约完成自身适配与本地 Mock UDS 测试：

1. **在 `envoy-service` 新增 `conduit` 运行时适配器（纯 UDS + WS，不引入 `conduit-*` 依赖）**：
   - 新增 `WsByteStream`（`AsyncRead + AsyncWrite` 封装 WebSocket Binary 帧）。
   - 新增 `ConduitClient`（连接 `CONDUIT_CONTROL_SOCK` / `~/.conduit/control.sock`）：
     - `devices()` / `status()` / `subscribe_events()`
     - `ConduitDialer`（实现 `AgentDialer`）：默认走 `GET /v1/services/16/open/{peer}`；可选支持 L3 模式（通过 `devices()` 解析 `virtual_ip` 直连 `<ip>:<port>`）。
   - 修改 `envoy_service::dialer_for`：除 `local` 外使用 `ConduitDialer`（或在迁移期保留 `ENVOY_BRIDGE_SOCK` 兼容 `RouteDialer`）。
2. **在 `envoy-cli` 的 `envoy serve` 接入入站桥接**：
   - 后台常驻任务连接 `control.sock` 的 `GET /v1/services/16/accept?name=envoy`，维持 2~4 条等待连接，断线指数退避重连（`conduit` 未启动时静默等待重试，不影响本地 `envoy.sock` 正常使用）。
   - 收到 `"event": "accepted"` 文本帧后，读取 `peer_device_id` 并 `tokio::spawn(server.serve_stream(&peer_device_id, ws_byte_stream))`。
   - 若同时启用 L3 TCP 监听（可选 `--listen-vpn <port>`），对接入连接按 `remote_ip -> device_id` 反查后再调用 `AgentServer::serve_stream(&device_id, tcp_stream)`。
3. **测试与清理**：
   - 在 `envoy-service` 中使用本地 `tokio::net::UnixListener` + `axum`/`tokio-tungstenite` 编写 Mock `control.sock` 集成测试，覆盖：`/v1/services/16/open/{peer}` 正常与 `404/409/503` 错误传播、`/v1/services/16/accept` 身份交接与 `allowlist` 拦截、流关闭取消 `JobLog` 推送等场景。
   - 迁移验证完成后，移除或归档不再使用的 `envoy/bridge/`。
