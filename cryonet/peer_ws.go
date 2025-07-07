package cryonet

import (
	"errors"
	"io"
	"net"
	"os"

	"github.com/coder/websocket"
	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller_rtc"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/anypb"
)

type PeerWS struct {
	id  string
	ws  *websocket.Conn
	tun *os.File
}

func NewWSPeer(id string, ws *websocket.Conn) *PeerWS {
	return &PeerWS{
		id: id,
		ws: ws,
	}
}

var _ goakt.Actor = (*PeerWS)(nil)

func (w *PeerWS) PreStart(ctx *goakt.Context) error {
	tun, err := CreateTun(ctx, Config.InterfacePrefixWS+w.id)
	if err != nil {
		w.close()
		ctx.ActorSystem().Logger().Error(err)
		return err
	}
	w.tun = tun
	return nil
}

func (w *PeerWS) PostStop(ctx *goakt.Context) error {
	w.close()
	return nil
}

func (w *PeerWS) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		go w.wsRead(ctx)
		go w.tunRead(ctx)
		ctx.Tell(ctx.ActorSystem().TopicActor(), &goaktpb.Subscribe{Topic: "peers"})
	case *peer.CastAlive:
		packet := &ws.Packet{
			P: &ws.Packet_Packet{
				Packet: &common.Packet{
					Packet: &common.Packet_Alive{
						Alive: msg.GetAlive(),
					},
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			ctx.Err(err)
			return
		}
		// TODO: this might not thread safe
		err = w.ws.Write(ctx.Context(), websocket.MessageBinary, data)
		if err != nil {
			ctx.Err(err)
			return
		}
	case *peer.CastDesc:
		packet := &ws.Packet{
			P: &ws.Packet_Packet{
				Packet: &common.Packet{
					Packet: &common.Packet_Desc{
						Desc: msg.GetDesc(),
					},
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			ctx.Err(err)
			return
		}
		err = w.ws.Write(ctx.Context(), websocket.MessageBinary, data)
		if err != nil {
			ctx.Err(err)
			return
		}
	case *peer.SendDesc:
		if msg.GetDesc().GetTo() != w.id {
			return
		}
		packet := &ws.Packet{
			P: &ws.Packet_Packet{
				Packet: &common.Packet{
					Packet: &common.Packet_Desc{
						Desc: msg.GetDesc(),
					},
				},
			},
		}
		data, err := proto.Marshal(packet)
		if err != nil {
			ctx.Err(err)
			return
		}
		err = w.ws.Write(ctx.Context(), websocket.MessageBinary, data)
		if err != nil {
			ctx.Err(err)
			return
		}
	default:
		ctx.Unhandled()
	}
}

func (w *PeerWS) close() {
	if w.ws != nil {
		w.ws.Close(websocket.StatusNormalClosure, "stop")
	}
	if w.tun != nil {
		w.tun.Close()
	}
}

func (w *PeerWS) wsRead(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	self := ctx.Self()
	// get controller_rtc
	_, rtcCtrl, err := ctx.ActorSystem().ActorOf(ctx.Context(), "rtc")
	if err != nil {
		panic(err)
	}
	for {
		if !self.IsRunning() {
			break
		}
		_, data, err := w.ws.Read(ctx.Context())
		if errors.Is(err, net.ErrClosed) || errors.Is(err, io.EOF) {
			logger.Error(err)
			ctx.Stop(self)
			break
		}
		if err != nil {
			logger.Error(err)
			continue
		}
		packet := &ws.Packet{}
		err = proto.Unmarshal(data, packet)
		if err != nil {
			logger.Error(err)
			continue
		}
		switch packet := packet.P.(type) {
		case *ws.Packet_Init:
			panic("unreachable")
		case *ws.Packet_Packet:
			switch packet := packet.Packet.Packet.(type) {
			case *common.Packet_Alive:
				ctx.Tell(rtcCtrl, &controller_rtc.Alive{Alive: packet.Alive})
			case *common.Packet_Desc:
				ctx.Tell(rtcCtrl, &controller_rtc.Desc{Desc: packet.Desc})
				desc, err := anypb.New(&peer.SendDesc{
					Desc: packet.Desc,
				})
				if err != nil {
					logger.Error(err)
					continue
				}
				ctx.Tell(ctx.ActorSystem().TopicActor(), &goaktpb.Publish{
					Id:      uuid.NewString(),
					Topic:   "peers",
					Message: desc,
				})
			case *common.Packet_Data:
				data := packet.Data.GetData()
				_, err := w.tun.Write(data)
				if err != nil {
					logger.Error(err)
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

func (w *PeerWS) tunRead(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()
	self := ctx.Self()
	data := make([]byte, Config.BufSize)
	for {
		if !self.IsRunning() {
			break
		}
		n, err := w.tun.Read(data)
		if errors.Is(err, os.ErrClosed) || errors.Is(err, io.EOF) {
			logger.Error(err)
			ctx.Stop(self)
			break
		}
		if err != nil {
			logger.Error(err)
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
			logger.Error(err)
			continue
		}
		err = w.ws.Write(ctx.Context(), websocket.MessageBinary, data)
		if err != nil {
			logger.Error(err)
			continue
		}
	}
}
