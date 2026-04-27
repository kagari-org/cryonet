<template>
  <div class="main">
    <div class="config">
      <k-form v-model="config" :schema="Config" :initial="initial"></k-form>
    </div>
    <div class="run">
      <el-button type="info" @click="run" size="large">run</el-button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { Schema } from 'schemastery-vue'
import { ref } from 'vue'
import { Cryonet } from 'cryonet-lib'

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
}

interface Config {
  cryonet: CryonetConfig
}

const Config = Schema.object({
  ip: Schema.string().required(),
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
  }),
}).description('cryonet')

const config = ref<Config>({
  cryonet: {
    id: Math.round(Math.random() * 1000),
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
    enable_packet_information: false,
    tap_mac_prefix: 0x0200,
  },
})

const initial = ref<Config>({
  cryonet: {
    id: Math.round(Math.random() * 1000),
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
    enable_packet_information: false,
    tap_mac_prefix: 0x0200,
  },
})

let ran = false
async function run() {
  if (ran) return
  ran = true
  await Cryonet.init(config.value.cryonet, (buf: Uint8Array) => {
    console.log('recv', buf)
  }, async () => {
    await new Promise(resolve => {})
  })
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
