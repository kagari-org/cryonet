package cryonet

import (
	"sync"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/actors/controller/ws_connect"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type WSConnect struct {
	peers map[string]*Peer
}

type Peer struct {
	server string
	lock   sync.Mutex
	pid    *goakt.PID
}

var _ goakt.Actor = (*WSConnect)(nil)

func NewWSConnect() *WSConnect {
	return &WSConnect{}
}

func (w *WSConnect) PreStart(ctx *goakt.Context) error {
	w.peers = make(map[string]*Peer)
	for _, server := range Config.WSServers {
		w.peers[server] = &Peer{
			server: server,
			lock:   sync.Mutex{},
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
		err := ctx.ActorSystem().Schedule(ctx.Context(), &ws_connect.Connect{}, ctx.Self(), Config.CheckInterval)
		if err != nil {
			// TODO: log error
		}
	case *ws_connect.Connect:
		for _, peer := range w.peers {
			go w.connect(ctx, peer)
		}
	default:
		ctx.Unhandled()
	}
}

func (w *WSConnect) connect(ctx *goakt.ReceiveContext, p *Peer) error {
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
		return err
	}

	pid, err := WSShakeOrClose(ctx, conn)
	if err != nil {
		return err
	}

	p.pid = pid

	return nil
}
