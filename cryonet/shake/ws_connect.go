package shake

import (
	"encoding/json"
	"errors"

	"github.com/coder/websocket"
	"github.com/emirpasic/gods/v2/sets/hashset"
	"github.com/kagari-org/cryonet/cryonet"
	"github.com/kagari-org/cryonet/gen/shake/ws_connect"
	"github.com/kagari-org/cryonet/gen/shake/ws_packet"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"google.golang.org/protobuf/proto"
)

type WSConnect struct {
	connecting *hashset.Set[string]
}

var _ goakt.Actor = (*WSConnect)(nil)

func NewWSConnect() *WSConnect {
	return &WSConnect{
		connecting: hashset.New[string](),
	}
}

func (w *WSConnect) PreStart(ctx *goakt.Context) error {
	return nil
}

func (w *WSConnect) PostStop(ctx *goakt.Context) error {
	return nil
}

func (w *WSConnect) Receive(ctx *goakt.ReceiveContext) {
	switch ctx.Message().(type) {
	case *ws_connect.Connect:
		servers := hashset.New(cryonet.Config.WSServers...)
		diff := servers.Difference(w.connecting)
		for _, server := range diff.Values() {
			go connect(ctx, server)
		}
	case *ws_connect.Connected:
	default:
		ctx.Unhandled()
	}
}

func connect(ctx *goakt.ReceiveContext, endpoint string) error {
	ws, _, err := websocket.Dial(ctx.Context(), endpoint, nil)
	if err != nil {
		return err
	}
	defer ws.Close(websocket.StatusNormalClosure, "normal closure")

	// send Init
	sendInit := ws_packet.Init{
		Id:    cryonet.Config.Id,
		Token: cryonet.Config.Token,
	}
	data, err := proto.Marshal(&sendInit)
	if err != nil {
		return err
	}
	err = ws.Write(ctx.Context(), websocket.MessageBinary, data)
	if err != nil {
		return err
	}

	// recv Init
	_, data, err = ws.Read(ctx.Context())
	if err != nil {
		return err
	}
	recvInit := &ws_packet.Init{}
	err = proto.Unmarshal(data, recvInit)
	if err != nil {
		return err
	}
	if recvInit.GetId() == cryonet.Config.Id {
		return errors.New("same id in network")
	}
	if recvInit.GetToken() != cryonet.Config.Token {
		return errors.New("invalid token")
	}

	// send Offer
	peer, err := webrtc.NewPeerConnection(webrtc.Configuration{
		ICEServers: cryonet.GetICEServers(),
	})
	if err != nil {
		return err
	}
	offer, err := peer.CreateOffer(nil)
	if err != nil {
		return err
	}
	gather := webrtc.GatheringCompletePromise(peer)
	err = peer.SetLocalDescription(offer)
	if err != nil {
		return err
	}
	<-gather
	offerData, err := json.Marshal(peer.LocalDescription())
	if err != nil {
		return err
	}
	sendOffer := ws_packet.Offer{
		Description: offerData,
	}
	data, err = proto.Marshal(&sendOffer)
	if err != nil {
		return err
	}
	err = ws.Write(ctx.Context(), websocket.MessageBinary, data)
	if err != nil {
		return err
	}

	// recv answer
	_, data, err = ws.Read(ctx.Context())
	if err != nil {
		return err
	}
	recvAnswer := &ws_packet.Answer{}
	err = proto.Unmarshal(data, recvAnswer)
	if err != nil {
		return nil
	}
	answerData := &webrtc.SessionDescription{}
	err = json.Unmarshal(recvAnswer.GetDescription(), answerData)
	if err != nil {
		return err
	}
	err = peer.SetRemoteDescription(*answerData)
	if err != nil {
		return nil
	}

	return nil
}
