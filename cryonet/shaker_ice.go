package cryonet

import (
	"context"
	"errors"
	"sync/atomic"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/actors/shaker_ice"
	"github.com/kagari-org/cryonet/gen/channel"
	"github.com/kagari-org/wireguard-go/device"
	"github.com/pion/ice/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
)

type ShakerICE struct {
	peerId string
	shaked atomic.Bool
	sk     device.NoisePrivateKey

	agent       *ice.Agent
	localUfrag  string
	localPwd    string
	remoteUfrag string
	remotePwd   string

	timeoutId string
}

func SpawnShakerICE(parent *goakt.PID, peerId string) (*goakt.PID, error) {
	sk, err := GenWGPrivkey()
	if err != nil {
		return nil, err
	}
	return parent.SpawnChild(
		context.Background(),
		"shaker-ice-"+peerId,
		&ShakerICE{
			peerId: peerId,
			sk:     sk,
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*ShakerICE)(nil)

func (s *ShakerICE) PreStart(ctx *goakt.Context) error { return nil }

func (s *ShakerICE) PostStop(ctx *goakt.Context) error {
	ctx.ActorSystem().CancelSchedule(s.timeoutId)
	if s.agent != nil {
		s.agent.Close()
	}
	return nil
}

func (s *ShakerICE) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		s.timeoutId = uuid.NewString()
		ctx.ActorSystem().ScheduleOnce(
			context.Background(),
			&shaker_ice.OStop{},
			ctx.Self(),
			Config.ShakeTimeout,
			goakt.WithReference(s.timeoutId),
		)

		self := ctx.Self()
		_, rtr, err := ctx.ActorSystem().ActorOf(ctx.Context(), "router")
		if err != nil {
			panic("unreachable")
		}

		agent, err := ice.NewAgent(&ice.AgentConfig{
			Urls: GetICEServers(),
			NetworkTypes: []ice.NetworkType{
				ice.NetworkTypeUDP4,
				ice.NetworkTypeUDP6,
			},
		})
		if err != nil {
			ctx.Err(err)
			return
		}
		s.agent = agent

		agent.OnConnectionStateChange(func(state ice.ConnectionState) {
			check := func(err error) bool {
				if err != nil {
					self.Logger().Error(err)
					err := self.Tell(context.Background(), self, &shaker_ice.OStop{})
					if err != nil {
						self.Logger().Error(err)
					}
					return false
				}
				return true
			}
			self.Logger().Info("ice state changed: ", state.String())
			if state == ice.ConnectionStateDisconnected || state == ice.ConnectionStateFailed {
				if !s.shaked.Load() {
					self.Logger().Error("ice connection failed before shaked")
					err := self.Tell(context.Background(), self, &shaker_ice.OStop{})
					if err != nil {
						self.Logger().Error(err)
					}
					return
				}
				if !check(agent.Restart(s.localUfrag, s.localPwd)) {
					return
				}
				if !check(agent.SetRemoteCredentials(s.remoteUfrag, s.remotePwd)) {
					return
				}
				if !check(s.agent.GatherCandidates()) {
					return
				}
			}
		})

		agent.OnCandidate(func(c ice.Candidate) {
			if c == nil {
				return
			}
			candidate := c.Marshal()
			self.Logger().Debug("new ice candidate: ", candidate)
			err = self.Tell(context.Background(), rtr, &router.OSendPacket{
				Link: router.Link_ANY,
				Packet: &channel.Packet{
					From: Config.Id,
					To:   s.peerId,
					Payload: &channel.Packet_Candidate{
						Candidate: &channel.Candidate{
							Candidate: candidate,
						},
					},
				},
			})
			if err != nil {
				self.Logger().Error(err)
			}
		})

		if IsMaster(s.peerId) {
			pk := GenWGPubkey(&s.sk)
			ufrag, pwd, err := s.agent.GetLocalUserCredentials()
			if err != nil {
				ctx.Err(err)
				return
			}

			answer, err := AskForAnswer(ctx.Self(), s.peerId, &channel.Offer{
				Ufrag:  ufrag,
				Pwd:    pwd,
				Pubkey: pk[:],
			})
			if err != nil {
				ctx.Err(err)
				return
			}
			if !answer.Success {
				ctx.Err(errors.New("need to reshake"))
				return
			}

			s.localUfrag = ufrag
			s.localPwd = pwd
			s.remoteUfrag = answer.Ufrag
			s.remotePwd = answer.Pwd
			s.shaked.Store(true)

			err = agent.GatherCandidates()
			if err != nil {
				ctx.Err(err)
				return
			}

			go func() {
				conn, err := s.agent.Dial(context.Background(), s.remoteUfrag, s.remotePwd)
				if err != nil {
					self.Logger().Error("failed to dial: ", err)
					err := self.Tell(context.Background(), self, &shaker_ice.OStop{})
					if err != nil {
						self.Logger().Error(err)
					}
					return
				}
				self.ActorSystem().CancelSchedule(s.timeoutId)
				_, err = SpawnICEPeer(self, s.peerId, conn, s.sk, device.NoisePublicKey(answer.Pubkey))
				if err != nil {
					self.Logger().Error("failed to spawn ice peer: ", err)
					err := self.Tell(context.Background(), self, &shaker_ice.OStop{})
					if err != nil {
						self.Logger().Error(err)
					}
					return
				}
			}()
		}
	case *shaker_ice.OStop:
		ctx.Err(errors.New("stop shaker " + s.peerId))
	case *shaker_ice.OOnOffer:
		if IsMaster(s.peerId) {
			panic("unreachable")
		}

		if s.shaked.Load() {
			ctx.Response(&shaker_ice.OOnOfferResponse{
				Answer: &channel.Answer{
					Success: false,
				},
			})
			ctx.Err(errors.New("remote restarted, restart self now"))
			return
		}

		pk := GenWGPubkey(&s.sk)
		ufrag, pwd, err := s.agent.GetLocalUserCredentials()
		if err != nil {
			ctx.Err(err)
			return
		}
		s.localUfrag = ufrag
		s.localPwd = pwd
		s.remoteUfrag = msg.Offer.Ufrag
		s.remotePwd = msg.Offer.Pwd

		s.shaked.Store(true)

		ctx.Response(&shaker_ice.OOnOfferResponse{
			Answer: &channel.Answer{
				Success: true,
				Ufrag:   ufrag,
				Pwd:     pwd,
				Pubkey:  pk[:],
			},
		})

		err = s.agent.GatherCandidates()
		if err != nil {
			ctx.Err(err)
			return
		}

		self := ctx.Self()
		go func() {
			conn, err := s.agent.Accept(context.Background(), s.remoteUfrag, s.remotePwd)
			if err != nil {
				self.Logger().Error("failed to dial: ", err)
				err := self.Tell(context.Background(), self, &shaker_ice.OStop{})
				if err != nil {
					self.Logger().Error(err)
				}
				return
			}
			self.ActorSystem().CancelSchedule(s.timeoutId)
			_, err = SpawnICEPeer(self, s.peerId, conn, s.sk, device.NoisePublicKey(msg.Offer.Pubkey))
			if err != nil {
				self.Logger().Error("failed to spawn ice peer: ", err)
				err := self.Tell(context.Background(), self, &shaker_ice.OStop{})
				if err != nil {
					self.Logger().Error(err)
				}
				return
			}
		}()
	case *shaker_ice.OOnCandidate:
		candidate, err := ice.UnmarshalCandidate(msg.Candidate)
		if err != nil {
			ctx.Logger().Error("failed to unmarshal candidate: ", err)
			return
		}
		if !UseCandidate(candidate) {
			ctx.Logger().Info("candidate filtered: ", candidate)
			return
		}
		ctx.Logger().Debug("adding ice candidate: ", candidate)
		err = s.agent.AddRemoteCandidate(candidate)
		if err != nil {
			ctx.Logger().Error("failed to add ice candidate: ", err)
			return
		}
	case *goaktpb.Mayday:
		ctx.Logger().Error("peer "+ctx.Sender().Name()+" failed: ", msg.GetMessage())
		ctx.Stop(ctx.Sender())
		ctx.Stop(ctx.Self())
	default:
		ctx.Unhandled()
	}
}
