package controller

import (
	"fmt"
	"sync"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/cryonet"
	"github.com/kagari-org/cryonet/cryonet/peer"
	"github.com/kagari-org/cryonet/gen/actors/controller/ws_connect"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
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
	for _, server := range cryonet.Config.WSServers {
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
		err := ctx.ActorSystem().Schedule(ctx.Context(), &ws_connect.Connect{}, ctx.Self(), cryonet.Config.CheckInterval)
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

	// send Init
	sendInit := &ws.Packet{
		P: &ws.Packet_Init{
			Init: &ws.Init{
				Id:    cryonet.Config.Id,
				Token: cryonet.Config.Token,
			},
		},
	}
	data, err := proto.Marshal(sendInit)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to marshal init packet")
		return err
	}
	err = conn.Write(ctx.Context(), websocket.MessageBinary, data)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to send init packet")
		return err
	}

	// recv Init
	_, data, err = conn.Read(ctx.Context())
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to read init packet")
		return err
	}
	recvInit := &ws.Packet{}
	err = proto.Unmarshal(data, recvInit)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to unmarshal init packet")
		return err
	}
	if recvInit.GetInit() == nil {
		conn.Close(websocket.StatusProtocolError, "received invalid init packet")
		return err
	}
	if recvInit.GetInit().GetId() == cryonet.Config.Id {
		conn.Close(websocket.StatusProtocolError, "received init packet with same ID")
		return err
	}
	if recvInit.GetInit().GetToken() != cryonet.Config.Token {
		conn.Close(websocket.StatusProtocolError, "received init packet with invalid token")
		return err
	}

	id := recvInit.GetInit().GetId()
	ws := peer.NewWSPeer(id, conn)
	p.pid, err = ctx.ActorSystem().Spawn(ctx.Context(), fmt.Sprintf("ws-connect-%s", id), ws)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to spawn ws peer")
		return err
	}

	return nil
}
