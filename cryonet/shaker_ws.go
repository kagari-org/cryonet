package cryonet

import (
	"context"
	"errors"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/kagari-org/cryonet/gen/channel"
	goakt "github.com/tochemey/goakt/v3/actor"
	gerrors "github.com/tochemey/goakt/v3/errors"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type ShakerWS struct {
	conn   *websocket.Conn
	peerId string
}

var peerLock sync.Mutex

func SpawnShakerWS(parent *goakt.PID, suffix string, conn *websocket.Conn) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"shaker-"+suffix,
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
		peerLock.Lock()
		defer peerLock.Unlock()
		_, _, err = ctx.ActorSystem().ActorOf(ctx.Context(), "peer-ws-"+peerId)
		if err == nil {
			s.conn.Close(websocket.StatusProtocolError, "duplicate connection")
			peerLock.Unlock()
			// avoid respawning too fast
			time.Sleep(Config.ShakeTimeout)
			ctx.Stop(ctx.Self())
			return
		}
		if !errors.Is(err, gerrors.ErrActorNotFound) {
			ctx.Err(err)
			return
		}
		if !errors.Is(err, gerrors.ErrActorNotFound) {

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
		ctx.Stop(ctx.Sender())
		ctx.Stop(ctx.Self())
	default:
		ctx.Unhandled()
	}
}

func (s *ShakerWS) shake(ctx *goakt.ReceiveContext) (string, error) {
	context, cancel := context.WithTimeout(ctx.Context(), Config.ShakeTimeout)
	defer cancel()

	s.conn.SetReadLimit(-1)

	// send Init
	sendInit := &channel.Init{
		Id:    Config.Id,
		Token: Config.Token,
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
	recvInit := &channel.Init{}
	err = proto.Unmarshal(data, recvInit)
	if err != nil {
		s.conn.Close(websocket.StatusInternalError, "failed to unmarshal init packet")
		return "", err
	}
	if recvInit.Id == Config.Id {
		s.conn.Close(websocket.StatusProtocolError, "received init packet with same ID")
		return "", err
	}
	if recvInit.Token != Config.Token {
		s.conn.Close(websocket.StatusProtocolError, "received init packet with invalid token")
		return "", err
	}

	return recvInit.Id, nil
}
