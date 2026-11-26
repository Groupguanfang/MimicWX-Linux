import type { Session } from 'koishi'
import type { WebSocket } from 'ws'
import events from 'node:events'
import http from 'node:http'
import moduleModule from 'node:module'
import { expect, it, vi } from 'vitest'
import { WebSocketServer } from 'ws'

const require = moduleModule.createRequire(import.meta.url)
const { App, h } = require('koishi') as typeof import('koishi')
const Http = require('@koishijs/plugin-http').default
const mimicwx = require('../lib/index.cjs')

it('koishi exchanges OneBot messages in an isolated websocket sandbox', async () => {
  const mediaServer = http.createServer((request, response) => {
    const redirects: Record<string, string> = {
      '/storage/random?q=%E6%94%B6%E5%88%B0': '/storage/example.gif',
      '/storage/loop': '/storage/loop',
      '/storage/outside': '/unexpected',
      '/storage/invalid': 'file:///sandbox/image.gif',
      '/storage/external': 'https://example.test/remote.gif',
    }
    if (request.url && redirects[request.url]) {
      response.writeHead(302, { location: redirects[request.url] })
      response.end()
      return
    }
    response.setHeader('content-type', request.url === '/storage/html' ? 'text/html' : 'image/gif')
    response.end(request.url === '/storage/html' ? '<html>login</html>' : 'GIF89a0000')
  })
  mediaServer.listen(0, '127.0.0.1')
  await events.once(mediaServer, 'listening')
  const mediaAddress = mediaServer.address()
  if (typeof mediaAddress === 'string' || !mediaAddress) throw new Error('Missing media sandbox address')
  const mediaOrigin = `http://127.0.0.1:${mediaAddress.port}`
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 })
  await events.once(server, 'listening')
  const address = server.address()
  if (typeof address === 'string' || !address) throw new Error('Missing sandbox address')

  const app = new App()
  const received: Session[] = []
  const requests: { action: string, params: Record<string, any> }[] = []
  const headers: string[] = []
  let connection: WebSocket | undefined

  server.on('connection', (socket, request) => {
    connection = socket
    headers.push(request.headers.authorization || '')
    socket.on('message', (buffer) => {
      const request = JSON.parse(buffer.toString())
      requests.push(request)
      let data: any = null
      let supported = true
      switch (request.action) {
        case 'get_login_info':
          data = { user_id: 'wxid_sandbox_bot', nickname: '沙盒机器人' }
          break
        case 'get_msg':
          data = {
            message_id: request.params.message_id,
            message_type: 'group',
            group_id: 'sandbox@chatroom',
            sender: { user_id: 'wxid_sandbox_bot', nickname: '沙盒机器人' },
            time: 1794800000,
            message_seq: 17,
            message: [{ type: 'text', data: { text: '被引用的原消息' } }],
          }
          break
        case 'get_group_msg_history':
          data = {
            messages: [{
              message_id: '9007199254740995',
              message_type: 'group',
              group_id: request.params.group_id,
              sender: { user_id: 'wxid_sandbox_friend', nickname: '沙盒群友' },
              time: 1794800000,
              message_seq: 16,
              message: [{ type: 'text', data: { text: '群聊缓存历史' } }],
            }],
          }
          break
        case 'get_friend_msg_history':
          data = {
            messages: [{
              message_id: 'history_private',
              message_type: 'private',
              user_id: request.params.user_id,
              sender: { user_id: request.params.user_id, nickname: '沙盒好友' },
              time: 1794800000,
              message: [{ type: 'text', data: { text: '私聊缓存历史' } }],
            }],
          }
          break
        case 'get_group_info':
          data = { group_id: request.params.group_id, group_name: '沙盒测试群' }
          break
        case 'send_group_msg':
        case 'send_private_msg':
          data = { message_id: `sandbox_${requests.length}` }
          break
        default:
          supported = false
      }
      socket.send(JSON.stringify({
        status: supported ? 'ok' : 'failed',
        retcode: supported ? 0 : 1404,
        data,
        echo: request.echo,
      }))
    })
  })

  app.plugin(Http)
  app.plugin(mimicwx, {
    protocol: 'ws',
    endpoint: `ws://127.0.0.1:${address.port}`,
    selfId: 'wxid_sandbox_bot',
    token: 'sandbox-token',
    responseTimeout: 3000,
    allowedTargets: ['sandbox@chatroom', 'wxid_sandbox_friend'],
    inlineMediaPrefixes: [`${mediaOrigin}/storage/`],
  })
  app.on('message', session => received.push(session))

  try {
    await app.start()
    await vi.waitFor(() => expect(app.bots[0]?.status).toBe(1))
    const bot = app.bots[0]
    expect(bot.platform).toBe('wechat-robot')
    expect(bot.selfId).toBe('wxid_sandbox_bot')
    expect(headers).toEqual(['Bearer sandbox-token'])

    connection!.send(JSON.stringify({
      time: 1794800000,
      self_id: 'wxid_sandbox_bot',
      post_type: 'message',
      message_type: 'group',
      sub_type: 'normal',
      group_id: 'sandbox@chatroom',
      user_id: 'wxid_sandbox_friend',
      message_id: '9007199254740993',
      sender: {
        user_id: 'wxid_sandbox_friend',
        nickname: '沙盒群友',
        card: '',
        remark: '仅自己可见的备注',
        display_name: '仅自己可见的备注',
      },
      message: [
        { type: 'reply', data: { id: 'quoted_sandbox' } },
        { type: 'at', data: { qq: 'wxid_sandbox_bot' } },
        { type: 'text', data: { text: ' 看看这张图片' } },
        { type: 'image', data: { file: 'image-id', url: 'https://example.test/image.png' } },
      ],
    }))
    await vi.waitFor(() => expect(received).toHaveLength(1))
    const group = received[0]
    expect(group.platform).toBe('wechat-robot')
    expect(group.guildId).toBe('sandbox@chatroom')
    expect(group.messageId).toBe('9007199254740993')
    expect(group.username).toBe('沙盒群友')
    expect(group.author?.name).toBe('沙盒群友')
    expect(group.author?.nick).toBe('')
    expect(group.author?.nick || group.author?.name || group.username).toBe('沙盒群友')
    expect(group.quote?.user?.id).toBe('wxid_sandbox_bot')
    expect(group.quote?.content).toBe('被引用的原消息')
    expect(group.elements?.some(element => element.type === 'at' && element.attrs.id === bot.selfId)).toBe(true)
    expect(group.elements?.some(element => element.type === 'img' && element.attrs.src === 'https://example.test/image.png')).toBe(true)
    expect((await bot.getGuild('sandbox@chatroom')).name).toBe('沙盒测试群')
    const history = await bot.getMessageList('sandbox@chatroom', 'quoted_sandbox')
    expect(history.data[0].id).toBe('9007199254740995')
    expect(history.data[0].content).toBe('群聊缓存历史')
    expect(requests.find(request => request.action === 'get_group_msg_history')?.params).toEqual({
      group_id: 'sandbox@chatroom',
      message_seq: 17,
    })
    await expect(bot.getMessageList('forbidden@chatroom')).rejects.toThrow('允许收发列表')
    await expect(bot.getMessageList('sandbox@chatroom', undefined, 'after')).rejects.toThrow('Unsupported direction.')
    await expect(bot.sendMessage('forbidden@chatroom', '禁止发送')).rejects.toThrow('允许收发列表')
    await expect(bot.sendPrivateMessage('wxid_forbidden', '禁止发送')).rejects.toThrow('允许收发列表')
    const forbidden = bot.session({
      type: 'message',
      guild: { id: 'forbidden@chatroom' },
      channel: { id: 'forbidden@chatroom', type: 0 },
      user: { id: 'wxid_sandbox_friend' },
      message: { id: 'forbidden-message', content: '禁止接收' },
    })
    bot.dispatch(forbidden)
    await new Promise(resolve => setImmediate(resolve))
    expect(received).toHaveLength(1)

    await bot.sendMessage('sandbox@chatroom', [
      h.quote('quoted_sandbox'),
      h.at('wxid_sandbox_friend'),
      '沙盒回复',
    ])
    const sentGroup = requests.find(request => request.action === 'send_group_msg')
    expect(sentGroup?.params.group_id).toBe('sandbox@chatroom')
    expect(sentGroup?.params.message).toEqual(expect.arrayContaining([
      { type: 'reply', data: { id: 'quoted_sandbox' } },
      { type: 'at', data: { qq: 'wxid_sandbox_friend' } },
      { type: 'text', data: { text: '沙盒回复' } },
    ]))
    await bot.sendMessage('sandbox@chatroom', h.image('data:image/gif;base64,R0lGODlhMDAwMA=='))
    const sentImage = requests.find(request =>
      request.action === 'send_group_msg'
      && (request.params.message as { type: string }[]).some(segment => segment.type === 'image'))
    expect(sentImage?.params.message).toEqual([
      { type: 'image', data: { file: 'base64://R0lGODlhMDAwMA==', cache: 0 } },
    ])
    await bot.sendMessage('sandbox@chatroom', h.image(`${mediaOrigin}/storage/example.gif`))
    expect(requests.at(-1)?.params.message).toEqual([
      { type: 'image', data: { file: 'base64://R0lGODlhMDAwMA==', cache: 0 } },
    ])
    const randomSticker = h.image(`${mediaOrigin}/storage/random?q=${encodeURIComponent('收到')}`, { sticker: true })
    for (const channelId of ['sandbox@chatroom', 'private:wxid_sandbox_friend']) {
      await bot.sendMessage(channelId, randomSticker)
      expect(requests.at(-1)?.action).toBe(channelId.startsWith('private:') ? 'send_private_msg' : 'send_group_msg')
      expect(requests.at(-1)?.params.message).toEqual([
        { type: 'image', data: { sticker: true, file: 'base64://R0lGODlhMDAwMA==', cache: 0 } },
      ])
    }
    await bot.sendMessage('sandbox@chatroom', h.image(`/storage/random?q=${encodeURIComponent('收到')}`))
    expect(requests.at(-1)?.params.message).toEqual([
      { type: 'image', data: { file: 'base64://R0lGODlhMDAwMA==', cache: 0 } },
    ])
    const beforeRejectedImage = requests.length
    await expect(bot.sendMessage('sandbox@chatroom', h.image(`${mediaOrigin}/storage/html`))).rejects.toThrow('类型 text/html')
    await expect(bot.sendMessage('sandbox@chatroom', h.image(`${mediaOrigin}/storage/loop`))).rejects.toThrow('重定向次数')
    await expect(bot.sendMessage('sandbox@chatroom', h.image(`${mediaOrigin}/storage/outside`))).rejects.toThrow('超出已配置')
    await expect(bot.sendMessage('sandbox@chatroom', h.image(`${mediaOrigin}/storage/invalid`))).rejects.toThrow('HTTP(S)')
    expect(requests).toHaveLength(beforeRejectedImage)
    await bot.sendMessage('sandbox@chatroom', h.image(`${mediaOrigin}/storage/external`))
    expect(requests.at(-1)?.params.message).toEqual([
      { type: 'image', data: { file: 'https://example.test/remote.gif', cache: 0 } },
    ])
    await bot.sendMessage('sandbox@chatroom', h.image('https://example.test/public.gif'))
    expect(requests.at(-1)?.params.message).toEqual([
      { type: 'image', data: { file: 'https://example.test/public.gif', cache: 0 } },
    ])

    connection!.send(JSON.stringify({
      time: 1794800000,
      self_id: 'wxid_sandbox_bot',
      post_type: 'message',
      message_type: 'private',
      sub_type: 'friend',
      user_id: 'wxid_sandbox_friend',
      message_id: 'private_sandbox',
      sender: {
        user_id: 'wxid_sandbox_friend',
        nickname: '沙盒好友',
        card: '',
        remark: '仅自己可见的备注',
        display_name: '仅自己可见的备注',
      },
      message: [{ type: 'text', data: { text: '私聊测试' } }],
    }))
    await vi.waitFor(() => expect(received).toHaveLength(2))
    expect(received[1].isDirect).toBe(true)
    expect(received[1].username).toBe('沙盒好友')
    expect(received[1].author?.name).toBe('沙盒好友')
    expect(received[1].author?.nick || received[1].author?.name || received[1].username).toBe('沙盒好友')
    const privateHistory = await bot.getMessageList('private:wxid_sandbox_friend')
    expect(privateHistory.data[0].content).toBe('私聊缓存历史')
    expect(requests.find(request => request.action === 'get_friend_msg_history')?.params.user_id).toBe('wxid_sandbox_friend')
    await bot.sendPrivateMessage('wxid_sandbox_friend', '私聊沙盒回复')
    expect(requests.find(request => request.action === 'send_private_msg')?.params.user_id).toBe('wxid_sandbox_friend')
  }
  finally {
    await app.stop()
    for (const client of server.clients) client.terminate()
    await new Promise<void>(resolve => server.close(() => resolve()))
    await new Promise<void>(resolve => mediaServer.close(() => resolve()))
  }
}, 15000)
