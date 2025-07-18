package cryonet

import (
	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"google.golang.org/protobuf/proto"
)

func WSShakeOrClose(ctx *goakt.ReceiveContext, conn *websocket.Conn) (string, *websocket.Conn, error) {
	logger := ctx.Logger()

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
		logger.Error(err)
		return "", nil, err
	}
	err = conn.Write(ctx.Context(), websocket.MessageBinary, data)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to send init packet")
		logger.Error(err)
		return "", nil, err
	}

	// recv Init
	_, data, err = conn.Read(ctx.Context())
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to read init packet")
		logger.Error(err)
		return "", nil, err
	}
	recvInit := &ws.Packet{}
	err = proto.Unmarshal(data, recvInit)
	if err != nil {
		conn.Close(websocket.StatusInternalError, "failed to unmarshal init packet")
		logger.Error(err)
		return "", nil, err
	}
	if recvInit.GetInit() == nil {
		conn.Close(websocket.StatusProtocolError, "received invalid init packet")
		logger.Error(err)
		return "", nil, err
	}
	if recvInit.GetInit().GetId() == Config.Id {
		conn.Close(websocket.StatusProtocolError, "received init packet with same ID")
		logger.Error(err)
		return "", nil, err
	}
	if recvInit.GetInit().GetToken() != Config.Token {
		conn.Close(websocket.StatusProtocolError, "received init packet with invalid token")
		logger.Error(err)
		return "", nil, err
	}

	// pid := ctx.Spawn(fmt.Sprintf("ws-peer-%s", peerId), NewWSPeer(peerId, conn), goakt.WithLongLived())

	return recvInit.GetInit().GetId(), conn, nil
}

func IsMaster(peerId string) bool {
	if len(Config.Id) > len(peerId) {
		return true
	}
	if len(Config.Id) < len(peerId) {
		return false
	}
	for i := 0; i < len(Config.Id); i++ {
		if Config.Id[i] < peerId[i] {
			return false
		} else if Config.Id[i] > peerId[i] {
			return true
		}
	}
	panic("id should not be equal")
}
