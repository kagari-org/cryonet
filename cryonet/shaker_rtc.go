package cryonet

import (
	"context"
	"encoding/json"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/actors/shaker_rtc"
	"github.com/kagari-org/cryonet/gen/channel"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type ShakerRTC struct {
	peerId string
	connId string
	peer   *webrtc.PeerConnection
	dc     *webrtc.DataChannel
}

func SpawnShakerRTC(parent *goakt.PID, peerId string) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"shaker-rtc-"+peerId,
		&ShakerRTC{
			peerId: peerId,
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
		if err := s.init(ctx); err != nil {
			ctx.Err(err)
			return
		}
		ctx.Tell(ctx.Self(), &shaker_rtc.IShake{})
	case *shaker_rtc.IShake:
		if !IsMaster(s.peerId) {
			// TODO: add timeout here
			return
		}
		err := s.master(ctx, msg.GetRestart())
		if err != nil {
			ctx.Err(err)
			return
		}
		ctx.Tell(ctx.Self(), &shaker_rtc.IDataChannelSet{})
	case *shaker_rtc.OOnOffer:
		if IsMaster(s.peerId) {
			panic("unreachable")
		}
		if msg.Offer.Restart && s.connId != msg.Offer.ConnId {
			ctx.Response(&shaker_rtc.OOnOfferResponse{
				Answer: &channel.Answer{
					Success: false,
					Data:    nil,
				},
			})
			return
		}

		s.connId = msg.Offer.ConnId
		offer := webrtc.SessionDescription{}
		err := json.Unmarshal(msg.Offer.Data, &offer)
		if err != nil {
			ctx.Err(err)
			return
		}
		answer, err := s.slave(&offer)
		if err != nil {
			ctx.Err(err)
			return
		}
		answerBytes, err := json.Marshal(answer)
		if err != nil {
			ctx.Err(err)
			return
		}
		ctx.Response(&shaker_rtc.OOnOfferResponse{
			Answer: &channel.Answer{
				Success: true,
				Data:    answerBytes,
			},
		})
	case *shaker_rtc.OOnCandidate:
		candidate := webrtc.ICECandidateInit{}
		err := json.Unmarshal(msg.GetCandidate(), &candidate)
		if err != nil {
			ctx.Logger().Error("failed to unmarshal candidate: ", err)
			return
		}
		err = s.peer.AddICECandidate(candidate)
		if err != nil {
			ctx.Logger().Error("failed to add ice candidate: ", err)
			return
		}
	case *shaker_rtc.IDataChannelSet:
		if s.dc == nil {
			panic("unreachable")
		}
		// TODO: spawn child
	case *goaktpb.Mayday:
		ctx.Logger().Error("peer "+ctx.Sender().Name()+" failed: ", msg.GetMessage())
		ctx.Stop(ctx.Sender())
		ctx.Stop(ctx.Self())
	default:
		ctx.Unhandled()
	}
}

func (s *ShakerRTC) init(ctx *goakt.ReceiveContext) error {
	self := ctx.Self()
	_, rtr, err := ctx.ActorSystem().ActorOf(ctx.Context(), "router")
	if err != nil {
		panic("unreachable")
	}

	peer, err := webrtc.NewPeerConnection(webrtc.Configuration{
		ICEServers: GetICEServers(),
	})
	if err != nil {
		return err
	}
	s.peer = peer

	peer.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		self.Logger().Debug("rtc state changed: ", state.String())
		if state == webrtc.PeerConnectionStateFailed {
			err := self.Tell(context.Background(), self, &shaker_rtc.IShake{
				Restart: true,
			})
			if err != nil {
				self.Logger().Error(err)
			}
		}
	})

	peer.OnICECandidate(func(candidate *webrtc.ICECandidate) {
		if candidate == nil {
			return
		}
		self.Logger().Debug("new ice candidate: ", candidate.String())
		candidateBytes, err := json.Marshal(candidate.ToJSON())
		if err != nil {
			self.Logger().Error("failed to marshal candidate: ", err)
			return
		}
		err = self.Tell(context.Background(), rtr, &router.OSendPacket{
			Link: router.Link_FORWARD,
			Packet: &channel.Normal{
				From: Config.Id,
				To:   s.peerId,
				Payload: &channel.Normal_Candidate{
					Candidate: &channel.Candidate{
						Data: candidateBytes,
					},
				},
			},
		})
		if err != nil {
			self.Logger().Error(err)
		}
	})

	master := IsMaster(s.peerId)
	peer.OnDataChannel(func(dc *webrtc.DataChannel) {
		if master {
			panic("unreachable")
		}
		s.dc = dc
		err := self.Tell(context.Background(), ctx.Self(), &shaker_rtc.IDataChannelSet{})
		if err != nil {
			self.Logger().Error(err)
		}
	})

	if master {
		dc, err := peer.CreateDataChannel("dc", nil)
		if err != nil {
			return err
		}
		s.dc = dc
		err = self.Tell(ctx.Context(), ctx.Self(), &shaker_rtc.IDataChannelSet{})
		if err != nil {
			return err
		}
	}

	return nil
}

func (s *ShakerRTC) master(ctx *goakt.ReceiveContext, restart bool) error {
	if s.peer == nil || s.dc == nil {
		panic("unreachable")
	}
	s.connId = uuid.NewString()

	offer, err := s.peer.CreateOffer(&webrtc.OfferOptions{
		ICERestart: restart,
	})
	if err != nil {
		return err
	}
	err = s.peer.SetLocalDescription(offer)
	if err != nil {
		return err
	}
	// this calls peer's s.slave()
	answer, err := AskForAnswer(ctx.Self(), s.peerId, s.connId, restart, &offer)
	if err != nil {
		return err
	}
	err = s.peer.SetRemoteDescription(*answer)
	if err != nil {
		return err
	}

	return nil
}

func (s *ShakerRTC) slave(offer *webrtc.SessionDescription) (*webrtc.SessionDescription, error) {
	if s.peer == nil {
		panic("unreachable")
	}

	err := s.peer.SetRemoteDescription(*offer)
	if err != nil {
		return nil, err
	}
	answer, err := s.peer.CreateAnswer(nil)
	if err != nil {
		return nil, err
	}
	err = s.peer.SetLocalDescription(answer)
	if err != nil {
		return nil, err
	}

	return &answer, nil
}
