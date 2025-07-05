package cryonet

import (
	"errors"
	"net"
	"os"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type WSPeer struct {
	id  string
	ws  *websocket.Conn
	tun *os.File
}

func NewWSPeer(id string, ws *websocket.Conn) *WSPeer {
	return &WSPeer{
		id: id,
		ws: ws,
	}
}

var _ goakt.Actor = (*WSPeer)(nil)

func (w *WSPeer) PreStart(ctx *goakt.Context) error {
	tun, err := CreateTun(ctx, Config.InterfacePrefixWS+w.id)
	if err != nil {
		w.close()
		ctx.ActorSystem().Logger().Error(err)
		return err
	}
	w.tun = tun
	return nil
}

func (w *WSPeer) PostStop(ctx *goakt.Context) error {
	w.close()
	return nil
}

func (w *WSPeer) Receive(ctx *goakt.ReceiveContext) {
	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		go w.wsRead(ctx)
		go w.tunRead(ctx)
	default:
		ctx.Unhandled()
	}
}

func (w *WSPeer) close() {
	if w.ws != nil {
		w.ws.Close(websocket.StatusNormalClosure, "stop")
	}
	if w.tun != nil {
		w.tun.Close()
	}
}

func (w *WSPeer) wsRead(ctx *goakt.ReceiveContext) {
	self := ctx.Self()
	for {
		if !self.IsRunning() {
			break
		}
		_, data, err := w.ws.Read(ctx.Context())
		if errors.Is(err, net.ErrClosed) {
			ctx.Stop(self)
			ctx.Logger().Error(err)
			break
		}
		if err != nil {
			ctx.Logger().Error(err)
			continue
		}
		packet := &ws.Packet{}
		err = proto.Unmarshal(data, packet)
		if err != nil {
			ctx.Logger().Error(err)
			continue
		}
		switch packet := packet.P.(type) {
		case *ws.Packet_Init:
			panic("unreachable")
		case *ws.Packet_Packet:
			switch packet := packet.Packet.Packet.(type) {
			case *common.Packet_Alive:
				// TODO
			case *common.Packet_Desc:
				// TODO
			case *common.Packet_Data:
				data := packet.Data.GetData()
				_, err := w.tun.Write(data)
				if err != nil {
					ctx.Logger().Error(err)
					continue
				}
			default:
				panic("unreachable")
			}
		default:
			panic("unreachable")
		}
	}
}

func (w *WSPeer) tunRead(ctx *goakt.ReceiveContext) {
	self := ctx.Self()
	data := make([]byte, Config.BufSize)
	for {
		if !self.IsRunning() {
			break
		}
		n, err := w.tun.Read(data)
		if err != nil {
			ctx.Logger().Error(err)
			continue
		}
		packet := &ws.Packet{
			P: &ws.Packet_Packet{
				Packet: &common.Packet{
					Packet: &common.Packet_Data{
						Data: &common.Data{
							Data: data[:n],
						},
					},
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			ctx.Logger().Error(err)
			continue
		}
		err = w.ws.Write(ctx.Context(), websocket.MessageBinary, data)
		if err != nil {
			ctx.Logger().Error(err)
			continue
		}
	}
}
