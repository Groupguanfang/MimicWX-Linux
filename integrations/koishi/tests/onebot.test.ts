import type { Session } from 'koishi'
import type { WebSocket } from 'ws'
import events from 'node:events'
import moduleModule from 'node:module'
import { expect, it, vi } from 'vitest'
import { WebSocketServer } from 'ws'

const require = moduleModule.createRequire(import.meta.url)
const { App, h } = require('koishi') as typeof import('koishi')
const Http = require('@koishijs/plugin-http').default
const mimicwx = require('../lib/index.cjs')

it('koishi exchanges OneBot messages in an isolated websocket sandbox', async () => {
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
            message: [{ type: 'text', data: { text: '被引用的原消息' } }],
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
      sender: { user_id: 'wxid_sandbox_friend', nickname: '沙盒群友' },
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
    expect(group.quote?.user?.id).toBe('wxid_sandbox_bot')
    expect(group.quote?.content).toBe('被引用的原消息')
    expect(group.elements?.some(element => element.type === 'at' && element.attrs.id === bot.selfId)).toBe(true)
    expect(group.elements?.some(element => element.type === 'img' && element.attrs.src === 'https://example.test/image.png')).toBe(true)
    expect((await bot.getGuild('sandbox@chatroom')).name).toBe('沙盒测试群')
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

    connection!.send(JSON.stringify({
      time: 1794800000,
      self_id: 'wxid_sandbox_bot',
      post_type: 'message',
      message_type: 'private',
      sub_type: 'friend',
      user_id: 'wxid_sandbox_friend',
      message_id: 'private_sandbox',
      sender: { user_id: 'wxid_sandbox_friend', nickname: '沙盒好友' },
      message: [{ type: 'text', data: { text: '私聊测试' } }],
    }))
    await vi.waitFor(() => expect(received).toHaveLength(2))
    expect(received[1].isDirect).toBe(true)
    await bot.sendPrivateMessage('wxid_sandbox_friend', '私聊沙盒回复')
    expect(requests.find(request => request.action === 'send_private_msg')?.params.user_id).toBe('wxid_sandbox_friend')
  }
  finally {
    await app.stop()
    for (const client of server.clients) client.terminate()
    await new Promise<void>(resolve => server.close(() => resolve()))
  }
}, 15000)
