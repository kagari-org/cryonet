package cryonet

import (
	"context"
	"errors"

	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	goakt "github.com/tochemey/goakt/v3/actor"
)

type Router struct {
	routes *TTLMap
}

func SpawnRouter(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"router",
		&Router{
			routes: NewTTLMap(Config.CheckInterval),
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.ResumeDirective),
		)),
	)
}

var _ goakt.Actor = (*Router)(nil)

func (r *Router) PreStart(ctx *goakt.Context) error { return nil }
func (r *Router) PostStop(ctx *goakt.Context) error { return nil }

func (r *Router) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *router.OSendPacket:
		if msg.Packet.To == Config.Id {
			// local packets
			ctx.Tell(ctx.Self(), &router.ORecvPacket{
				Packet: msg.Packet,
			})
			return
		}
		// check local links
		if msg.Link == router.Link_ANY || msg.Link == router.Link_LOCAL {
			_, ws, err := ctx.ActorSystem().ActorOf(ctx.Context(), "peer-ws-"+msg.Packet.To)
			if err != nil && !errors.Is(err, goakt.ErrActorNotFound) {
				ctx.Err(err)
				return
			}
			if err == nil {
				ctx.Tell(ws, &peer.OSendPacket{
					Packet: msg.Packet,
				})
				return
			}
			_, rtc, err := ctx.ActorSystem().ActorOf(ctx.Context(), "peer-rtc-"+msg.Packet.To)
			if err != nil && !errors.Is(err, goakt.ErrActorNotFound) {
				ctx.Err(err)
				return
			}
			if err == nil {
				ctx.Tell(rtc, &peer.OSendPacket{
					Packet: msg.Packet,
				})
				return
			}
			if msg.Link == router.Link_LOCAL {
				// no local links
				ctx.Err(errors.New("no local link to peer " + msg.Packet.To))
				return
			}
			// need to forward
		}
		// forward packets
		pid := r.routes.GetNewest(msg.Packet.To)
		if pid == nil {
			ctx.Err(errors.New("no route to peer " + msg.Packet.To))
			return
		}
		ctx.Tell(pid, &peer.OSendPacket{
			Packet: msg.Packet,
		})
	case *router.ORecvPacket:
		if msg.Packet.To == Config.Id {
			// TODO: local packets
			return
		}
		ctx.Tell(ctx.Self(), &router.OSendPacket{
			Packet: msg.Packet,
			Link:   router.Link_LOCAL,
		})
	case *router.OAlive:
		_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), msg.FromPid)
		if err != nil && !errors.Is(err, goakt.ErrActorNotFound) {
			ctx.Err(err)
			return
		}
		if err != nil {
			// peer died
			return
		}
		for _, peerId := range msg.Alive.GetPeers() {
			if peerId == Config.Id {
				continue
			}
			r.routes.Add(peerId, pid)
		}
	default:
		ctx.Unhandled()
	}
}
