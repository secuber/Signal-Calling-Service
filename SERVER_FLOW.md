# 群组视频服务端流程说明 (Server Flow)

本文档描述了 Signal Calling Service (SFU) 的服务端主要流程、接口定义及调用顺序。

## 1. 核心流程概述

群组视频通话建立的过程主要包含以下几个阶段：

1.  **信令握手 (Signaling Handshake)**: 客户端通过 HTTP REST API 向 SFU 发起加入请求，交换身份信息和密钥协商参数。
2.  **传输层建立 (Transport Establishment)**: 客户端与 SFU 通过 ICE (Interactive Connectivity Establishment) 协议建立 UDP/TCP 连接。
3.  **密钥协商 (Key Negotiation)**: 双方使用在信令阶段交换的公钥 (ECDH/SM2) 计算出共享密钥，用于后续媒体数据的加密传输。
4.  **媒体传输 (Media Transport)**: 建立 SRTP (Secure Real-time Transport Protocol) 通道，开始双向传输音视频数据。

---

## 2. 详细接口说明

服务端主要仅暴露极简的 HTTP 接口用于“入会 (Join)”，其余交互均在 UDP/TCP 长连接中进行。

### 2.1 加入房间 (Join Call)

*   **Endpoint**: `POST /v1/call/:call_id/client/:demux_id`
*   **功能**: 客户端请求加入指定的通话房间。
*   **参数说明**:
    *   `call_id`: 通话/房间的唯一标识符 (Hex string)。
    *   `demux_id`: 客户端在此次通话中的唯一标识 (整数)。
*   **请求体 (JSON)**:
    ```json
    {
      "endpointId": "UserA",        // 用户ID
      "clientIceUfrag": "...",      // 客户端生成的 ICE Fragment用户名
      "clientDhePublicKey": "...",  // 客户端生成的临时公钥 (Hex)
      "hkdfExtraInfo": null,        // 可选：用于密钥派生的额外信息
      "region": "us-west",          // 客户端所在区域
      "roomId": "..."               // 房间ID
    }
    ```
*   **关键处理逻辑**:
    1.  **验证参数**: 校验房间号、用户ID格式。
    2.  **密钥解析**: 解析 `clientDhePublicKey`。服务端根据配置支持标准格式 (0x04前缀) 或 SM2 压缩格式 (0x03/0x02前缀)。
    3.  **资源分配**: 在内存中查找或创建对应的 `Call` 对象，并尝试添加该 `Client`。
    4.  **服务端生成参数**: 服务端生成自己的临时公钥 (`server_dhe_public_key`) 和 ICE 凭证 (`server_ice_ufrag`, `server_ice_pwd`)。
*   **响应体 (JSON)**:
    ```json
    {
      "serverIp": "...",           // SFU 的公网 IP
      "serverPort": 11102,         // UDP 端口
      "serverIceUfrag": "...",     // 服务端 ICE 用户名
      "serverIcePwd": "...",       // 服务端 ICE 密码
      "serverDhePublicKey": "..."  // 服务端临时公钥
    }
    ```
*   **后续动作**: 客户端收到响应后，立刻根据 `serverIp` 和 `serverPort` 发起 STUN 绑定请求，并使用双方公钥计算共享密钥。

### 2.2 获取房间成员 (Get Clients)

*   **Endpoint**: `GET /v1/call/:call_id/clients`
*   **功能**: 查询当前在房间内的所有客户端列表。

---

## 3. 功能函数与调用链 (Call Chain)

以下是请求处理的核心函数调用链路：

1.  **Entry Point**: `backend/src/signaling_server.rs` -> `join()`
    *   负责 HTTP 请求解析、参数校验、密钥格式兼容处理。
2.  **SFU Logic**: `backend/src/sfu.rs` -> `sfu.add_client()`
    *   获取全局锁，查找对应的 `Call` 实例。
3.  **Call Management**: `backend/src/call.rs` -> `call.add_client()`
    *   判断用户状态（是否管理员、是否需要批准）。
    *   调用 `promote_client()` 将用户升级为正式成员。
4.  **Client Promotion**: `backend/src/call.rs` -> `promote_client()`
    *   初始化客户端状态 (`Client`结构体)。
    *   分配带宽、视频层级 (`allocate_video_layers`)。
    *   **关键点**: 在此处打印详细的成员列表日志，用于调试。

---

## 4. 调试与日志

*   **日志级别**: 推荐使用 `RUST_LOG="calling_backend=debug"`。
*   **关键日志标识**:
    *   `join() called`: 表示收到 HTTP 加入请求。
    *   `Uncompressed key prefix`: 密钥格式调试信息。
    *   `promote_client`: 用户正式加入房间，开始分配资源。
    *   `call: ... adding demux_id`: SFU 层正在处理添加操作。

