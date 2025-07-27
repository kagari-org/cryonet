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
	"github.com/kagari-org/cryonet/gen/channels/common"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type Controller struct {
	self            *goakt.PID
	checkScheduleId string
	aliveScheduleId string

	// ws connect
	wsConnectLock  sync.Mutex
	wsConnectPeers map[string]*goakt.PID

	// ws listen
	listener      net.Listener
	server        *http.Server
	wsListenLock  sync.Mutex
	wsListenPeers []*goakt.PID

	// rtc
	rtcPeers map[string]*goakt.PID
}

func SpawnController(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"controller",
		&Controller{
			checkScheduleId: uuid.NewString(),
			aliveScheduleId: uuid.NewString(),
			wsConnectPeers:  make(map[string]*goakt.PID),
			wsListenPeers:   make([]*goakt.PID, 0),
			rtcPeers:        make(map[string]*goakt.PID),
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
	ctx.ActorSystem().CancelSchedule(c.aliveScheduleId)
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

		ctx.Tell(ctx.Self(), &controller.ICheck{})
		ctx.ActorSystem().Schedule(
			ctx.Context(),
			&controller.ICheck{},
			ctx.Self(),
			Config.CheckInterval,
			goakt.WithReference(c.checkScheduleId),
		)

		ctx.Tell(ctx.Self(), &controller.IAlive{})
		ctx.ActorSystem().Schedule(
			ctx.Context(),
			&controller.ICheck{},
			ctx.Self(),
			Config.CheckInterval,
			goakt.WithReference(c.checkScheduleId),
		)
	case *controller.ICheck:
		// ws connect
		wg := sync.WaitGroup{}
		for _, server := range Config.WSServers {
			if c.wsConnectPeers[server].IsRunning() {
				continue
			}
			wg.Add(1)
			go func() {
				defer wg.Done()
				conn, _, err := websocket.Dial(ctx.Context(), server, nil)
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
				c.wsConnectPeers[server] = pid
				c.wsConnectLock.Unlock()
			}()
		}
		wg.Wait()
	case *controller.OAlive:
		// rtc
		for _, peerId := range msg.GetAlive().GetPeers() {
			if peerId == Config.Id {
				continue
			}
			if c.rtcPeers[peerId].IsRunning() {
				continue
			}
			pid, err := SpawnShakerRTC(ctx.Self(), peerId)
			if err != nil {
				ctx.Logger().Error(err)
				continue
			}
			c.rtcPeers[peerId] = pid
		}
	case *controller.IAlive:
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
		alive := &peer.OAlive{
			Alive: &common.Alive{
				Id:    Config.Id,
				Peers: peerIds,
			},
		}
		for _, p := range peers {
			ctx.Tell(p, alive)
		}
	case *controller.OShakeDesc:
		peers := c.getPeers(ctx)
		for _, p := range peers {
			ctx.Tell(p, &peer.ODesc{
				Desc: msg.GetDesc(),
			})
		}
	case *controller.OForwardDesc:
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
	case *goaktpb.Mayday:
		ctx.Logger().Error("shaker "+ctx.Sender().Name()+" failed: ", msg.GetMessage())
		ctx.Stop(ctx.Sender())
	default:
		ctx.Unhandled()
	}
}

func (c *Controller) ServeHTTP(writer http.ResponseWriter, request *http.Request) {
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
	c.wsListenPeers = append(c.wsListenPeers, pid)
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
