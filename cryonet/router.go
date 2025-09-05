package cryonet

import (
	"context"
	"encoding/json"
	"errors"
	"sync"
	"time"

	"github.com/kagari-org/cryonet/gen/actors/alive"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/actors/shaker_rtc"
	"github.com/kagari-org/cryonet/gen/channel"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	gerrors "github.com/tochemey/goakt/v3/errors"
)

type Router struct {
	routes *TTLMap

	blockingAnswerLock     sync.Mutex
	blockingAnswerRequests map[string]chan *channel.Answer
}

func SpawnRouter(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"router",
		&Router{
			routes:                 NewTTLMap(Config.CheckInterval),
			blockingAnswerRequests: make(map[string]chan *channel.Answer),
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
		if msg.Link == router.Link_SPECIFIC {
			_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), msg.SpecificPid)
			if err != nil {
				ctx.Err(err)
				return
			}
			ctx.Tell(pid, &peer.OSendPacket{
				Packet: msg.Packet,
			})
			return
		}
		// check local links
		if msg.Link == router.Link_ANY || msg.Link == router.Link_LOCAL {
			_, ws, err := ctx.ActorSystem().ActorOf(ctx.Context(), "peer-ws-"+msg.Packet.To)
			if err != nil && !errors.Is(err, gerrors.ErrActorNotFound) {
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
			if err != nil && !errors.Is(err, gerrors.ErrActorNotFound) {
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
		if err != nil && !errors.Is(err, gerrors.ErrActorNotFound) {
			ctx.Err(err)
			return
		}
		if err != nil {
			// peer died
			return
		}
		for _, peerId := range msg.Alive.Peers {
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
		if packet.From != Config.Id {
			// not bootstrap alive from self, update route
			ctx.Tell(ctx.Self(), &router.IAlive{
				FromPid: ctx.Sender().ID(),
				Alive:   payload.Alive,
			})

			_, al, err := ctx.ActorSystem().ActorOf(ctx.Context(), "alive")
			if err != nil {
				panic("unreachable")
			}
			ctx.Tell(al, &alive.OAlive{
				FromPid: ctx.Sender().ID(),
				From:    packet.From,
			})
		}
		_, ctrl, err := ctx.ActorSystem().ActorOf(ctx.Context(), "controller")
		if err != nil {
			panic("unreachable")
		}
		ctx.Tell(ctrl, &controller.OAlive{
			From:  packet.From,
			Alive: payload.Alive,
		})
	case *channel.Normal_Offer:
		// the request from master
		// we are slave
		if IsMaster(packet.From) {
			panic("unreachable")
		}
		_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), "shaker-rtc-"+packet.From)
		if err != nil {
			// tell master to restart shaker to retry
			err := ctx.Self().Tell(ctx.Context(), ctx.Self(), &router.OSendPacket{
				Link: router.Link_ANY,
				Packet: &channel.Normal{
					From: Config.Id,
					To:   packet.From,
					Payload: &channel.Normal_Answer{
						Answer: &channel.Answer{
							Success: false,
							Data:    nil,
						},
					},
				},
			})
			return err
		}
		response, err := ctx.Self().Ask(ctx.Context(), pid, &shaker_rtc.OOnOffer{
			Offer: payload.Offer,
		}, time.Second*5)
		if err != nil {
			return err
		}
		res := response.(*shaker_rtc.OOnOfferResponse)
		err = ctx.Self().Tell(ctx.Context(), ctx.Self(), &router.OSendPacket{
			Link: router.Link_ANY,
			Packet: &channel.Normal{
				From: Config.Id,
				To:   packet.From,
				Payload: &channel.Normal_Answer{
					Answer: res.Answer,
				},
			},
		})
		if err != nil {
			return err
		}
	case *channel.Normal_Answer:
		// the response from slave
		// we are master
		if !IsMaster(packet.From) {
			panic("unreachable")
		}
		r.blockingAnswerLock.Lock()
		ch, ok := r.blockingAnswerRequests[packet.From]
		r.blockingAnswerLock.Unlock()
		if !ok {
			return errors.New("unknown reponse received. maybe timeout")
		}
		ch <- payload.Answer
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

func AskForAnswer(cid *goakt.PID, peerId string, conn_id string, restart bool, offer *webrtc.SessionDescription) (*webrtc.SessionDescription, error) {
	_, rtr, err := cid.ActorSystem().ActorOf(context.Background(), "router")
	if err != nil {
		panic("unreachable")
	}
	offerBytes, err := json.Marshal(offer)
	if err != nil {
		return nil, err
	}

	ch := make(chan *channel.Answer, 1)
	rtrActor := rtr.Actor().(*Router)
	rtrActor.blockingAnswerLock.Lock()
	rtrActor.blockingAnswerRequests[peerId] = ch
	rtrActor.blockingAnswerLock.Unlock()
	defer func() {
		rtrActor.blockingAnswerLock.Lock()
		delete(rtrActor.blockingAnswerRequests, peerId)
		rtrActor.blockingAnswerLock.Unlock()
		close(ch)
	}()

	err = cid.Tell(context.Background(), rtr, &router.OSendPacket{
		Link: router.Link_ANY,
		Packet: &channel.Normal{
			From: Config.Id,
			To:   peerId,
			Payload: &channel.Normal_Offer{
				Offer: &channel.Offer{
					ConnId:  conn_id,
					Restart: restart,
					Data:    offerBytes,
				},
			},
		},
	})
	if err != nil {
		return nil, err
	}

	select {
	case answer := <-ch:
		if !answer.Success {
			return nil, errors.New("failed to shake with peer, need to restart shaker")
		}
		desc := webrtc.SessionDescription{}
		err = json.Unmarshal(answer.Data, &desc)
		if err != nil {
			return nil, err
		}
		return &desc, nil
	case <-time.After(Config.ShakeTimeout):
		return nil, errors.New("timeout waiting for answer")
	}
}
