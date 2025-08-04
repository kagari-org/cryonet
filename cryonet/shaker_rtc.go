package cryonet

import (
	"context"
	"encoding/json"
	"errors"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/shaker_rtc"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type ShakerRTC struct {
	peerId string
	descId string
	peer   *webrtc.PeerConnection
	dc     *webrtc.DataChannel

	shaked bool
}

func SpawnShakerRTC(parent *goakt.PID, peerId string) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"shaker-rtc-"+peerId,
		&ShakerRTC{
			peerId: peerId,
			descId: uuid.NewString(),
			shaked: false,
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*ShakerRTC)(nil)

func (s *ShakerRTC) PreStart(ctx *goakt.Context) error { return nil }

func (s *ShakerRTC) PostStop(ctx *goakt.Context) error {
	ctx.ActorSystem().CancelSchedule(s.descId)
	if s.dc != nil {
		s.dc.Close()
	}
	if s.peer != nil {
		s.peer.Close()
	}
	return nil
}

func (s *ShakerRTC) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		ctx.ActorSystem().ScheduleOnce(
			context.Background(),
			&shaker_rtc.ITimeout{},
			ctx.Self(),
			Config.ShakeTimeout,
		)
		if err := s.init(ctx); err != nil {
			ctx.Err(err)
			return
		}
	case *shaker_rtc.ITimeout:
		if !s.shaked {
			ctx.Err(errors.New("shake timeout"))
		}
	case *shaker_rtc.ODesc:
		if s.shaked {
			// ignore desc
			return
		}
		var err error
		if IsMaster(s.peerId) {
			err = s.masterReceiveDesc(ctx, msg.Desc)
		} else {
			err = s.slaveReceiveDesc(ctx, msg.Desc)
		}
		if err != nil {
			ctx.Err(err)
			return
		}
	case *shaker_rtc.IShaked:
		ctx.ActorSystem().CancelSchedule(s.descId)
		if s.dc == nil || s.shaked {
			panic("unreachable")
		}
		s.shaked = true
		s.peer.OnDataChannel(nil)
		_, err := SpawnRTCPeer(ctx.Self(), s.peerId, s.dc)
		if err != nil {
			ctx.Err(err)
			return
		}
	case *goaktpb.Mayday:
		ctx.Logger().Error("peer "+ctx.Sender().Name()+"failed: ", msg.GetMessage())
		ctx.Reinstate(ctx.Sender())
		ctx.Stop(ctx.Sender())
		go ctx.Stop(ctx.Self())
	default:
		ctx.Unhandled()
	}
}

func (s *ShakerRTC) init(ctx *goakt.ReceiveContext) error {
	peer, err := webrtc.NewPeerConnection(webrtc.Configuration{
		ICEServers: GetICEServers(),
	})
	if err != nil {
		return err
	}
	s.peer = peer

	self := ctx.Self()
	peer.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateClosed ||
			state == webrtc.PeerConnectionStateFailed ||
			state == webrtc.PeerConnectionStateDisconnected {
			self.Logger().Error("rtc state changed: ", state)
			err := self.Stop(context.Background(), self)
			if err != nil {
				self.Logger().Error(err)
			}
		}
	})

	if IsMaster(s.peerId) {
		dc, err := peer.CreateDataChannel("data", nil)
		if err != nil {
			return err
		}
		s.dc = dc

		offer, err := peer.CreateOffer(nil)
		if err != nil {
			return err
		}
		gather := webrtc.GatheringCompletePromise(peer)
		err = s.peer.SetLocalDescription(offer)
		if err != nil {
			return err
		}
		<-gather
		offer = *peer.LocalDescription()
		offer2, err := json.Marshal(offer)
		if err != nil {
			return err
		}

		ctx.ActorSystem().Schedule(context.Background(), &controller.OShakeDesc{
			Desc: &common.Desc{
				From:   Config.Id,
				To:     s.peerId,
				DescId: s.descId,
				Desc:   offer2,
			},
		}, ctx.Self().Parent(), Config.CheckInterval, goakt.WithReference(s.descId))
	}

	return nil
}

func (s *ShakerRTC) masterReceiveDesc(ctx *goakt.ReceiveContext, desc *common.Desc) error {
	if !(desc.From == s.peerId && desc.To == Config.Id) {
		return nil
	}
	if desc.DescId != s.descId {
		// ignore old desc
		return nil
	}

	ctx.Logger().Info("received desc")

	err := ctx.ActorSystem().CancelSchedule(s.descId)
	if err != nil {
		ctx.Logger().Error(err)
	}

	answer := &webrtc.SessionDescription{}
	err = json.Unmarshal(desc.Desc, answer)
	if err != nil {
		return err
	}
	err = s.peer.SetRemoteDescription(*answer)
	if err != nil {
		return err
	}

	ctx.Tell(ctx.Self(), &shaker_rtc.IShaked{})

	return nil
}

func (s *ShakerRTC) slaveReceiveDesc(ctx *goakt.ReceiveContext, desc *common.Desc) error {
	if !(desc.From == s.peerId && desc.To == Config.Id) {
		return nil
	}

	ctx.Logger().Info("received desc")

	s.descId = desc.DescId

	self := ctx.Self()
	s.peer.OnDataChannel(func(dc *webrtc.DataChannel) {
		s.dc = dc
		err := self.Tell(context.Background(), self, &shaker_rtc.IShaked{})
		if err != nil {
			self.Logger().Error(err)
		}
	})

	offer := &webrtc.SessionDescription{}
	err := json.Unmarshal(desc.Desc, offer)
	if err != nil {
		return err
	}
	err = s.peer.SetRemoteDescription(*offer)
	if err != nil {
		return err
	}

	answer, err := s.peer.CreateAnswer(nil)
	if err != nil {
		return err
	}
	gather := webrtc.GatheringCompletePromise(s.peer)
	err = s.peer.SetLocalDescription(answer)
	if err != nil {
		return err
	}
	<-gather
	answer2 := *s.peer.LocalDescription()

	answer3, err := json.Marshal(answer2)
	if err != nil {
		return err
	}
	ctx.ActorSystem().Schedule(context.Background(), &controller.OShakeDesc{
		Desc: &common.Desc{
			From:   Config.Id,
			To:     s.peerId,
			DescId: s.descId,
			Desc:   answer3,
		},
	}, ctx.Self().Parent(), Config.CheckInterval, goakt.WithReference(s.descId))

	return nil
}
