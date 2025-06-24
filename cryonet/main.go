package cryonet

import (
	"context"
	"time"

	"github.com/alecthomas/kong"
	goakt "github.com/tochemey/goakt/v3/actor"
)

var Config struct {
	Id string `arg:""`

	Listen    string   `env:"LISTEN" default:"127.0.0.1:2333"`
	WSServers []string `env:"WS_SERVERS"`
	Token     string   `env:"TOKEN"`

	ice_servers []string `env:"ICE_SERVERS" default:"stun:stun.l.google.com"`

	CheckInterval     time.Duration `env:"CHECK_INTERVAL" default:"10s"`
	SendAliveInterval time.Duration `env:"SEND_ALIVE_INTERVAL" default:"1m"`

	InterfacePrefix         string `env:"INTERFACE_PREFIX" default:"cn"`
	AutoInterfaceName       bool   `env:"AUTO_INTERFACE_NAME" default:"false"`
	EnablePacketInformation bool   `env:"ENABLE_PACKET_INFORMATION" default:"true"`
	BufSize                 int    `env:"BUF_SIZE" default:"1504"`
}

func Main() {
	kong.Parse(&Config)

	ctx := context.Background()

	system, err := goakt.NewActorSystem("HelloWorldSystem")

	if err != nil {
		panic(err)
	}

	if err := system.Start(ctx); err != nil {
		panic(err)
	}
}
