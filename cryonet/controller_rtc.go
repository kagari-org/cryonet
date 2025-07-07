package cryonet

import (
	"encoding/json"
	"errors"
	"sync"

	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/controller_rtc"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/types/known/anypb"
)

type RTC struct {
	peers map[string]*RTCPeer
}

type RTCPeer struct {
	desc chan *common.Desc

	lock   sync.Mutex
	peerId string
	pid    *goakt.PID
}

func NewRTC() *RTC {
	return &RTC{
		peers: make(map[string]*RTCPeer),
	}
}

var _ goakt.Actor = (*RTC)(nil)

func (r *RTC) PreStart(ctx *goakt.Context) error {
	return nil
}

func (r *RTC) PostStop(ctx *goakt.Context) error {
	for _, p := range r.peers {
		close(p.desc)
	}
	// children will be stopped automatically
	return nil
}

func (r *RTC) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *controller_rtc.Alive:
		for _, peerId := range msg.Alive.GetPeers() {
			if _, ok := r.peers[peerId]; !ok {
				r.peers[peerId] = &RTCPeer{
					desc: make(chan *common.Desc),

					lock:   sync.Mutex{},
					peerId: peerId,
					pid:    nil,
				}
			}
			p := r.peers[peerId]
			go r.shake(ctx, p)
		}
	case *controller_rtc.Desc:
		for _, peer := range r.peers {
			select {
			case peer.desc <- msg.GetDesc():
			default:
				// discard desc if receiver is not receiving
			}
		}
	case *controller_rtc.EmitDesc:
		desc, err := anypb.New(&peer.CastDesc{
			Desc: msg.GetDesc(),
		})
		if err != nil {
			ctx.Err(err)
			return
		}
		ctx.Tell(ctx.ActorSystem().TopicActor(), &goaktpb.Publish{
			Id:      uuid.NewString(),
			Topic:   "peers",
			Message: desc,
		})
	case *controller.GetPeers:
		peers := make([]string, 0)
		for _, peer := range r.peers {
			if peer.pid != nil && peer.pid.IsRunning() {
				peers = append(peers, peer.peerId)
			}
		}
		ctx.Response(&controller.GetPeersResponse{
			Peers: peers,
		})
	default:
		ctx.Unhandled()
	}
}

func (r *RTC) shake(ctx *goakt.ReceiveContext, p *RTCPeer) error {
	logger := ctx.Logger()

	if !p.lock.TryLock() {
		return nil
	}
	defer p.lock.Unlock()

	if p.pid != nil && p.pid.IsRunning() {
		// already connected
		return nil
	}

	shakeFailed := make(chan struct{}, 1)
	master := IsMaster(p.peerId)

	peer, err := webrtc.NewPeerConnection(webrtc.Configuration{
		ICEServers: GetICEServers(),
	})
	if err != nil {
		logger.Error(err)
		return err
	}
	peer.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
			shakeFailed <- struct{}{}
		}
	})
	defer peer.OnConnectionStateChange(nil)

	var dc *webrtc.DataChannel
	if master {
		descId := uuid.NewString()

		channel, err := peer.CreateDataChannel("data", nil)
		if err != nil {
			logger.Error(err)
			peer.Close()
			return err
		}
		dc = channel

		offer, err := peer.CreateOffer(nil)
		if err != nil {
			logger.Error(err)
			peer.Close()
			return err
		}
		gather := webrtc.GatheringCompletePromise(peer)
		err = peer.SetLocalDescription(offer)
		if err != nil {
			logger.Error(err)
			peer.Close()
			return err
		}
		<-gather
		offer = *peer.LocalDescription()
		offerBytes, err := json.Marshal(offer)
		if err != nil {
			logger.Error(err)
			peer.Close()
			return err
		}

		ctx.ActorSystem().Schedule(ctx.Context(), &controller_rtc.EmitDesc{
			Desc: &common.Desc{
				From:   Config.Id,
				To:     p.peerId,
				DescId: descId,
				Desc:   offerBytes,
			},
		}, ctx.Self(), Config.CheckInterval, goakt.WithReference(descId))
		defer ctx.ActorSystem().CancelSchedule(descId)

	loopMaster:
		for {
			select {
			case <-shakeFailed:
				err := errors.New("peer connection failed")
				logger.Error(err)
				peer.Close()
				return err
			case answer, ok := <-p.desc:
				if !ok {
					err := errors.New("desc channel closed")
					logger.Error(err)
					peer.Close()
					return err
				}
				if !(answer.From == p.peerId && answer.To == Config.Id) {
					continue
				}
				if answer.DescId != descId {
					// ignore old desc
					continue
				}
				ctx.ActorSystem().CancelSchedule(descId)
				answerDesc := &webrtc.SessionDescription{}
				err := json.Unmarshal(answer.Desc, answerDesc)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}
				err = peer.SetRemoteDescription(*answerDesc)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}
				break loopMaster
			}
		}
	} else {
	loopSlave:
		for {
			select {
			case <-shakeFailed:
				err := errors.New("peer connection failed")
				logger.Error(err)
				peer.Close()
				return err
			case offer, ok := <-p.desc:
				if !ok {
					err := errors.New("desc channel closed")
					logger.Error(err)
					peer.Close()
					return err
				}
				if !(offer.From == p.peerId && offer.To == Config.Id) {
					continue
				}

				channelChan := make(chan *webrtc.DataChannel, 1)
				peer.OnDataChannel(func(d *webrtc.DataChannel) {
					channelChan <- d
				})
				defer peer.OnDataChannel(nil)

				offerDesc := &webrtc.SessionDescription{}
				err := json.Unmarshal(offer.Desc, offerDesc)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}
				err = peer.SetRemoteDescription(*offerDesc)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}

				answer, err := peer.CreateAnswer(nil)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}
				gather := webrtc.GatheringCompletePromise(peer)
				err = peer.SetLocalDescription(answer)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}
				<-gather
				answerDesc := *peer.LocalDescription()

				answerBytes, err := json.Marshal(answerDesc)
				if err != nil {
					logger.Error(err)
					peer.Close()
					return err
				}
				ctx.ActorSystem().Schedule(ctx.Context(), &controller_rtc.EmitDesc{
					Desc: &common.Desc{
						From:   Config.Id,
						To:     p.peerId,
						DescId: offer.DescId,
						Desc:   answerBytes,
					},
				}, ctx.Self(), Config.CheckInterval, goakt.WithReference(offer.DescId))
				defer ctx.ActorSystem().CancelSchedule(offer.DescId)

				select {
				case <-shakeFailed:
					err := errors.New("peer connection failed")
					logger.Error(err)
					peer.Close()
					return err
				case channel, ok := <-channelChan:
					if !ok {
						err := errors.New("channelChan closed")
						logger.Error(err)
						peer.Close()
						return err
					}
					dc = channel
					break loopSlave
				}
			}
		}
	}

	p.pid = ctx.Spawn("rtc-peer-"+p.peerId, NewRTCPeer(p.peerId, peer, dc), goakt.WithLongLived())

	return nil
}
