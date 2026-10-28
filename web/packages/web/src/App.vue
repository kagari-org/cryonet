<template>
  <div class="main">
    <div class="config" v-if="!ran">
      <k-form v-model="config" :schema="Config" :initial="initial"></k-form>
    </div>
    <div class="run" v-if="!ran">
      <el-button type="info" @click="run" size="large">run</el-button>
    </div>
    <div ref="v86div" class="v86"></div>
    <input type="file" ref="file" style="display: none;"/>
  </div>
</template>

<script setup lang="ts">
import { Schema } from 'schemastery-vue'
import { ref } from 'vue'
import { Cryonet } from 'cryonet-lib'
import { V86 } from 'v86'
import v86wasm from 'v86/build/v86.wasm?url'
import { FatFsDisk } from 'fatfs-wasm'

interface IceServer {
  url: string,
  username?: string,
  credential?: string
}

interface CryonetConfig {
  id: number
  token?: string
  servers: string[]
  ice_servers: IceServer[]
  enable_packet_information: boolean
  addresses: string[]
  ipv6_subnet?: string
  name?: string
}

interface Config {
  cryonet: CryonetConfig
}

const Config = Schema.object({
  cryonet: Schema.object({
    id: Schema.number().required(),
    token: Schema.string(),
    servers: Schema.array(Schema.string()),
    ice_servers: Schema.array(Schema.object({
      url: Schema.string().required(),
      username: Schema.string(),
      credential: Schema.string(),
    })),
    enable_packet_information: Schema.boolean().required(),
    addresses: Schema.array(Schema.string()),
    ipv6_subnet: Schema.string(),
    name: Schema.string(),
  }),
}).description('cryonet')

const id = (() => {
  const words = new Uint32Array(2)
  if (globalThis.crypto?.getRandomValues) {
    globalThis.crypto.getRandomValues(words)
  } else {
    words[0] = Math.random() * 0x100000000
    words[1] = Math.random() * 0x100000000
  }
  return ((words[0] & 0x1fffff) * 0x100000000 + words[1]) || 1
})()
const addressRand = (() => {
  const bytes = new Uint8Array(9)
  if (globalThis.crypto?.getRandomValues) {
    globalThis.crypto.getRandomValues(bytes)
  } else {
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = Math.floor(Math.random() * 256)
    }
  }
  return bytes
})()
const ipv4 = `10.114.0.${(addressRand[0] % 254) + 1}`
const ipv6 = `fd72::${Array.from(addressRand.slice(1), (byte) => byte.toString(16).padStart(2, '0')).join('')}`

const config = ref<Config>({
  cryonet: {
    id,
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
    enable_packet_information: false,
    addresses: [ipv4, ipv6],
    ipv6_subnet: '',
    name: '',
  },
})

const initial = ref<Config>({
  cryonet: {
    id,
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
    enable_packet_information: false,
    addresses: [ipv4, ipv6],
    ipv6_subnet: '',
  },
})

const v86div = ref<HTMLDivElement>()
const file = ref<HTMLInputElement>()

const ran = ref(false)
async function run() {
  if (ran.value) return
  ran.value = true

  let resolve: () => void
  const promise = new Promise<void>(r => resolve = r)
  file.value!.onchange = resolve!
  file.value!.click()
  await promise

  const macBytes = crypto.getRandomValues(new Uint8Array(6))
  macBytes[0] = (macBytes[0]! & 0xfc) | 0x02
  const mac = [...macBytes].map(x => x.toString(16).padStart(2, '0')).join(':')

  const v86 = new V86({
    wasm_path: v86wasm,
    autostart: true,
    memory_size: 1024 * 1024 * 1024, // 1GB
    screen: {
      container: v86div.value,
    },
    bios: {
      url: 'https://cdn.jsdelivr.net/gh/copy/v86/bios/seabios.bin',
    },
    vga_bios: {
      url: 'https://cdn.jsdelivr.net/gh/copy/v86/bios/vgabios.bin',
    },
    cdrom: {
      // 'https://cdn.jsdelivr.net/gh/copy/images/linux.iso'
      url: URL.createObjectURL(file.value!.files![0]!),
    },
    hda: {
      buffer: await createImage({ mac, ipv4, ipv6 }),
    },
    // boot_order: BootOrder.CD_HARDDISK_FLOPPY,
    boot_order: 0x123,
  })

  function send(data: Uint8Array) {
    // @ts-ignore
    v86.bus.send('net0-receive', data)
  }

  let notify = (_data: Uint8Array) => {}
  function recv() {
    return new Promise<Uint8Array>(resolve => {
      notify = resolve
    })
  }
  v86.add_listener('net0-send', data => {
    notify(data)
  })

  config.value.cryonet.addresses.push(macToLinkLocal(mac))
  const cryonet = await Cryonet.init({
    ...config.value.cryonet,
    mac: mac.split(':').map(x => parseInt(x, 16)),
  }, send, recv)

  // @ts-ignore
  globalThis.cryonet = cryonet
  // @ts-ignore
  globalThis.v86 = v86
}

async function createImage(info: any) {
  const image = new Uint8Array(1024 * 1024) // 1MB
  const disk = await FatFsDisk.create(image)
  disk.mkfs()
  disk.mount()
  disk.writeFile('/info', new TextEncoder().encode(JSON.stringify(info)))
  disk.unmount()
  return image.buffer
}

function macToLinkLocal(mac: string) {
  const parts = mac.split(':').map(x => parseInt(x, 16))
  const eui64 = [
    parts[0]! ^ 0b00000010,
    parts[1]!,
    parts[2]!,
    0xff,
    0xfe,
    parts[3]!,
    parts[4]!,
    parts[5]!,
  ]
  const groups = []
  for (let i = 0; i < 8; i += 2) {
    groups.push(((eui64[i]! << 8) | eui64[i + 1]!).toString(16).padStart(4, '0'))
  }
  return 'fe80::' + groups.join(':')
}
</script>

<style scoped>
.main {
  width: 100%;
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 20px;
}

.config {
  width: 60%;
}
</style>
