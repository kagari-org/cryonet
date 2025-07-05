package controller

import (
	"net"
	"net/http"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/cryonet"
	"github.com/kagari-org/cryonet/cryonet/utils"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type WSListen struct {
	listener     net.Listener
	server       *http.Server
	postStartCtx *goakt.ReceiveContext
}

var _ goakt.Actor = (*WSListen)(nil)

func NewWSListen() *WSListen {
	return &WSListen{}
}

func (w *WSListen) PreStart(ctx *goakt.Context) error {
	listener, err := net.Listen("tcp", cryonet.Config.Listen)
	if err != nil {
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
	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		go func() {
			w.postStartCtx = ctx
			err := w.server.Serve(w.listener)
			if err != nil {
				// TODO: log err
			}
		}()
	default:
		ctx.Unhandled()
	}
}

func (w *WSListen) ServeHTTP(writer http.ResponseWriter, request *http.Request) {
	conn, err := websocket.Accept(writer, request, nil)
	if err != nil {
		return
	}

	_, err = utils.WSShakeOrClose(w.postStartCtx, conn)
	if err != nil {
		return
	}
}
