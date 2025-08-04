package cryonet

import (
	"context"

	"github.com/coder/websocket"
	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/channels/ws"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type ShakerWS struct {
	conn   *websocket.Conn
	peerId string
}

func SpawnShakerWS(parent *goakt.PID, conn *websocket.Conn) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"shaker-ws-"+uuid.NewString(),
		&ShakerWS{
			conn: conn,
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*ShakerWS)(nil)

func (s *ShakerWS) PreStart(ctx *goakt.Context) error { return nil }

func (s *ShakerWS) PostStop(ctx *goakt.Context) error {
	s.conn.CloseNow()
	return nil
}

func (s *ShakerWS) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		peerId, err := s.shake(ctx)
		if err != nil {
			ctx.Err(err)
			return
		}
		s.peerId = peerId
		_, err = SpawnWSPeer(ctx.Self(), s.peerId, s.conn)
		if err != nil {
			ctx.Err(err)
			return
		}
	case *goaktpb.Mayday:
		ctx.Logger().Error(msg.GetMessage())
		ctx.Reinstate(ctx.Sender())
		ctx.Stop(ctx.Sender())
		go ctx.Stop(ctx.Self())
	default:
		ctx.Unhandled()
	}
}

func (s *ShakerWS) shake(ctx *goakt.ReceiveContext) (string, error) {
	context, cancel := context.WithTimeout(ctx.Context(), Config.ShakeTimeout)
	defer cancel()

	s.conn.SetReadLimit(-1)

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
		s.conn.Close(websocket.StatusInternalError, "failed to marshal init packet")
		return "", err
	}
	err = s.conn.Write(context, websocket.MessageBinary, data)
	if err != nil {
		s.conn.Close(websocket.StatusInternalError, "failed to send init packet")
		return "", err
	}

	// recv Init
	_, data, err = s.conn.Read(context)
	if err != nil {
		s.conn.Close(websocket.StatusInternalError, "failed to read init packet")
		return "", err
	}
	recvInit := &ws.Packet{}
	err = proto.Unmarshal(data, recvInit)
	if err != nil {
		s.conn.Close(websocket.StatusInternalError, "failed to unmarshal init packet")
		return "", err
	}
	if recvInit.GetInit() == nil {
		s.conn.Close(websocket.StatusProtocolError, "received invalid init packet")
		return "", err
	}
	if recvInit.GetInit().GetId() == Config.Id {
		s.conn.Close(websocket.StatusProtocolError, "received init packet with same ID")
		return "", err
	}
	if recvInit.GetInit().GetToken() != Config.Token {
		s.conn.Close(websocket.StatusProtocolError, "received init packet with invalid token")
		return "", err
	}

	return recvInit.GetInit().GetId(), nil
}
