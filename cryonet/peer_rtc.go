package cryonet

import (
	"context"
	"errors"
	"os"
	"time"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/rtc"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type PeerRTC struct {
	peerId string
	dc     *webrtc.DataChannel
	tun    *os.File

	last            time.Time
	checkScheduleId string
}

func SpawnRTCPeer(parent *goakt.PID, peerId string, dc *webrtc.DataChannel) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"peer-rtc-"+peerId,
		&PeerRTC{
			peerId:          peerId,
			dc:              dc,
			last:            time.Now(),
			checkScheduleId: uuid.NewString(),
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
	ctx.ActorSystem().CancelSchedule(p.checkScheduleId)
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

		ctx.ActorSystem().Schedule(
			ctx.Context(),
			&peer.ICheck{},
			ctx.Self(),
			Config.CheckInterval,
			goakt.WithReference(p.checkScheduleId),
		)
	case *peer.IAlive:
		p.last = time.Now()
	case *peer.ICheck:
		if time.Since(p.last) > Config.PeerTimeout {
			ctx.Err(errors.New("peer timeout"))
		}
	case *peer.IStop:
		// stop by parent
		ctx.Err(errors.New("stop peer"))
	case *peer.OGetPeerId:
		ctx.Response(&peer.OGetPeerIdResponse{
			PeerId: p.peerId,
		})
	case *peer.OAlive:
		packet := &rtc.Packet{
			Packet: &common.Packet{
				Packet: &common.Packet_Alive{
					Alive: msg.GetAlive(),
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			ctx.Err(err)
			return
		}

		self := ctx.Self()
		go func() {
			err = p.dc.Send(data)
			if err != nil {
				self.Logger().Error(err)
			}
		}()
	case *peer.ODesc:
		packet := &rtc.Packet{
			Packet: &common.Packet{
				Packet: &common.Packet_Desc{
					Desc: msg.GetDesc(),
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			ctx.Err(err)
			return
		}
		self := ctx.Self()
		go func() {
			err = p.dc.Send(data)
			if err != nil {
				self.Logger().Error(err)
			}
		}()
	default:
		ctx.Unhandled()
	}
}

func (p *PeerRTC) rtcRead(self *goakt.PID) {
	p.dc.OnMessage(func(msg webrtc.DataChannelMessage) {
		if !self.IsRunning() {
			return
		}
		packet := &rtc.Packet{}
		err := proto.Unmarshal(msg.Data, packet)
		if err != nil {
			self.Logger().Error(err)
			return
		}
		switch packet := packet.Packet.Packet.(type) {
		case *common.Packet_Alive:
			err := self.Tell(
				context.Background(),
				self,
				&peer.IAlive{},
			)
			if err != nil {
				self.Logger().Error(err)
			}
			err = self.Tell(
				context.Background(),
				self.Parent().Parent(),
				&controller.OAlive{Alive: packet.Alive},
			)
			if err != nil {
				self.Logger().Error(err)
			}
		case *common.Packet_Desc:
			err := self.Tell(
				context.Background(),
				self.Parent().Parent(),
				&controller.OForwardDesc{Desc: packet.Desc},
			)
			if err != nil {
				self.Logger().Error(err)
			}
		case *common.Packet_Data:
			_, err := p.tun.Write(packet.Data.GetData())
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
		packet := &rtc.Packet{
			Packet: &common.Packet{
				Packet: &common.Packet_Data{
					Data: &common.Data{
						Data: data[:n],
					},
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
