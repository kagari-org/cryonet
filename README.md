# Cryonet

[简体中文](README_zh.md)

Cryonet is a decentralized networking daemon.

## Overview

Nodes connect to each other via WebSocket. When the nodes form a connected graph, Cryonet automatically builds a forwarding network so that any two nodes can communicate with each other — this serves as the control plane. Using the control plane, a full mesh of WebRTC ICE connections is automatically established between a node and all other nodes. A TUN interface is created for each node, forming the data plane. Cryonet attempts to establish n(n-1)/2 connections in order to utilize all available links in the network. If a TURN server is configured and NAT hole-punching fails, nodes can still relay traffic through the TURN server.

Cryonet expects users to run a routing protocol (such as OSPF, Babel, etc.) on top of the interfaces, delegating the path selection to the routing protocol.

## Encryption

Cryonet exchanges ECDH public keys via the control plane and encrypts data using AES-GCM. A counter is used to generate nonces, and keys are periodically re-negotiated. By default, Cryonet does not encrypt intra-network traffic; this behavior can be changed through an option.

## TAP mode

By default, Cryonet creates a TUN device for each node. In TAP mode, Cryonet implements a pseudo-Layer 2 and creates only a single TAP device. Nodes broadcast the IP addresses on their network interfaces through the control plane and proxy ARP/NDP requests on behalf of other nodes. Cryonet maintains a mapping between node IDs and MAC addresses, so the destination node ID can be derived directly from a MAC address, allowing the packet to be forwarded straight to the target node. If a packet entering the TAP device has a source MAC address that is not the one expected by Cryonet, the packet will be dropped; therefore, you cannot bridge this Layer 2 interface to other network interfaces. Broadcast and multicast packets are forwarded to all nodes.

In TAP mode, you can choose not to run a routing protocol on the interface. Simply add addresses from the same subnet to the interface to enable direct communication.

Packets in TAP mode are identical to those in TUN mode, so nodes in the network can mix TUN mode and TAP mode.

## wasm

Cryonet can run in the browser. In the browser, Cryonet negotiates with other nodes to communicate using WebRTC DataChannels.

> ### Why?
> Because it's fun :)

## MPLS

When packet information is enabled, Cryonet supports forwarding MPLS packets. With packet information enabled, the TUN device can correctly handle MPLS packets (though packet capture tools may not parse them correctly).

## Usage

Use the `--help` option to see usage instructions. You can also use the `cryoc` command-line tool to communicate with the Cryonet daemon and query its running status.
