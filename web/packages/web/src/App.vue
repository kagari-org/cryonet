<template>
  <el-container>
    <el-main>
      <el-row justify="center" align="middle">
        <el-col :span="10" justify="center" align="middle">
          <k-form v-model="config" :schema="Config" :initial="initial"></k-form>
        </el-col>
        <el-col :span="3" justify="center" align="middle">
          <el-button type="info" @click="run">run</el-button>
        </el-col>
      </el-row>
    </el-main>
  </el-container>
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

interface Config {
  id: number
  token?: string
  servers: string[]
  ice_servers: IceServer[]
}

const Config = Schema.object({
  id: Schema.number().required(),
  token: Schema.string(),
  servers: Schema.array(Schema.string()),
  iceServers: Schema.array(Schema.object({
    url: Schema.string().required(),
    username: Schema.string(),
    credential: Schema.string(),
  })),
}).description('cryonet')

const config = ref<Config>({
  id: Math.round(Math.random() * 1000),
  servers: ['ws://127.0.0.1:2333'],
  ice_servers: [],
})

const initial = ref<Config>({
  id: 0,
  servers: ['ws://127.0.0.1:2333'],
  ice_servers: [],
})

let cryonet: Cryonet | null = null
async function run() {
  if (!cryonet) {
    cryonet = await Cryonet.init(config.value)
  }
}
</script>
