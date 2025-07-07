package cryonet

import (
	"net"
	"net/http"
	"sync"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type WSListen struct {
	listener     net.Listener
	server       *http.Server
	postStartCtx *goakt.ReceiveContext

	peersLock sync.Mutex
	peers     map[string]*WSListenPeer
}

type WSListenPeer struct {
	peerId string
	pid    *goakt.PID
}

var _ goakt.Actor = (*WSListen)(nil)

func NewWSListen() *WSListen {
	return &WSListen{
		peersLock: sync.Mutex{},
		peers:     make(map[string]*WSListenPeer),
	}
}

func (w *WSListen) PreStart(ctx *goakt.Context) error {
	listener, err := net.Listen("tcp", Config.Listen)
	if err != nil {
		ctx.ActorSystem().Logger().Error(err)
		return err
	}
	server := &http.Server{
		Handler: w,
	}
	w.listener = listener
	w.server = server
	return nil
}

func (w *WSListen) PostStop(ctx *goakt.Context) error {
	w.server.Close()
	w.listener.Close()
	return nil
}

func (w *WSListen) Receive(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		w.postStartCtx = ctx
		go func() {
			err := w.server.Serve(w.listener)
			if err != nil {
				logger.Error(err)
				return
			}
		}()
	case *controller.GetPeers:
		w.peersLock.Lock()
		defer w.peersLock.Unlock()
		peers := make([]string, 0)
		for peerId, peer := range w.peers {
			if peer.pid != nil && peer.pid.IsRunning() {
				peers = append(peers, peerId)
			}
		}
		ctx.Response(&controller.GetPeersResponse{
			Peers: peers,
		})
	default:
		ctx.Unhandled()
	}
}

func (w *WSListen) ServeHTTP(writer http.ResponseWriter, request *http.Request) {
	conn, err := websocket.Accept(writer, request, nil)
	if err != nil {
		return
	}

	conn.SetReadLimit(-1)

	w.peersLock.Lock()
	defer w.peersLock.Unlock()

	// TODO: check peers with same id
	peerId, conn, err := WSShakeOrClose(w.postStartCtx, conn)
	if err != nil {
		return
	}
	if w.peers[peerId] != nil && w.peers[peerId].pid != nil && w.peers[peerId].pid.IsRunning() {
		conn.Close(websocket.StatusProtocolError, "peer with same id already exists")
		return
	}
	pid := w.postStartCtx.Spawn("ws-peer-"+peerId, NewWSPeer(peerId, conn), goakt.WithLongLived())
	w.peers[peerId] = &WSListenPeer{
		peerId: peerId,
		pid:    pid,
	}
}
