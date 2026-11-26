import type { Context, Session, Universal } from 'koishi'
import buffer from 'node:buffer'
import { h, Schema } from 'koishi'
import { OneBot, OneBotBot } from 'koishi-plugin-adapter-onebot'

export const name = 'mimicwx-onebot'

export type MimicWXConfig = OneBotBot.Config & {
  allowedTargets?: string[]
  inlineMediaPrefixes?: string[]
}

export const Config: Schema<MimicWXConfig> = Schema.intersect([
  OneBotBot.Config,
  Schema.object({
    allowedTargets: Schema.array(String).default([]).description('允许收发消息的微信用户或群 ID；留空允许全部。'),
    inlineMediaPrefixes: Schema.array(String).default([]).description('由 Koishi 下载并转为内联图片的 URL 前缀，用于桥接容器无法访问的内部存储地址。'),
  }),
])

export class MimicWXBot extends OneBotBot<Context, MimicWXConfig> {
  constructor(ctx: Context, config: MimicWXConfig) {
    super(ctx, config)
    this.platform = 'wechat-robot'
    if (this.user) this.user.avatar = ''
  }

  private allows(target?: string): boolean {
    if (!target) return false
    return !this.config.allowedTargets?.length
      || this.config.allowedTargets.includes(target.replace(/^private:/, ''))
  }

  override dispatch(session: Session): void {
    if (session.type === 'message' && !this.allows(session.guildId || session.channelId)) return
    super.dispatch(session)
  }

  override async createMessage(channelId: string, content: h.Fragment, referrer?: unknown, options?: Parameters<OneBotBot<Context>['createMessage']>[3]): Promise<Universal.Message[]> {
    if (!this.allows(channelId)) throw new Error('该会话未加入 MimicWX 允许收发列表')
    const inlineImage = async (attrs: Record<string, any>): Promise<h> => {
      let source = attrs.src || attrs.url
      const isInline = (url: string): boolean => !!this.config.inlineMediaPrefixes?.some(prefix => prefix && url.startsWith(prefix))
      if (typeof source === 'string' && source.startsWith('/')) {
        const candidates = this.config.inlineMediaPrefixes || []
        const origin = (candidates.find(prefix => prefix && source.startsWith(new URL(prefix).pathname)) || candidates.find(Boolean))?.trim()
        if (!origin) throw new Error('内部图片相对地址缺少可用的 URL 前缀')
        source = new URL(source, origin).href
      }
      if (typeof source !== 'string' || !isInline(source)) return h('img', attrs)
      const signal = AbortSignal.timeout(20000)
      let currentSource = source
      let response = await fetch(currentSource, { signal, redirect: 'manual' })
      for (let redirects = 0; [301, 302, 303, 307, 308].includes(response.status); redirects++) {
        const location = response.headers.get('location')
        await response.body?.cancel()
        if (redirects >= 5) throw new Error('内部图片重定向次数超过 5 次')
        if (!location) throw new Error('内部图片重定向缺少目标地址')
        const target = new URL(location, currentSource)
        if (!['http:', 'https:'].includes(target.protocol) || target.username || target.password) {
          throw new Error('内部图片重定向必须使用不带凭据的 HTTP(S) 地址')
        }
        if (!isInline(target.href)) {
          if (target.origin === new URL(currentSource).origin) throw new Error('内部图片重定向超出已配置的 URL 前缀')
          return h('img', { ...attrs, src: target.href })
        }
        currentSource = target.href
        response = await fetch(currentSource, { signal, redirect: 'manual' })
      }
      const mimeType = response.headers.get('content-type')?.split(';')[0].trim()
      if (!response.ok || !mimeType || !/^image\/(?:gif|png|jpeg|webp|bmp)$/.test(mimeType)) {
        await response.body?.cancel()
        throw new Error(`内部图片下载失败：HTTP ${response.status}，类型 ${mimeType || 'unknown'}`)
      }
      const limit = 20 * 1024 * 1024
      if (Number(response.headers.get('content-length')) > limit) {
        await response.body?.cancel()
        throw new Error('内部图片超过 20 MiB')
      }
      const reader = response.body?.getReader()
      if (!reader) throw new Error('内部图片响应为空')
      const chunks: Uint8Array[] = []
      let size = 0
      try {
        for (;;) {
          const chunk = await reader.read()
          if (chunk.done) break
          size += chunk.value.length
          if (size > limit) throw new Error('内部图片超过 20 MiB')
          chunks.push(chunk.value)
        }
      }
      finally {
        await reader.cancel()
      }
      return h('img', { ...attrs, src: `data:${mimeType};base64,${buffer.Buffer.concat(chunks).toString('base64')}` })
    }
    const prepared = await h.transformAsync(h.normalize(content), { img: inlineImage, image: inlineImage })
    return super.createMessage(channelId, prepared, referrer, options)
  }

  override async getMessageList(channelId: string, before?: string, direction: Universal.Direction = 'before'): Promise<{ data: Universal.Message[] }> {
    if (!this.allows(channelId)) throw new Error('该会话未加入 MimicWX 允许收发列表')
    if (direction !== 'before') throw new Error('Unsupported direction.')
    const sequence = before ? (await this.internal.getMsg(before)).message_seq : undefined
    if (before && sequence === undefined) return { data: [] }
    const messages = channelId.startsWith('private:')
      ? await this.internal.getFriendMsgHistory(channelId.slice('private:'.length), sequence)
      : (await this.internal.getGroupMsgHistory(channelId, sequence)).messages
    return { data: await Promise.all(messages.map(message => OneBot.adaptMessage(this, message))) }
  }
}

export function apply(ctx: Context, config: MimicWXConfig): void {
  ctx.plugin(MimicWXBot, config)
}
