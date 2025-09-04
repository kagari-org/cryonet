package cryonet

import (
	"context"
	"errors"
	"time"

	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/actors/shaker_rtc"
	"github.com/kagari-org/cryonet/gen/channel"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
)

type Router struct {
	routes                 *TTLMap
	blockingAnswerRequests map[string]chan *channel.Description
}

func SpawnRouter(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"router",
		&Router{
			routes:                 NewTTLMap(Config.CheckInterval),
			blockingAnswerRequests: make(map[string]chan *channel.Description),
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
			err := r.handleLocalPacket(ctx, msg.Packet)
			if err != nil {
				ctx.Err(err)
			}
			return
		}
		ctx.Tell(ctx.Self(), &router.OSendPacket{
			Packet: msg.Packet,
			Link:   router.Link_LOCAL,
		})
	case *router.IAlive:
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

func (r *Router) handleLocalPacket(ctx *goakt.ReceiveContext, packet *channel.Normal) error {
	switch payload := packet.Payload.(type) {
	case *channel.Normal_Alive:
		ctx.Self().Tell(ctx.Context(), ctx.Self(), &router.IAlive{
			FromPid: packet.From,
			Alive:   payload.Alive,
		})
		// TODO: keep alive
	case *channel.Normal_Description:
		if IsMaster(packet.From) {
			// the response from slave
			ch, ok := r.blockingAnswerRequests[packet.From]
			if !ok {
				return errors.New("unknown reponse received. maybe timeout")
			}
			// TODO: add lock
			ch <- payload.Description
		} else {
			// the request from master
			_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), "shaker-rtc-"+packet.From)
			if err != nil && !errors.Is(err, goakt.ErrActorNotFound) {
				return err
			}
			if err != nil {
				// TODO: spawn shaker
				// pid = ...
			}
			response, err := ctx.Self().Ask(ctx.Context(), pid, &shaker_rtc.OOnOffer{
				Offer: payload.Description.Data,
			}, time.Second*5)
			if err != nil {
				return err
			}
			res := response.(*shaker_rtc.OOnOfferResponse)
			err = ctx.Self().Tell(ctx.Context(), ctx.Self(), &router.OSendPacket{
				Link: router.Link_FORWARD,
				Packet: &channel.Normal{
					From: Config.Id,
					To:   packet.From,
					Payload: &channel.Normal_Description{
						Description: &channel.Description{
							Data: res.Answer,
						},
					},
				},
			})
			if err != nil {
				return err
			}
		}
	case *channel.Normal_Candidate:
		_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), "shaker-rtc-"+packet.From)
		if err != nil {
			return err
		}
		err = ctx.Self().Tell(ctx.Context(), pid, &shaker_rtc.OOnCandidate{
			Candidate: payload.Candidate.Data,
		})
		if err != nil {
			return err
		}
	}
	return nil
}

func AskPeerForAnswer(peerId string, offer *webrtc.SessionDescription) (*webrtc.SessionDescription, error) {
	panic("unimplemented")
}
