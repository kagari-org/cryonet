# Cryonet

A decentralized networking daemon.

## Overview

Nodes connect to each other over WebSocket connections to form a control plane. These WebSocket links do not need to be fully meshed — as long as the nodes are connected, the built-in routing protocol will automatically discover the topology and compute shortest paths. Using the routing information, Cryonet then establishes SRTP over WebRTC between every reachable pair of nodes, forming a full mesh network as a data plane.

## Usage

### Daemon (`cryonet`)

```
cryonet <ID> [OPTIONS]
```

| Argument | Env Variable | Description | Default |
|---|---|---|---|
| `<ID>` | `ID` | Node ID (u32, supports `0x` hex prefix) | *required* |
| `--token` | `TOKEN` | Shared secret for WebSocket authentication | — |
| `--listen` | `LISTEN` | TCP address for the WebSocket listener | `0.0.0.0:2333` |
| `--servers` | `SERVERS` | Comma-separated WebSocket URLs to connect to | — |
| `--ice-servers` | `ICE_SERVERS` | Comma-separated ICE servers (`url` or `url\|user\|pass`) | — |
| `--candidate-filter-prefix` | `CANDIDATE_FILTER_PREFIX` | CIDR prefix to exclude from ICE candidates | — |
| `--interface-prefix` | `INTERFACE_PREFIX` | TUN interface name prefix | `cn` |
| `--enable-packet-information` | `ENABLE_PACKET_INFORMATION` | Enable packet info on TUN devices | `false` |
| `--ctl-path` | `CTL_PATH` | Unix socket path for UAPI | `cryonet.ctl` |

### CLI Tool (`cryoc`)

Connects to the daemon's Unix socket.

| Subcommand | Alias | Description |
|---|---|---|
| `links` | `l` | List all direct WebSocket links |
| `routes` | `r` | List mesh routes |
| `igp-routes` | `ir` | Show detailed IGP route table |
| `full-mesh-peers` | `fmp` | Show all WebRTC peer connections per node |
| `ping <NODE_ID>` | — | Ping a remote node through the mesh (measures RTT) |
