package cryonet

import (
	"context"
	"errors"
	"io"
	"net"
	"os"
	"time"

	"github.com/coder/websocket"
	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type PeerWS struct {
	peerId string
	ws     *websocket.Conn
	tun    *os.File

	last            time.Time
	checkScheduleId string

	context context.Context
	cancel  func()
}

func SpawnWSPeer(parent *goakt.PID, peerId string, ws *websocket.Conn) (*goakt.PID, error) {
	ctx, cancel := context.WithCancel(context.Background())
	return parent.SpawnChild(
		context.Background(),
		"peer-ws-"+peerId,
		&PeerWS{
			peerId:          peerId,
			ws:              ws,
			last:            time.Now(),
			checkScheduleId: uuid.NewString(),
			context:         ctx,
			cancel:          cancel,
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
	ctx.ActorSystem().CancelSchedule(p.checkScheduleId)
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
		ctx.Err(errors.New("stop peer " + p.peerId))
	case *peer.OGetPeerId:
		ctx.Response(&peer.OGetPeerIdResponse{
			PeerId: p.peerId,
		})
	case *peer.OAlive:
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
		self := ctx.Self()
		go func() {
			err = p.ws.Write(ctx.Context(), websocket.MessageBinary, data)
			if err != nil {
				self.Logger().Error(err)
			}
		}()
	case *peer.ODesc:
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
		self := ctx.Self()
		go func() {
			err = p.ws.Write(ctx.Context(), websocket.MessageBinary, data)
			if err != nil {
				self.Logger().Error(err)
			}
		}()
	default:
		ctx.Unhandled()
	}
}

func (p *PeerWS) wsRead(self *goakt.PID) {
	logger := self.Logger()
	for {
		if !self.IsRunning() {
			break
		}
		_, data, err := p.ws.Read(p.context)
		if errors.Is(err, net.ErrClosed) || errors.Is(err, io.EOF) {
			logger.Error(err)
			err := self.Tell(context.Background(), self, &peer.IStop{})
			if err != nil {
				logger.Error(err)
			}
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
				logger.Info("Received alive packet: ", packet.Alive)
				err := self.Tell(
					context.Background(),
					self,
					&peer.IAlive{},
				)
				if err != nil {
					logger.Error(err)
				}
				err = self.Tell(
					context.Background(),
					self.Parent().Parent(),
					&controller.OAlive{Alive: packet.Alive},
				)
				if err != nil {
					logger.Error(err)
				}
			case *common.Packet_Desc:
				err := self.Tell(
					context.Background(),
					self.Parent().Parent(),
					&controller.OForwardDesc{Desc: packet.Desc},
				)
				if err != nil {
					logger.Error(err)
				}
			case *common.Packet_Data:
				data := packet.Data.GetData()
				_, err := p.tun.Write(data)
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

func (p *PeerWS) tunRead(self *goakt.PID) {
	logger := self.Logger()
	data := make([]byte, Config.BufSize)
	for {
		if !self.IsRunning() {
			break
		}
		n, err := p.tun.Read(data)
		if errors.Is(err, os.ErrClosed) || errors.Is(err, io.EOF) {
			logger.Error(err)
			err := self.Tell(context.Background(), self, &peer.IStop{})
			if err != nil {
				logger.Error(err)
			}
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
		err = p.ws.Write(p.context, websocket.MessageBinary, data)
		if err != nil {
			logger.Error(err)
			continue
		}
	}
}
