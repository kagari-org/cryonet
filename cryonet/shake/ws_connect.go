package shake

import (
	"github.com/coder/websocket"
	mapset "github.com/deckarep/golang-set/v2"
	"github.com/kagari-org/cryonet/cryonet"
	"github.com/kagari-org/cryonet/gen/shake/ws_connect"
	goakt "github.com/tochemey/goakt/v3/actor"
)

type WSConnect struct {
	connecting mapset.Set[string]
}

var _ goakt.Actor = (*WSConnect)(nil)

func NewWSConnect() *WSConnect {
	return &WSConnect{
		connecting: mapset.NewSet[string](),
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
		servers := mapset.NewSet(cryonet.Config.WSServers...)
		diff := servers.Difference(w.connecting)
		for server := range diff.Iter() {
			go connect(ctx, server)
		}
	case *ws_connect.Connected:
	default:
		ctx.Unhandled()
	}
}

func connect(ctx *goakt.ReceiveContext, endpoint string) error {
	_, _, err := websocket.Dial(ctx.Context(), endpoint, nil)
	if err != nil {
		return err
	}
	return nil
}
