package cryonet

import (
	"context"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"os"
	"strings"
	"time"

	"github.com/kagari-org/cryonet/gen/actors/peer"
	"github.com/kagari-org/cryonet/gen/actors/router"
	"github.com/kagari-org/cryonet/gen/channel"
	"github.com/kagari-org/wireguard-go/conn"
	"github.com/kagari-org/wireguard-go/device"
	"github.com/kagari-org/wireguard-go/tun"
	"github.com/pion/ice/v4"
	goakt "github.com/tochemey/goakt/v3/actor"
	"github.com/tochemey/goakt/v3/goaktpb"
	"google.golang.org/protobuf/proto"
)

type PeerRTC struct {
	self *goakt.PID

	peerId string
	conn   *ice.Conn
	sk     device.NoisePrivateKey
	pk     device.NoisePublicKey
	tun    *os.File

	wgDev *device.Device
	user  chan []byte
}

func SpawnRTCPeer(
	parent *goakt.PID,
	peerId string,
	conn *ice.Conn,

	sk device.NoisePrivateKey,
	pk device.NoisePublicKey,
) (*goakt.PID, error) {
	return parent.SpawnChild(
		context.Background(),
		"peer-rtc-"+peerId,
		&PeerRTC{
			peerId: peerId,
			conn:   conn,
			user:   make(chan []byte, 16),
			sk:     sk,
			pk:     pk,
		},
		goakt.WithLongLived(),
		goakt.WithSupervisor(goakt.NewSupervisor(
			goakt.WithAnyErrorDirective(goakt.EscalateDirective),
		)),
	)
}

var _ goakt.Actor = (*PeerRTC)(nil)
var _ tun.Device = (*PeerRTC)(nil)
var _ conn.Bind = (*PeerRTC)(nil)

func (p *PeerRTC) PreStart(ctx *goakt.Context) error { return nil }

func (p *PeerRTC) PostStop(ctx *goakt.Context) error {
	if p.conn != nil {
		p.conn.Close()
	}
	if p.tun != nil {
		p.tun.Close()
	}
	if p.wgDev != nil {
		p.wgDev.Close()
	}
	return nil
}

func (p *PeerRTC) Receive(ctx *goakt.ReceiveContext) {
	switch msg := ctx.Message().(type) {
	case *goaktpb.PostStart:
		p.self = ctx.Self()

		tun, err := CreateTun(Config.InterfacePrefixRTC + p.peerId)
		if err != nil {
			ctx.Err(err)
			return
		}
		p.tun = tun

		wgDev := device.NewDevice(p, p, device.NewLogger(device.LogLevelError, p.peerId))
		p.wgDev = wgDev
		err = wgDev.Up()
		if err != nil {
			ctx.Err(err)
			return
		}
		op := []string{
			"private_key=" + hex.EncodeToString(p.sk[:]),
			"public_key=" + hex.EncodeToString(p.pk[:]),
			"endpoint=0",
		}
		err = wgDev.IpcSet(strings.Join(op, "\n"))
		if err != nil {
			ctx.Err(err)
			return
		}
	case *peer.IRecvPacket:
		packet := &channel.Packet{}
		err := proto.Unmarshal(msg.Packet, packet)
		if err != nil {
			ctx.Err(err)
			return
		}
		_, rtr, err := ctx.ActorSystem().ActorOf(ctx.Context(), "router")
		if err != nil {
			ctx.Err(err)
			return
		}
		ctx.Tell(rtr, &router.ORecvPacket{
			Packet: packet,
		})
	case *peer.OStop:
		ctx.Err(errors.New("stop peer"))
	case *peer.OSendPacket:
		if len(msg.Packet) > Config.BufSize {
			ctx.Err(errors.New("packet too large"))
			return
		}
		p.user <- msg.Packet
	default:
		ctx.Unhandled()
	}
}

// tun.Device implementation
func (p *PeerRTC) BatchSize() int           { return 1 }
func (p *PeerRTC) Close() error             { return nil }
func (p *PeerRTC) Events() <-chan tun.Event { return nil }
func (p *PeerRTC) File() *os.File           { return p.tun }
func (p *PeerRTC) MTU() (int, error)        { return Config.BufSize, nil }
func (p *PeerRTC) Name() (string, error)    { return p.peerId, nil }

func (p *PeerRTC) Read(bufs [][]byte, sizes []int, users []bool, offset int) (int, error) {
	select {
	case user := <-p.user:
		binary.LittleEndian.PutUint32(bufs[0][offset:offset+4], uint32(len(user)))
		copy(bufs[0][offset+4:], user)
		sizes[0] = len(user) + 4
		users[0] = true
		return 1, nil
	default:
	}

	err := p.tun.SetReadDeadline(time.Now().Add((time.Millisecond * 500)))
	if err != nil {
		panic(err)
	}
	n, err := p.tun.Read(bufs[0][offset:])
	if errors.Is(err, os.ErrDeadlineExceeded) {
		return 0, nil
	}
	if err != nil {
		return 0, err
	}
	sizes[0] = n
	users[0] = false
	return 1, nil
}

func (p *PeerRTC) Write(bufs [][]byte, users []bool, offset int) (int, error) {
	sent := 0
	var e error
	for i, buf := range bufs {
		if users[i] {
			len := binary.LittleEndian.Uint32(buf[offset : offset+4])
			err := p.self.Tell(context.Background(), p.self, &peer.IRecvPacket{
				Packet: buf[offset+4 : offset+4+int(len)],
			})
			if err != nil {
				if e == nil {
					e = err
				} else {
					e = errors.Join(e, err)
				}
				continue
			}
			sent++
		} else {
			_, err := p.tun.Write(buf[offset:])
			if err != nil {
				if e == nil {
					e = err
				} else {
					e = errors.Join(e, err)
				}
				continue
			}
			sent++
		}
	}
	return sent, e
}

// conn.Bind implementation
func (p *PeerRTC) SetMark(mark uint32) error                     { return nil }
func (p *PeerRTC) ParseEndpoint(s string) (conn.Endpoint, error) { return &conn.StdNetEndpoint{}, nil }

func (p *PeerRTC) Open(port uint16) (fns []conn.ReceiveFunc, actualPort uint16, err error) {
	f := func(packets [][]byte, sizes []int, eps []conn.Endpoint) (n int, err error) {
		rn, rerr := p.conn.Read(packets[0])
		if rerr != nil {
			return 0, rerr
		}
		sizes[0] = rn
		eps[0] = &conn.StdNetEndpoint{}
		return 1, nil
	}
	return []conn.ReceiveFunc{f}, 0, nil
}

func (p *PeerRTC) Send(bufs [][]byte, ep conn.Endpoint) error {
	var e error
	for _, buf := range bufs {
		_, err := p.conn.Write(buf)
		if err != nil {
			if e == nil {
				e = err
			} else {
				e = errors.Join(e, err)
			}
		}
	}
	return e
}
