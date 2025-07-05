package cryonet

import (
	"os"

	goakt "github.com/tochemey/goakt/v3/actor"
	"golang.org/x/sys/unix"
)

func CreateTun(ctx *goakt.Context, name string) (*os.File, error) {
	ifreq, err := unix.NewIfreq(name)
	if err != nil {
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}
	flags := unix.IFF_TUN
	if !Config.EnablePacketInformation {
		flags |= unix.IFF_NO_PI
	}
	ifreq.SetUint16(uint16(flags))

	device, err := unix.Open("/dev/net/tun", unix.O_RDWR, 0)
	if err != nil {
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}
	if err := unix.IoctlIfreq(device, unix.TUNSETIFF, ifreq); err != nil {
		unix.Close(device)
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}

	// configure the interface
	socket, err := unix.Socket(unix.AF_INET, unix.SOCK_DGRAM, 0)
	if err != nil {
		unix.Close(device)
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}
	defer unix.Close(socket)

	ifreq.SetUint16(unix.IFF_UP | unix.IFF_RUNNING)
	if err := unix.IoctlIfreq(socket, unix.SIOCSIFFLAGS, ifreq); err != nil {
		unix.Close(device)
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}
	mtu := Config.BufSize
	if Config.EnablePacketInformation {
		mtu -= 4
	}
	ifreq.SetUint32(uint32(mtu))
	if err := unix.IoctlIfreq(socket, unix.SIOCSIFMTU, ifreq); err != nil {
		unix.Close(device)
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}

	// https://github.com/vishvananda/netlink/blob/e1e260214862392fb28ff72c9b11adc84df73e2c/link_tuntap_linux.go#L77
	if err := unix.SetNonblock(device, true); err != nil {
		unix.Close(device)
		ctx.ActorSystem().Logger().Error(err)
		return nil, err
	}

	return os.NewFile(uintptr(device), "tun"), nil
}
