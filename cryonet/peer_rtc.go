package cryonet

import (
	"os"

	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/rtc"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type RTCPeer struct {
	id   string
	peer *webrtc.PeerConnection
	dc   *webrtc.DataChannel
	tun  *os.File
}

func NewRTCPeer(id string, peer *webrtc.PeerConnection, dc *webrtc.DataChannel) *RTCPeer {
	return &RTCPeer{
		id:   id,
		peer: peer,
		dc:   dc,
	}
}

var _ goakt.Actor = (*RTCPeer)(nil)

func (r *RTCPeer) PreStart(ctx *goakt.Context) error {
	tun, err := CreateTun(Config.InterfacePrefixRTC + r.id)
	if err != nil {
		r.close()
		return err
	}
	r.tun = tun
	return nil
}

func (r *RTCPeer) PostStop(ctx *goakt.Context) error {
	r.close()
	return nil
}

func (r *RTCPeer) Receive(ctx *goakt.ReceiveContext) {
	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		r.rtcRead(ctx.Self())
		go r.tunRead(ctx.Self())
	default:
		ctx.Unhandled()
	}
}

func (r *RTCPeer) close() {
	if r.dc != nil {
		r.dc.Close()
	}
	if r.peer != nil {
		r.peer.Close()
	}
}

func (r *RTCPeer) rtcRead(pid *goakt.PID) {
	r.dc.OnMessage(func(msg webrtc.DataChannelMessage) {
		if !pid.IsRunning() {
			return
		}
		packet := &rtc.Packet{}
		err := proto.Unmarshal(msg.Data, packet)
		if err != nil {
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
				return
			}
		default:
			panic("unreachable")
		}
	})
}

func (r *RTCPeer) tunRead(pid *goakt.PID) {
	data := make([]byte, Config.BufSize)
	for {
		if !pid.IsRunning() {
			break
		}
		n, err := r.tun.Read(data)
		if err != nil {
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
			continue
		}
		err = r.dc.Send(data)
		if err != nil {
			continue
		}
	}
}
