package cryonet

import (
	"context"
	"errors"
	"io"
	"net"
	"os"

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
	tun    *os.File

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
	if p.tun != nil {
		p.tun.Close()
	}
	p.cancel()
	return nil
}

func (p *PeerWS) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		tun, err := CreateTun(Config.InterfacePrefixWS + p.peerId)
		if err != nil {
			ctx.Err(err)
			return
		}
		p.tun = tun

		go p.wsRead(ctx.Self())
		go p.tunRead(ctx.Self())
	case *peer.IStop:
		// stop by parent
		ctx.Err(errors.New("stop peer " + p.peerId))
	case *peer.IRecvPacket:
		_, rtr, err := ctx.ActorSystem().ActorOf(ctx.Context(), "router")
		if err != nil {
			ctx.Err(err)
			return
		}
		ctx.Tell(rtr, &router.ORecvPacket{
			Packet: msg.Packet,
		})
	case *peer.OSendPacket:
		data, err := proto.Marshal(&channel.Packet{
			Packet: &channel.Packet_Normal{
				Normal: msg.GetPacket(),
			},
		})
		if err != nil {
			ctx.Err(err)
			return
		}
		context, cancel := context.WithTimeout(ctx.Context(), Config.PeerTimeout)
		defer cancel()
		err = p.ws.Write(context, websocket.MessageBinary, data)
		if err != nil {
			ctx.Err(err)
			return
		}
	default:
		ctx.Unhandled()
	}
}

func (p *PeerWS) wsRead(self *goakt.PID) {
	for {
		if !self.IsRunning() {
			break
		}
		_, data, err := p.ws.Read(p.context)
		if errors.Is(err, net.ErrClosed) || errors.Is(err, io.EOF) {
			self.Logger().Error(err)
			err := self.Tell(context.Background(), self, &peer.IStop{})
			if err != nil {
				self.Logger().Error(err)
			}
			break
		}
		if err != nil {
			self.Logger().Error(err)
			continue
		}
		packet := &channel.Packet{}
		err = proto.Unmarshal(data, packet)
		if err != nil {
			self.Logger().Error(err)
			continue
		}
		switch packet := packet.GetPacket().(type) {
		case *channel.Packet_Direct:
			_, err := p.tun.Write(packet.Direct.GetData())
			if err != nil {
				self.Logger().Error(err)
				continue
			}
		case *channel.Packet_Normal:
			err := self.Tell(
				context.Background(),
				self,
				&peer.IRecvPacket{
					Packet: packet.Normal,
				},
			)
			if err != nil {
				self.Logger().Error(err)
			}
		default:
			panic("unreachable")
		}
	}
}

func (p *PeerWS) tunRead(self *goakt.PID) {
	data := make([]byte, Config.BufSize)
	for {
		if !self.IsRunning() {
			break
		}
		n, err := p.tun.Read(data)
		if errors.Is(err, os.ErrClosed) || errors.Is(err, io.EOF) {
			self.Logger().Error(err)
			err := self.Tell(context.Background(), self, &peer.IStop{})
			if err != nil {
				self.Logger().Error(err)
			}
			break
		}
		if err != nil {
			self.Logger().Error(err)
			continue
		}
		packet := &channel.Packet{
			Packet: &channel.Packet_Direct{
				Direct: &channel.Direct{
					Data: data[:n],
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			self.Logger().Error(err)
			continue
		}
		err = p.ws.Write(p.context, websocket.MessageBinary, data)
		if err != nil {
			self.Logger().Error(err)
			continue
		}
	}
}
