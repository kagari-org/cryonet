package utils

import (
	"fmt"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/cryonet"
	"github.com/kagari-org/cryonet/cryonet/peer"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"google.golang.org/protobuf/proto"
)

func WSShakeOrClose(ctx *goakt.ReceiveContext, conn *websocket.Conn) (*goakt.PID, error) {
	// send Init
	sendInit := &ws.Packet{
		P: &ws.Packet_Init{
			Init: &ws.Init{
				Id:    cryonet.Config.Id,
				Token: cryonet.Config.Token,
			},
		},
	}
	data, err := proto.Marshal(sendInit)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to marshal init packet")
		return nil, err
	}
	err = conn.Write(ctx.Context(), websocket.MessageBinary, data)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to send init packet")
		return nil, err
	}

	// recv Init
	_, data, err = conn.Read(ctx.Context())
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to read init packet")
		return nil, err
	}
	recvInit := &ws.Packet{}
	err = proto.Unmarshal(data, recvInit)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to unmarshal init packet")
		return nil, err
	}
	if recvInit.GetInit() == nil {
		conn.Close(websocket.StatusProtocolError, "received invalid init packet")
		return nil, err
	}
	if recvInit.GetInit().GetId() == cryonet.Config.Id {
		conn.Close(websocket.StatusProtocolError, "received init packet with same ID")
		return nil, err
	}
	if recvInit.GetInit().GetToken() != cryonet.Config.Token {
		conn.Close(websocket.StatusProtocolError, "received init packet with invalid token")
		return nil, err
	}

	// spawn peer
	id := recvInit.GetInit().GetId()
	ws := peer.NewWSPeer(id, conn)
	pid, err := ctx.ActorSystem().Spawn(ctx.Context(), fmt.Sprintf("ws-connect-%s", id), ws)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to spawn ws peer")
		return nil, err
	}

	return pid, nil
}
