package cryonet

import (
	"context"
	"os"
	"os/signal"
	"strings"
	"time"

	"github.com/alecthomas/kong"
	"github.com/google/uuid"
	"github.com/kagari-org/cryonet/gen/actors/controller"
	"github.com/kagari-org/cryonet/gen/actors/controller_rtc"
	"github.com/kagari-org/cryonet/gen/actors/cryonet"
	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/channels/common"
	"github.com/pion/webrtc/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"github.com/tochemey/goakt/v3/log"
	"google.golang.org/protobuf/types/known/anypb"
)

var Config struct {
	Id string `arg:""`

	Listen    string   `env:"LISTEN" default:"127.0.0.1:2333"`
	WSServers []string `env:"WS_SERVERS"`
	Token     string   `env:"TOKEN"`

	IceServers []string `env:"ICE_SERVERS" default:"stun:stun.l.google.com"`

	CheckInterval     time.Duration `env:"CHECK_INTERVAL" default:"10s"`
	SendAliveInterval time.Duration `env:"SEND_ALIVE_INTERVAL" default:"1m"`

	InterfacePrefixWS       string `env:"INTERFACE_PREFIX" default:"cnw"`
	InterfacePrefixRTC      string `env:"INTERFACE_PREFIX" default:"cnr"`
	EnablePacketInformation bool   `env:"ENABLE_PACKET_INFORMATION" default:"true"`
	BufSize                 int    `env:"BUF_SIZE" default:"1504"`
}

type Cryonet struct {
	wsListen  *goakt.PID
	wsConnect *goakt.PID
	rtc       *goakt.PID
}

func NewCryonet() *Cryonet {
	return &Cryonet{}
}

var _ goakt.Actor = (*Cryonet)(nil)

func (c *Cryonet) PreStart(ctx *goakt.Context) error {
	return nil
}

func (c *Cryonet) PostStop(ctx *goakt.Context) error {
	return nil
}

func (c *Cryonet) Receive(ctx *goakt.ReceiveContext) {
	logger := ctx.Logger()

	switch ctx.Message().(type) {
	case *goaktpb.PostStart:
		c.wsListen = ctx.Spawn("ws-listen", NewWSListen(), goakt.WithLongLived())
		c.wsConnect = ctx.Spawn("ws-connect", NewWSConnect(), goakt.WithLongLived())
		c.rtc = ctx.Spawn("rtc", NewRTC(), goakt.WithLongLived())
		ctx.ActorSystem().Schedule(ctx.Context(), &cryonet.SendAlive{}, ctx.Self(), Config.SendAliveInterval)
	case *cryonet.SendAlive:
		peers1 := ctx.Ask(c.wsListen, &controller.GetPeers{}, time.Second*5).(*controller.GetPeersResponse)
		peers2 := ctx.Ask(c.wsConnect, &controller.GetPeers{}, time.Second*5).(*controller.GetPeersResponse)
		peers3 := ctx.Ask(c.rtc, &controller.GetPeers{}, time.Second*5).(*controller.GetPeersResponse)
		peers := append(peers1.Peers, peers2.Peers...)
		peers = append(peers, peers3.Peers...)
		alive := &common.Alive{
			Id:    Config.Id,
			Peers: peers,
		}
		aliveAny, err := anypb.New(&peer.CastAlive{Alive: alive})
		if err != nil {
			logger.Error(err)
			return
		}
		logger.Info("Sending alive messages: ", alive)
		ctx.Tell(ctx.ActorSystem().TopicActor(), &goaktpb.Publish{
			Id:      uuid.NewString(),
			Topic:   "peers",
			Message: aliveAny,
		})
		// send alive to self, so that it will create rtc from ws peer
		ctx.Tell(c.rtc, &controller_rtc.Alive{Alive: alive})
	default:
		ctx.Unhandled()
	}
}

func Main() {
	kong.Parse(&Config)

	ctx := context.Background()

	system, err := goakt.NewActorSystem("CryonetSystem",
		goakt.WithLogger(log.New(log.DebugLevel, os.Stderr)),
		goakt.WithPubSub())
	if err != nil {
		system.Logger().Fatal(err)
		return
	}
	defer system.Stop(ctx)

	if err := system.Start(ctx); err != nil {
		system.Logger().Fatal(err)
		return
	}

	_, err = system.Spawn(ctx, "cryonet", NewCryonet(), goakt.WithLongLived())
	if err != nil {
		system.Logger().Fatal(err)
		return
	}

	system.Logger().Info("Cryonet started.")

	sigint := make(chan os.Signal, 1)
	signal.Notify(sigint, os.Interrupt)
	<-sigint
}

func GetICEServers() []webrtc.ICEServer {
	ice_servers := []webrtc.ICEServer{}
	for _, server := range Config.IceServers {
		splited := strings.Split(server, "|")
		if len(splited) == 1 {
			ice_servers = append(ice_servers, webrtc.ICEServer{
				URLs: []string{server},
			})
		} else if len(splited) == 3 {
			ice_servers = append(ice_servers, webrtc.ICEServer{
				URLs:           []string{splited[0]},
				Username:       splited[1],
				Credential:     splited[2],
				CredentialType: webrtc.ICECredentialTypePassword,
			})
		} else {
			panic("Invalid ICE server format: " + server)
		}
	}
	return ice_servers
}
