package cryonet

import (
	"fmt"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"google.golang.org/protobuf/proto"
)

func WSShakeOrClose(ctx *goakt.ReceiveContext, conn *websocket.Conn) (*goakt.PID, error) {
	// send Init
	sendInit := &ws.Packet{
		P: &ws.Packet_Init{
			Init: &ws.Init{
				Id:    Config.Id,
				Token: Config.Token,
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
	if recvInit.GetInit().GetId() == Config.Id {
		conn.Close(websocket.StatusProtocolError, "received init packet with same ID")
		return nil, err
	}
	if recvInit.GetInit().GetToken() != Config.Token {
		conn.Close(websocket.StatusProtocolError, "received init packet with invalid token")
		return nil, err
	}

	// spawn peer
	id := recvInit.GetInit().GetId()
	ws := NewWSPeer(id, conn)
	pid, err := ctx.ActorSystem().Spawn(ctx.Context(), fmt.Sprintf("ws-peer-%s", id), ws, goakt.WithLongLived())
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to spawn ws peer")
		return nil, err
	}

	return pid, nil
}
