import { createApp } from 'vue'
import ElementPlus from 'element-plus'
import 'element-plus/dist/index.css'
import form from 'schemastery-vue'
import { createI18n } from 'vue-i18n'
// @ts-ignore
import Markdown from 'markdown-vue'
import App from './App.vue'

import init from 'cryonet-lib'
await init()

const i18n = createI18n({
  legacy: false,
});
const app = createApp(App);

app.use(ElementPlus)
app.use(i18n)
app.use(form)
app.component('k-markdown', Markdown)
app.mount('#app')
