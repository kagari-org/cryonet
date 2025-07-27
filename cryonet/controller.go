package cryonet

import (
	"context"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/shaker_rtc"
	"github.com/kagari-org/cryonet/gen/channels/common"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type Controller struct {
	self            *goakt.PID
	checkScheduleId string

	// ws connect
	wsConnectLock    sync.Mutex
	wsConnectShakers map[string]*goakt.PID

	// ws listen
	listener        net.Listener
	server          *http.Server
	wsListenLock    sync.Mutex
	wsListenShakers []*goakt.PID

	// rtc
	rtcShakers map[string]*goakt.PID
}

func SpawnController(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"controller",
		&Controller{
			checkScheduleId:  uuid.NewString(),
			wsConnectShakers: make(map[string]*goakt.PID),
			wsListenShakers:  make([]*goakt.PID, 0),
			rtcShakers:       make(map[string]*goakt.PID),
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*Controller)(nil)

func (c *Controller) PreStart(ctx *goakt.Context) error { return nil }

func (c *Controller) PostStop(ctx *goakt.Context) error {
	if c.server != nil {
		c.server.Close()
	}
	if c.listener != nil {
		c.listener.Close()
	}
	ctx.ActorSystem().CancelSchedule(c.checkScheduleId)
	return nil
}

func (c *Controller) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		c.self = ctx.Self()

		// ws listen
		listener, err := net.Listen("tcp", Config.Listen)
		if err != nil {
			ctx.Err(err)
			return
		}
		server := &http.Server{
			Handler: c,
		}
		c.listener = listener
		c.server = server
		go server.Serve(listener)

		ctx.Tell(ctx.Self(), &controller.ICheck{})
		ctx.ActorSystem().Schedule(
			context.Background(),
			&controller.ICheck{},
			ctx.Self(),
			Config.CheckInterval,
			goakt.WithReference(c.checkScheduleId),
		)
	case *controller.ICheck:
		// ws connect
		wg := sync.WaitGroup{}
		for _, server := range Config.WSServers {
			if c.wsConnectShakers[server].IsRunning() {
				continue
			}
			wg.Add(1)
			go func() {
				defer wg.Done()
				context, cancel := context.WithTimeout(ctx.Context(), Config.ShakeTimeout)
				defer cancel()
				conn, _, err := websocket.Dial(context, server, nil)
				if err != nil {
					ctx.Logger().Error(err)
					return
				}
				pid, err := SpawnShakerWS(ctx.Self(), conn)
				if err != nil {
					ctx.Logger().Error(err)
					return
				}
				c.wsConnectLock.Lock()
				c.wsConnectShakers[server] = pid
				c.wsConnectLock.Unlock()
			}()
		}
		wg.Wait()

		peers := c.getPeers(ctx)
		peerIds := make([]string, len(peers))
		for i, p := range peers {
			res := ctx.Ask(p, &peer.OGetPeerId{}, time.Second*5).(*peer.OGetPeerIdResponse)
			if res == nil {
				// Ask called ctx.Err
				return
			}
			peerIds[i] = res.GetPeerId()
		}
		alive := &common.Alive{
			Id:    Config.Id,
			Peers: peerIds,
		}
		ctx.Logger().Info("sending alive message: ", alive)
		for _, p := range peers {
			ctx.Tell(p, &peer.OAlive{Alive: alive})
		}
		// send alive to self, so that it will create rtc from ws peer
		ctx.Tell(ctx.Self(), &controller.OAlive{Alive: alive})
	case *controller.OAlive:
		// rtc
		ctx.Logger().Info("received alive message: ", msg.GetAlive())
		for _, peerId := range msg.GetAlive().GetPeers() {
			if peerId == Config.Id {
				continue
			}
			if c.rtcShakers[peerId].IsRunning() {
				continue
			}
			pid, err := SpawnShakerRTC(ctx.Self(), peerId)
			if err != nil {
				ctx.Logger().Error(err)
				continue
			}
			c.rtcShakers[peerId] = pid
		}
	case *controller.OShakeDesc:
		ctx.Logger().Info("sending shake desc: ", msg.GetDesc())
		peers := c.getPeers(ctx)
		for _, p := range peers {
			ctx.Tell(p, &peer.ODesc{
				Desc: msg.GetDesc(),
			})
		}
	case *controller.OForwardDesc:
		ctx.Logger().Info("received shake desc: ", msg.GetDesc())
		if msg.GetDesc().To == Config.Id {
			shaker := c.rtcShakers[msg.GetDesc().From]
			if shaker.IsRunning() {
				ctx.Tell(shaker, &shaker_rtc.ODesc{
					Desc: msg.GetDesc(),
				})
			}
		} else {
			peers := c.getPeers(ctx)
			for _, p := range peers {
				res := ctx.Ask(p, &peer.OGetPeerId{}, time.Second*5).(*peer.OGetPeerIdResponse)
				if res == nil {
					// Ask called ctx.Err
					return
				}
				if res.GetPeerId() == msg.GetDesc().GetTo() {
					ctx.Tell(p, &peer.ODesc{
						Desc: msg.GetDesc(),
					})
					return
				}
			}
		}
	case *goaktpb.Mayday:
		ctx.Logger().Error("shaker "+ctx.Sender().Name()+" failed: ", msg.GetMessage())
		ctx.Stop(ctx.Sender())
	default:
		ctx.Unhandled()
	}
}

func (c *Controller) ServeHTTP(writer http.ResponseWriter, request *http.Request) {
	c.self.Logger().Info("WebSocket connection request from", request.RemoteAddr)
	conn, err := websocket.Accept(writer, request, nil)
	if err != nil {
		c.self.Logger().Error(err)
		return
	}

	pid, err := SpawnShakerWS(c.self, conn)
	if err != nil {
		c.self.Logger().Error(err)
		return
	}

	c.wsListenLock.Lock()
	c.wsListenShakers = append(c.wsListenShakers, pid)
	c.wsListenLock.Unlock()
}

func (c *Controller) getPeers(ctx *goakt.ReceiveContext) []*goakt.PID {
	actors := ctx.ActorSystem().Actors()
	peers := make([]*goakt.PID, 0)
	for _, actor := range actors {
		if _, ok := actor.Actor().(*PeerWS); ok {
			peers = append(peers, actor)
		} else if _, ok := actor.Actor().(*PeerRTC); ok {
			peers = append(peers, actor)
		}
	}
	return peers
}
