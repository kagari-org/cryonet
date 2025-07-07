package cryonet

import (
	"sync"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/controller_ws_connect"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type WSConnect struct {
	peers map[string]*WSConnectPeer
}

type WSConnectPeer struct {
	lock   sync.Mutex
	server string
	peerId string
	pid    *goakt.PID
}

var _ goakt.Actor = (*WSConnect)(nil)

func NewWSConnect() *WSConnect {
	return &WSConnect{}
}

func (w *WSConnect) PreStart(ctx *goakt.Context) error {
	w.peers = make(map[string]*WSConnectPeer)
	for _, server := range Config.WSServers {
		w.peers[server] = &WSConnectPeer{
			lock:   sync.Mutex{},
			server: server,
			peerId: "",
			pid:    nil,
		}
	}
	return nil
}

func (w *WSConnect) PostStop(ctx *goakt.Context) error {
	return nil
}

func (w *WSConnect) Receive(ctx *goakt.ReceiveContext) {
	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		ctx.Tell(ctx.Self(), &controller_ws_connect.Connect{})
		err := ctx.ActorSystem().Schedule(ctx.Context(), &controller_ws_connect.Connect{}, ctx.Self(), Config.CheckInterval)
		if err != nil {
			panic(err)
		}
	case *controller_ws_connect.Connect:
		for _, peer := range w.peers {
			go w.connect(ctx, peer)
		}
	case *controller.GetPeers:
		peers := make([]string, 0)
		for _, peer := range w.peers {
			if peer.pid != nil && peer.pid.IsRunning() {
				peers = append(peers, peer.peerId)
			}
		}
		ctx.Response(&controller.GetPeersResponse{
			Peers: peers,
		})
	default:
		ctx.Unhandled()
	}
}

func (w *WSConnect) connect(ctx *goakt.ReceiveContext, p *WSConnectPeer) error {
	logger := ctx.Logger()

	if !p.lock.TryLock() {
		return nil
	}
	defer p.lock.Unlock()

	if p.pid != nil && p.pid.IsRunning() {
		// already connected
		return nil
	}

	conn, _, err := websocket.Dial(ctx.Context(), p.server, nil)
	if err != nil {
		logger.Error(err)
		return err
	}

	conn.SetReadLimit(-1)

	peerId, conn, err := WSShakeOrClose(ctx, conn)
	if err != nil {
		logger.Error(err)
		return err
	}
	pid := ctx.Spawn("ws-peer-"+peerId, NewWSPeer(peerId, conn), goakt.WithLongLived())

	p.peerId = peerId
	p.pid = pid

	return nil
}
