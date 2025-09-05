package cryonet

import (
	"context"
	"time"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/alive"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type Alive struct {
	checkScheduleId string
	table           map[string]time.Time
}

func SpawnAlive(parent *goakt.PID) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"alive",
		&Alive{
			checkScheduleId: uuid.NewString(),
			table:           make(map[string]time.Time),
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
			if _, ok := actor.Actor().(*PeerWS); ok {
				if _, ok := a.table[actor.ID()]; !ok {
					a.table[actor.ID()] = time.Now()
				}
			} else if _, ok := actor.Actor().(*PeerRTC); ok {
				if _, ok := a.table[actor.ID()]; !ok {
					a.table[actor.ID()] = time.Now()
				}
			}
		}
		for id, last := range a.table {
			if time.Since(last) > Config.PeerTimeout {
				delete(a.table, id)
				_, pid, err := ctx.ActorSystem().ActorOf(ctx.Context(), id)
				if err != nil {
					ctx.Logger().Error("failed to get actor", "id", id, "error", err)
					continue
				}
				ctx.Tell(pid, &peer.IStop{})
			}
		}
	case *alive.OAlive:
		a.table[msg.FromPid] = time.Now()
	default:
		ctx.Unhandled()
	}
}
