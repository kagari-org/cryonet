package cryonet

import (
	"context"
	"time"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/alive"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/channel"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type Alive struct {
	checkScheduleId string
	table           map[string]*AliveItem
}

type AliveItem struct {
	peerId string
	time   time.Time
}

func SpawnAlive(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"alive",
		&Alive{
			checkScheduleId: uuid.NewString(),
			table:           make(map[string]*AliveItem),
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.ResumeDirective),
		)),
	)
}

var _ goakt.Actor = (*Alive)(nil)

func (a *Alive) PreStart(ctx *goakt.Context) error { return nil }
func (a *Alive) PostStop(ctx *goakt.Context) error {
	ctx.ActorSystem().CancelSchedule(a.checkScheduleId)
	return nil
}

func (a *Alive) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		ctx.Tell(ctx.Self(), &alive.ICheck{})
		ctx.ActorSystem().Schedule(
			context.Background(),
			&alive.ICheck{},
			ctx.Self(),
			Config.CheckInterval,
		)
	case *alive.ICheck:
		actors := ctx.ActorSystem().Actors()
		for _, actor := range actors {
			if ws, ok := actor.Actor().(*PeerWS); ok {
				if _, ok := a.table[actor.Name()]; !ok {
					a.table[actor.Name()] = &AliveItem{
						peerId: ws.peerId,
						time:   time.Now(),
					}
				}
			} else if rtc, ok := actor.Actor().(*PeerRTC); ok {
				if _, ok := a.table[actor.Name()]; !ok {
					a.table[actor.Name()] = &AliveItem{
						peerId: rtc.peerId,
						time:   time.Now(),
					}
				}
			}
		}
		for id, item := range a.table {
			if time.Since(item.time) > Config.PeerTimeout {
				delete(a.table, id)
				_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), id)
				if err != nil {
					ctx.Logger().Error("failed to get actor", "id", id, "error", err)
					continue
				}
				ctx.Tell(pid, &peer.OStop{})
			}
		}

		_, rtr, err := ctx.ActorSystem().ActorOf(ctx.Context(), "router")
		if err != nil {
			panic("unreachable")
		}
		peers := make([]string, 0, len(a.table))
		for _, item := range a.table {
			peers = append(peers, item.peerId)
		}
		ctx.Logger().Debug("sending alive ", peers)
		// send alive packet to self to bootstrap rtc connections
		ctx.Tell(rtr, &router.OSendPacket{
			Link: router.Link_ANY,
			Packet: &channel.Normal{
				From: Config.Id,
				To:   Config.Id,
				Payload: &channel.Normal_Alive{
					Alive: &channel.Alive{
						Peers: peers,
					},
				},
			},
		})
		// send alive to all peers
		for pid, item := range a.table {
			ctx.Tell(rtr, &router.OSendPacket{
				Link:        router.Link_SPECIFIC,
				SpecificPid: pid,
				Packet: &channel.Normal{
					From: Config.Id,
					To:   item.peerId,
					Payload: &channel.Normal_Alive{
						Alive: &channel.Alive{
							Peers: peers,
						},
					},
				},
			})
		}
	case *alive.OAlive:
		a.table[msg.FromPid] = &AliveItem{
			peerId: msg.From,
			time:   time.Now(),
		}
	default:
		ctx.Unhandled()
	}
}
