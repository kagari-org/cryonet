package cryonet

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"slices"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	goakt "github.com/tochemey/goakt/v3/actor"
	gerrors "github.com/tochemey/goakt/v3/errors"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type Controller struct {
	self            *goakt.PID
	checkScheduleId string

	listener net.Listener
	server   *http.Server
}

func SpawnController(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"controller",
		&Controller{
			checkScheduleId: uuid.NewString(),
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.ResumeDirective),
		)),
	)
}

var _ goakt.Actor = (*Controller)(nil)

func (c *Controller) PreStart(ctx *goakt.Context) error { return nil }

func (c *Controller) PostStop(ctx *goakt.Context) error {
	ctx.ActorSystem().CancelSchedule(c.checkScheduleId)
	if c.server != nil {
		c.server.Close()
	}
	if c.listener != nil {
		c.listener.Close()
	}
	return nil
}

func (c *Controller) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		c.self = ctx.Self()

		// spawn router
		_, err := SpawnRouter(ctx.Self())
		if err != nil {
			ctx.Err(err)
			return
		}

		// spawn alive keeper
		_, err = SpawnAlive(ctx.Self())
		if err != nil {
			ctx.Err(err)
			return
		}

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
		ctx.Logger().Debug("controller check")
		// ws connect
		wg := sync.WaitGroup{}
		for i, server := range Config.WSServers {
			suffix := fmt.Sprintf("wsconnect-%d", i)
			_, _, err := ctx.ActorSystem().ActorOf(ctx.Context(), "shaker-"+suffix)
			if err != nil && !errors.Is(err, gerrors.ErrActorNotFound) {
				ctx.Logger().Error(err)
				continue
			}
			if err == nil {
				continue
			}
			wg.Add(1)
			go func() {
				defer wg.Done()
				context, cancel := context.WithTimeout(ctx.Context(), 10*time.Second)
				defer cancel()
				conn, _, err := websocket.Dial(context, server, nil)
				if err != nil {
					ctx.Logger().Error(err)
					return
				}
				_, err = SpawnShakerWS(ctx.Self(), suffix, conn)
				if err != nil {
					ctx.Logger().Error(err)
				}
			}()
		}
		wg.Wait()
	case *controller.OAlive:
		ctx.Logger().Debug("controller alive from ", msg.From, " peers: ", msg.Alive.Peers)
		ids := append(msg.Alive.Peers, msg.From)
		slices.Sort(ids)
		ids = slices.Compact(ids)
		for _, id := range ids {
			if id == Config.Id {
				continue
			}
			_, _, err := ctx.ActorSystem().ActorOf(ctx.Context(), "shaker-ice-"+id)
			if err != nil && !errors.Is(err, gerrors.ErrActorNotFound) {
				ctx.Err(err)
				return
			}
			if err == nil {
				continue
			}
			_, err = SpawnShakerICE(ctx.Self(), id)
			if err != nil {
				ctx.Err(err)
				return
			}
		}
	case *goaktpb.Mayday:
		ctx.Logger().Error("shaker "+ctx.Sender().Name()+" failed: ", msg.GetMessage())
		ctx.Stop(ctx.Sender())
		ctx.Logger().Debug("shaker " + ctx.Sender().Name() + " stopped")
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

	_, err = SpawnShakerWS(c.self, "wslisten-"+uuid.NewString(), conn)
	if err != nil {
		c.self.Logger().Error(err)
		return
	}
}
