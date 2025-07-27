package cryonet

import (
	"context"
	"os"
	"os/signal"
	"time"

	"github.com/alecthomas/kong"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"github.com/tochemey/goakt/v3/log"
)

var Config struct {
	Id string `arg:""`

	Listen    string   `env:"LISTEN" default:"127.0.0.1:2333"`
	WSServers []string `env:"WS_SERVERS"`
	Token     string   `env:"TOKEN"`

	IceServers []string `env:"ICE_SERVERS" default:"stun:stun.l.google.com"`

	CheckInterval     time.Duration `env:"CHECK_INTERVAL" default:"10s"`
	SendAliveInterval time.Duration `env:"SEND_ALIVE_INTERVAL" default:"1m"`
	ShakeTimeout      time.Duration `env:"SHAKE_TIMEOUT" default:"5m"`

	InterfacePrefixWS       string `env:"INTERFACE_PREFIX" default:"cnw"`
	InterfacePrefixRTC      string `env:"INTERFACE_PREFIX" default:"cnr"`
	EnablePacketInformation bool   `env:"ENABLE_PACKET_INFORMATION" default:"true"`
	BufSize                 int    `env:"BUF_SIZE" default:"1504"`
}

type Cryonet struct{}

var _ goakt.Actor = (*Cryonet)(nil)

func (c *Cryonet) PreStart(ctx *goakt.Context) error { return nil }
func (c *Cryonet) PostStop(ctx *goakt.Context) error { return nil }

func (c *Cryonet) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		_, err := SpawnController(ctx.Self())
		if err != nil {
			ctx.Err(err)
			return
		}
	case *goaktpb.Mayday:
		ctx.Logger().Error(msg.GetMessage())
		ctx.Stop(ctx.Sender())
		ctx.ActorSystem().Stop(context.Background())
		os.Exit(1)
	default:
		ctx.Unhandled()
	}
}

func Main() {
	kong.Parse(&Config)

	ctx := context.Background()

	system, err := goakt.NewActorSystem(
		"CryonetSystem",
		goakt.WithLogger(log.New(log.DebugLevel, os.Stderr)),
	)
	if err != nil {
		system.Logger().Fatal(err)
		return
	}
	defer system.Stop(ctx)

	if err := system.Start(ctx); err != nil {
		system.Logger().Fatal(err)
		return
	}

	_, err = system.Spawn(ctx, "cryonet", &Cryonet{}, goakt.WithLongLived())
	if err != nil {
		system.Logger().Fatal(err)
		return
	}

	system.Logger().Info("Cryonet started.")

	sigint := make(chan os.Signal, 1)
	signal.Notify(sigint, os.Interrupt)
	<-sigint
}
