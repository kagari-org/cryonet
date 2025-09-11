package cryonet

import (
	"context"
	"errors"
	"io"
	"net"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/channel"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type PeerWS struct {
	peerId string
	ws     *websocket.Conn

	context context.Context
	cancel  func()
}

func SpawnWSPeer(parent *goakt.PID, peerId string, ws *websocket.Conn) (*goakt.PID, error) {
	ctx, cancel := context.WithCancel(context.Background())
	return parent.SpawnChild(
		context.Background(),
		"peer-ws-"+peerId,
		&PeerWS{
			peerId:  peerId,
			ws:      ws,
			context: ctx,
			cancel:  cancel,
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*PeerWS)(nil)

func (p *PeerWS) PreStart(ctx *goakt.Context) error { return nil }

func (p *PeerWS) PostStop(ctx *goakt.Context) error {
	if p.ws != nil {
		p.ws.CloseNow()
	}
	p.cancel()
	return nil
}

func (p *PeerWS) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		go p.read(ctx.Self())
	case *peer.IRecvPacket:
		packet := &channel.Packet{}
		err := proto.Unmarshal(msg.Packet, packet)
		if err != nil {
			ctx.Err(err)
			return
		}
		_, rtr, err := ctx.ActorSystem().ActorOf(ctx.Context(), "router")
		if err != nil {
			ctx.Err(err)
			return
		}
		ctx.Tell(rtr, &router.ORecvPacket{
			Packet: packet,
		})
	case *peer.OStop:
		ctx.Err(errors.New("stop peer " + p.peerId))
	case *peer.OSendPacket:
		context, cancel := context.WithTimeout(ctx.Context(), Config.PeerTimeout)
		defer cancel()
		err := p.ws.Write(context, websocket.MessageBinary, msg.Packet)
		if err != nil {
			ctx.Err(err)
			return
		}
	default:
		ctx.Unhandled()
	}
}

func (p *PeerWS) read(self *goakt.PID) {
	for {
		if !self.IsRunning() {
			break
		}
		_, data, err := p.ws.Read(p.context)
		if errors.Is(err, net.ErrClosed) || errors.Is(err, io.EOF) {
			self.Logger().Error(err)
			err := self.Tell(context.Background(), self, &peer.OStop{})
			if err != nil {
				self.Logger().Error(err)
			}
			break
		}
		if err != nil {
			self.Logger().Error(err)
			continue
		}
		err = self.Tell(
			context.Background(),
			self,
			&peer.IRecvPacket{
				Packet: data,
			},
		)
		if err != nil {
			self.Logger().Error(err)
		}
	}
}
