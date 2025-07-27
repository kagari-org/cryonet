package cryonet

import (
	"strings"

	"github.com/pion/webrtc/v4"
)

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
