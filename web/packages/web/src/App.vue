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
import { createStack } from 'tcpip'

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
}

interface Config {
  ip: string
  cryonet: CryonetConfig
}

const Config = Schema.object({
  ip: Schema.string().required(),
  cryonet: Schema.object({
    id: Schema.number().required(),
    token: Schema.string(),
    servers: Schema.array(Schema.string()),
    iceServers: Schema.array(Schema.object({
      url: Schema.string().required(),
      username: Schema.string(),
      credential: Schema.string(),
    })),
  }),
}).description('cryonet')

const config = ref<Config>({
  ip: '10.0.0.1/24',
  cryonet: {
    id: Math.round(Math.random() * 1000),
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
  },
})

const initial = ref<Config>({
  ip: '10.0.0.1/24',
  cryonet: {
    id: Math.round(Math.random() * 1000),
    servers: ['ws://127.0.0.1:2333'],
    ice_servers: [],
  },
})

let ran = false
async function run() {
  if (ran) return
  ran = true
  const cryonet = await Cryonet.init(config.value.cryonet)
  const stack = await createStack()
  // @ts-ignore
  globalThis.stack = stack
  const writers = new Map<number, WritableStreamDefaultWriter>()
  let senders: Map<number, RTCDataChannel> = await cryonet.get_senders()
  cryonet.on_refresh(async () => {
    senders = await cryonet.get_senders()
    const receivers: Map<number, RTCDataChannel[]> = await cryonet!.get_receivers()
    for (const [id, recvs] of receivers) {
      let writer = writers.get(id)
      if (!writer) {
        const tun = await stack.createTunInterface({ ip: config.value.ip as any })
        tun.readable.pipeTo(new WritableStream({
          write: async (chunk) => {
            senders.get(id)?.send(new Uint8Array(chunk))
          },
        }))
        writer = tun.writable.getWriter()
        writers.set(id, writer)
      }
      for (const recv of recvs) {
        recv.onmessage = (event) => {
          writer.write(new Uint8Array(event.data))
        }
      }
    }
  })

  const listener = await stack.listenTcp({ port: 80 })
  for await (const conn of listener) {
    (async () => {
      const reader = conn.readable.getReader()
      const writer = conn.writable.getWriter()
      const encoder = new TextEncoder()
      const decoder = new TextDecoder()

      let rawHeaders = '';
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        rawHeaders += decoder.decode(value, { stream: true });
        if (rawHeaders.includes('\r\n\r\n')) {
          break;
        }
      }

      const [requestLine, ..._headerLines] = rawHeaders.split('\r\n');
      const [method, path, _version] = requestLine!.split(' ');
      console.log(`Received ${method} request for ${path}`);

      const body = `<html><body><h1>Hello from Cryonet!</h1><p>Path: ${path}</p></body></html>`;
      const bodyBytes = encoder.encode(body);

      const responseHeaders = [
        'HTTP/1.1 200 OK',
        'Content-Type: text/html; charset=utf-8',
        `Content-Length: ${bodyBytes.length}`,
        'Connection: close',
        '',
        ''
      ].join('\r\n');

      await writer.write(encoder.encode(responseHeaders));
      await writer.write(bodyBytes);
      await writer.close();
    })()
  }
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
