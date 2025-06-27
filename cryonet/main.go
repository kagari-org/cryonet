package cryonet

import (
	"context"
	"strings"
	"time"

	"github.com/alecthomas/kong"
	"github.com/pion/webrtc/v4"
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

func GetICEServers() []webrtc.ICEServer {
	ice_servers := []webrtc.ICEServer{}
	for _, server := range Config.ice_servers {
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
