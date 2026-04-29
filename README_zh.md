# Cryonet

Cryonet 是一个去中心化组网工具。

## 概述

节点通过 WebSocket 相互连接，当节点间组成连通图时 Cryonet 会自动组成转发网络，使得任意两个节点之间可以相互通信，以此作为控制平面。通过控制平面，节点与所有其他节点自动建立基于 WebRTC ICE 的全连接，对每个节点创建 TUN 接口，形成数据平面。Cryonet 将会尽量建立 n(n-1)/2 条连接，以利用网络中所有链路。在配置了 TURN 服务器的情况下，节点间若 NAT 打洞失败，还可通过 TURN 服务器进行中继转发。

Cryonet 期望用户在接口上再运行一个路由协议（如 OSPF、Babel 等），将最优路径的选择交给路由协议来完成。

## 加密

Cryonet 通过控制平面交换 ECDH 公钥，使用 AES-GCM 对数据进行加密。nonce 使用计数器生成，并定期重新交换密钥。默认情况下，Cryonet 不会对内网流量进行加密，可以通过选项修改这一行为。

## TAP 模式

默认情况下，Cryonet 会为每个节点创建一个 TUN 设备。在 TAP 模式下 Cryonet 实现了伪二层，仅会创建一个 TAP 设备，节点间会通过控制平面广播自己网络接口上的 IP 地址，并对 ARP/NDP 请求进行代答。Cryonet 维护了一套节点 ID 与 MAC 地址之间的映射，通过 MAC 地址即可计算出目标节点 ID，从而直接将数据包转发到目标节点。如果进入 TAP 设备的包的源 MAC 并非 Cryonet 所期望的 MAC，该包将被丢弃，因此你无法将此二层接口桥接到其他网络接口。对于广播包和组播包，Cryonet 会将其转发到所有节点。

在 TAP 模式下，你可以选择不在接口上运行路由协议，将同一子网的地址添加到接口上即可直接使用接口进行通信。

Cryonet 的 TAP 模式的数据包与 TUN 模式的数据包相同，因此网络中的节点可以混合使用 TUN 模式和 TAP 模式。

## wasm

Cryonet 支持在浏览器中运行。浏览器中，Cryonet 将会与其他节点协商使用 WebRTC DataChannel 进行通信。

> ### 为什么？  
> 因为好玩（

## MPLS

在开启 packet information 的情况下，Cryonet 支持传输 MPLS 包。TUN 设备在开启 packet information 的情况下可以正确处理 MPLS 包（尽管抓包工具可能无法正确解析）。

## 使用方法

请使用 `--help` 选项查看使用方法。你还可以使用 `cryoc` 命令行工具与 Cryonet 守护进程通信，查询运行状态。
