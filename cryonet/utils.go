package cryonet

import (
	"crypto/rand"
	"net/netip"
	"os"
	"strings"

	"github.com/kagari-org/wireguard-go/device"
	"github.com/pion/ice/v4"
	"github.com/pion/stun/v3"
	"golang.org/x/crypto/curve25519"
	"golang.org/x/sys/unix"
)

func GetICEServers() []*stun.URI {
	ice_servers := []*stun.URI{}
	for _, server := range Config.IceServers {
		splited := strings.Split(server, "|")
		if len(splited) == 1 {
			server, err := stun.ParseURI(server)
			if err != nil {
				panic(err)
			}
			ice_servers = append(ice_servers, server)
		} else if len(splited) == 3 {
			server, err := stun.ParseURI(splited[0])
			if err != nil {
				panic(err)
			}
			server.Username = splited[1]
			server.Password = splited[2]
			ice_servers = append(ice_servers, server)
		} else {
			panic("Invalid ICE server format: " + server)
		}
	}
	return ice_servers
}

func UseCandidate(candidate ice.Candidate) bool {
	for _, item := range Config.FilteredPrefixes {
		prefix, err := netip.ParsePrefix(item)
		if err != nil {
			panic(err)
		}
		ip, err := netip.ParseAddr(candidate.Address())
		if err != nil {
			return false
		}
		if prefix.Contains(ip) {
			return false
		}
	}
	return true
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

func CreateTun(name string) (*os.File, error) {
	ifreq, err := unix.NewIfreq(name)
	if err != nil {
		return nil, err
	}
	flags := unix.IFF_TUN
	if !Config.EnablePacketInformation {
		flags |= unix.IFF_NO_PI
	}
	ifreq.SetUint16(uint16(flags))

	device, err := unix.Open("/dev/net/tun", unix.O_RDWR, 0)
	if err != nil {
		return nil, err
	}
	if err := unix.IoctlIfreq(device, unix.TUNSETIFF, ifreq); err != nil {
		unix.Close(device)
		return nil, err
	}

	// configure the interface
	socket, err := unix.Socket(unix.AF_INET, unix.SOCK_DGRAM, 0)
	if err != nil {
		unix.Close(device)
		return nil, err
	}
	defer unix.Close(socket)

	ifreq.SetUint16(unix.IFF_UP | unix.IFF_RUNNING)
	if err := unix.IoctlIfreq(socket, unix.SIOCSIFFLAGS, ifreq); err != nil {
		unix.Close(device)
		return nil, err
	}
	mtu := Config.BufSize
	if Config.EnablePacketInformation {
		mtu -= 4
	}
	ifreq.SetUint32(uint32(mtu))
	if err := unix.IoctlIfreq(socket, unix.SIOCSIFMTU, ifreq); err != nil {
		unix.Close(device)
		return nil, err
	}

	// https://github.com/vishvananda/netlink/blob/e1e260214862392fb28ff72c9b11adc84df73e2c/link_tuntap_linux.go#L77
	if err := unix.SetNonblock(device, true); err != nil {
		unix.Close(device)
		return nil, err
	}

	return os.NewFile(uintptr(device), "tun"), nil
}

func GenWGPrivkey() (device.NoisePrivateKey, error) {
	var sk device.NoisePrivateKey
	_, err := rand.Read(sk[:])
	sk[0] &= 248
	sk[31] = (sk[31] & 127) | 64
	return sk, err
}

func GenWGPubkey(sk *device.NoisePrivateKey) device.NoisePublicKey {
	var pk device.NoisePublicKey
	apk := (*[device.NoisePublicKeySize]byte)(&pk)
	ask := (*[device.NoisePrivateKeySize]byte)(sk)
	curve25519.ScalarBaseMult(apk, ask)
	return pk
}
