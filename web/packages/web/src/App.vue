<template>
  <div class="main">
    <div class="config">
      <k-form v-model="config" :schema="Config" :initial="initial"></k-form>
    </div>
    <div class="run">
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
  tap_mac_prefix: number
  addresses: string[]
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
    tap_mac_prefix: Schema.number().required(),
    addresses: Schema.array(Schema.string()),
  }),
}).description('cryonet')

const config = ref<Config>({
  cryonet: {
    id: Math.round(Math.random() * 1000),
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
    enable_packet_information: false,
    tap_mac_prefix: 0x0200,
    addresses: ['10.114.0.2'],
  },
})

const initial = ref<Config>({
  cryonet: {
    id: Math.round(Math.random() * 1000),
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
    enable_packet_information: false,
    tap_mac_prefix: 0x0200,
    addresses: ['10.114.0.2'],
  },
})

const v86div = ref<HTMLDivElement>()
const file = ref<HTMLInputElement>()

let ran = false
async function run() {
  if (ran) return
  ran = true

  let resolve: () => void
  const promise = new Promise<void>(r => resolve = r)
  file.value!.onchange = resolve!
  file.value!.click()
  await promise

  const v86 = new V86({
    wasm_path: v86wasm,
    autostart: true,
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
  })

  function send(data: Uint8Array) {
    // @ts-ignore
    v86.bus.send('net0-receive', data)
  }

  let notify = (_data: Uint8Array) => {}
  v86.add_listener('net0-send', data => {
    notify(data)
  })

  function recv() {
    return new Promise<Uint8Array>(resolve => {
      notify = resolve
    })
  }

  const cryonet = await Cryonet.init(config.value.cryonet, send, recv)

  // @ts-ignore
  globalThis.cryonet = cryonet
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

.v86 {
  height: 1000px;
  width: 60%;
  background: black;
}

.config {
  width: 60%;
}
</style>
