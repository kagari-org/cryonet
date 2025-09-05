package cryonet

import (
	"context"
	"errors"
	"os"

	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/channel"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type PeerRTC struct {
	peerId string
	dc     *webrtc.DataChannel
	tun    *os.File
}

func SpawnRTCPeer(parent *goakt.PID, peerId string, dc *webrtc.DataChannel) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"peer-rtc-"+peerId,
		&PeerRTC{
			peerId: peerId,
			dc:     dc,
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*PeerRTC)(nil)

func (p *PeerRTC) PreStart(ctx *goakt.Context) error { return nil }

func (p *PeerRTC) PostStop(ctx *goakt.Context) error {
	if p.dc != nil {
		p.dc.Close()
	}
	if p.tun != nil {
		p.tun.Close()
	}
	return nil
}

func (p *PeerRTC) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		self := ctx.Self()

		tun, err := CreateTun(Config.InterfacePrefixRTC + p.peerId)
		if err != nil {
			ctx.Err(err)
			return
		}
		p.tun = tun

		p.dc.OnClose(func() {
			self.Logger().Debug("data channel closed")
			err := self.Tell(context.Background(), self, &peer.IStop{})
			if err != nil {
				self.Logger().Error(err)
			}
		})

		p.rtcRead(ctx.Self())
		go p.tunRead(ctx.Self())
	case *peer.IStop:
		// stop by parent
		ctx.Err(errors.New("stop peer"))
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
				Normal: msg.Packet,
			},
		})
		if err != nil {
			ctx.Err(err)
			return
		}
		err = p.dc.Send(data)
		if err != nil {
			ctx.Err(err)
			return
		}
	default:
		ctx.Unhandled()
	}
}

func (p *PeerRTC) rtcRead(self *goakt.PID) {
	p.dc.OnMessage(func(msg webrtc.DataChannelMessage) {
		if !self.IsRunning() {
			return
		}
		packet := &channel.Packet{}
		err := proto.Unmarshal(msg.Data, packet)
		if err != nil {
			self.Logger().Error(err)
			return
		}
		switch packet := packet.Packet.(type) {
		case *channel.Packet_Direct:
			_, err := p.tun.Write(packet.Direct.Data)
			if err != nil {
				self.Logger().Error(err)
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
	})
}

func (p *PeerRTC) tunRead(self *goakt.PID) {
	data := make([]byte, Config.BufSize)
	for {
		if !self.IsRunning() {
			break
		}
		n, err := p.tun.Read(data)
		if errors.Is(err, os.ErrClosed) || errors.Is(err, os.ErrInvalid) {
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
		err = p.dc.Send(data)
		if err != nil {
			self.Logger().Error(err)
			continue
		}
	}
}
