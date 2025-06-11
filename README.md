# cryonet

Decentralized full mesh tunneling tool.

Cryonet creates full mesh L3 interfaces between your nodes. For example, assume you have n nodes, then there will be n - 1 interfaces created by cryonet (with prefix `cn` by default) on each node. Each interface is a peer-to-peer interface.

You can run a routing protocol on these interfaces like OSPF/Babel to create a mesh network with forwarding to get the best latency and connectivity.

Cryonet uses WebRTC to establish connections between nodes to traverse NAT via hole punching and relaying. The purpose of this is to find every possible links between nodes. It doesn't care about how packets forward.

Different from Tailscale or Zerotier, there are no controller servers in cryonet. WebRTC requires a signaling server to exchange connection information between nodes, but cryonet uses broadcast for signaling. Nodes connect to some bootstrap nodes first and receive information about other nodes, then handshake (send WebRTC Offers and Answers) with each other by broadcast.

WebRTC traffic is encrypted by default. Cryonet uses WebSocket for bootstrapping. You may proxy WebSocket traffic through a reverse proxy to get all traffic encrypted.

## Usage

```
Usage: cryonet [OPTIONS] --token <TOKEN> <ID>

Arguments:
  <ID>  

Options:
      --listen <LISTEN>                            [env: LISTEN=] [default: 127.0.0.1:2333]
      --ws-servers <WS_SERVERS>                    [env: WS_SERVERS=]
      --token <TOKEN>                              [env: TOKEN=]
      --ice-servers <ICE_SERVERS>                  [env: ICE_SERVERS=]
      --check-interval <CHECK_INTERVAL>            [env: CHECK_INTERVAL=] [default: 10s]
      --check-timeout <CHECK_TIMEOUT>              [env: CHECK_TIMEOUT=] [default: 1m]
      --send-alive-interval <SEND_ALIVE_INTERVAL>  [env: SEND_ALIVE_INTERVAL=] [default: 20s]
      --interface-prefix <INTERFACE_PREFIX>        [env: INTERFACE_PREFIX=] [default: cn]
      --auto-interface-name                        [env: AUTO_INTERFACE_NAME=]
      --enable-packet-information                  [env: ENABLE_PACKET_INFORMATION=]
      --buf-size <BUF_SIZE>                        [env: BUF_SIZE=] [default: 1504]
  -h, --help                                       Print help
```

`--listen`: The port which cryonet listens for bootstrapping. Other nodes may connect this node to bootstrap. You should proxy this port with tls.

`--ws-servers`: Bootstrap nodes to connect to. Cryonet will connect to these nodes first to join the network. Example: `--ws-servers wss://node1,wss://node2,wss://node3`.

`--token`: A token to authenticate the node. Should be the same on all nodes.

`--ice-servers`: ICE servers to use for WebRTC connections. Example: `--ice-servers stun:stun.l.google.com,turn:turn.example.com|username|credential`.

`--interface-prefix`: The prefix of the interfaces created by cryonet. Interfaces are named `<INTERFACE_PREFIX><ID>` where `<ID>` is the node ID.

`--auto-interface-name`: If set, interface names are not set by cryonet but by system like `tun0`, `tun1`, etc.

# Deployment

When firewall is enabled, you should allow ports that WebRTC uses in the INPUT chain. For example:

```nftables
set cryonet-ports {
  type inet_service;
  flags dynamic;
  timeout 1d;
}
chain cryonet-output {
  type route hook output priority filter; policy accept;
  socket cgroupv2 level 1 "cryonet.slice" add @cryonet-ports { udp sport }
}
chain input {
  type filter hook input priority filter; policy drop;
  ... # your other rules
  udp dport @cryonet-ports accept
}
```

This adds udp sports to `cryonet-ports` set, which is used in the `input` chain to allow incoming WebRTC traffic. This uses cgroupv2 infomation to filter traffic from cryonet. You may use other methods to filter traffic.
