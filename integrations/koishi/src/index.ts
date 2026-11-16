import type { Context, h, Session, Universal } from 'koishi'
import { Schema } from 'koishi'
import { OneBot, OneBotBot } from 'koishi-plugin-adapter-onebot'

export const name = 'mimicwx-onebot'

export type MimicWXConfig = OneBotBot.Config & {
  allowedTargets?: string[]
}

export const Config: Schema<MimicWXConfig> = Schema.intersect([
  OneBotBot.Config,
  Schema.object({
    allowedTargets: Schema.array(String).default([]).description('允许收发消息的微信用户或群 ID；留空允许全部。'),
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
    return super.createMessage(channelId, content, referrer, options)
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
