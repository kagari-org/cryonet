package cryonet

import (
	"errors"
	"os"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller_rtc"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/rtc"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/anypb"
)

type PeerRTC struct {
	peerId string
	peer   *webrtc.PeerConnection
	dc     *webrtc.DataChannel
	tun    *os.File
}

func NewRTCPeer(peerId string, peer *webrtc.PeerConnection, dc *webrtc.DataChannel) *PeerRTC {
	return &PeerRTC{
		peerId: peerId,
		peer:   peer,
		dc:     dc,
	}
}

var _ goakt.Actor = (*PeerRTC)(nil)

func (r *PeerRTC) PreStart(ctx *goakt.Context) error {
	tun, err := CreateTun(ctx, Config.InterfacePrefixRTC+r.peerId)
	if err != nil {
		r.close()
		ctx.ActorSystem().Logger().Error(err)
		return err
	}
	r.tun = tun
	return nil
}

func (r *PeerRTC) PostStop(ctx *goakt.Context) error {
	r.close()
	return nil
}

func (r *PeerRTC) Receive(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	self := ctx.Self()
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		r.dc.OnClose(func() {
			logger.Debug("data channel closed")
			ctx.Stop(self)
		})
		r.peer.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
			if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
				logger.Error("peer connection state: ", state)
				ctx.Stop(self)
			}
		})
		r.rtcRead(ctx)
		go r.tunRead(ctx)
		ctx.Tell(ctx.ActorSystem().TopicActor(), &goaktpb.Subscribe{Topic: "peers"})
	case *peer.CastAlive:
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
		// TODO: this might not thread safe
		err = r.dc.Send(data)
		if err != nil {
			ctx.Err(err)
			return
		}
	case *peer.CastDesc:
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
		err = r.dc.Send(data)
		if err != nil {
			ctx.Err(err)
			return
		}
	case *peer.SendDesc:
		if msg.GetDesc().GetTo() != r.peerId {
			return
		}
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
		err = r.dc.Send(data)
		if err != nil {
			ctx.Err(err)
			return
		}
	default:
		ctx.Unhandled()
	}
}

func (r *PeerRTC) close() {
	if r.dc != nil {
		r.dc.Close()
	}
	if r.peer != nil {
		r.peer.Close()
	}
	r.tun.Close()
}

func (r *PeerRTC) rtcRead(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	self := ctx.Self()
	// get controller_rtc
	_, rtcCtrl, err := ctx.ActorSystem().ActorOf(ctx.Context(), "rtc")
	if err != nil {
		panic(err)
	}
	r.dc.OnMessage(func(msg webrtc.DataChannelMessage) {
		if !self.IsRunning() {
			return
		}
		packet := &rtc.Packet{}
		err := proto.Unmarshal(msg.Data, packet)
		if err != nil {
			logger.Error(err)
			return
		}
		switch packet := packet.Packet.Packet.(type) {
		case *common.Packet_Alive:
			logger.Info("Received alive packet: ", packet.Alive)
			ctx.Tell(rtcCtrl, &controller_rtc.Alive{Alive: packet.Alive})
		case *common.Packet_Desc:
			ctx.Tell(rtcCtrl, &controller_rtc.Desc{Desc: packet.Desc})
			desc, err := anypb.New(&peer.SendDesc{
				Desc: packet.Desc,
			})
			if err != nil {
				logger.Error(err)
				return
			}
			ctx.Tell(ctx.ActorSystem().TopicActor(), &goaktpb.Publish{
				Id:      uuid.NewString(),
				Topic:   "peers",
				Message: desc,
			})
		case *common.Packet_Data:
			_, err := r.tun.Write(packet.Data.GetData())
			if err != nil {
				logger.Error(err)
				return
			}
		default:
			panic("unreachable")
		}
	})
}

func (r *PeerRTC) tunRead(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	self := ctx.Self()
	data := make([]byte, Config.BufSize)
	for {
		if !self.IsRunning() {
			break
		}
		n, err := r.tun.Read(data)
		if errors.Is(err, os.ErrClosed) || errors.Is(err, os.ErrInvalid) {
			logger.Error(err)
			ctx.Stop(self)
			break
		}
		if err != nil {
			logger.Error(err)
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
			logger.Error(err)
			continue
		}
		err = r.dc.Send(data)
		if err != nil {
			logger.Error(err)
			continue
		}
	}
}
