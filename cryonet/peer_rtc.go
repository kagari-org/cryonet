package cryonet

import (
	"errors"
	"os"

	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/rtc"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type PeerRTC struct {
	id   string
	peer *webrtc.PeerConnection
	dc   *webrtc.DataChannel
	tun  *os.File
}

func NewRTCPeer(id string, peer *webrtc.PeerConnection, dc *webrtc.DataChannel) *PeerRTC {
	return &PeerRTC{
		id:   id,
		peer: peer,
		dc:   dc,
	}
}

var _ goakt.Actor = (*PeerRTC)(nil)

func (r *PeerRTC) PreStart(ctx *goakt.Context) error {
	tun, err := CreateTun(ctx, Config.InterfacePrefixRTC+r.id)
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
	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		r.rtcRead(ctx)
		go r.tunRead(ctx)
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
}

func (r *PeerRTC) rtcRead(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	self := ctx.Self()
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
			// TODO
		case *common.Packet_Desc:
			// TODO
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
	r.dc.OnClose(func() {
		logger.Debug("data channel closed")
		ctx.Stop(self)
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
