package shake

import (
	goakt "github.com/tochemey/goakt/v3/actor"
)

type WSConnect struct{}

var _ goakt.Actor = (*WSConnect)(nil)

func NewWSConnect() *WSConnect {
	return &WSConnect{}
}

func (w *WSConnect) PreStart(ctx *goakt.Context) error {
	return nil
}

func (w *WSConnect) PostStop(ctx *goakt.Context) error {
	return nil
}

func (w *WSConnect) Receive(ctx *goakt.ReceiveContext) {
}
