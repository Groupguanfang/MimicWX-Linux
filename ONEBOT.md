# OneBot 11 与 Koishi

## 连接

登录微信、数据库就绪后，在 `config.toml` 中启用：

```toml
[api]
token = "运行时生成的密钥"

[onebot]
enabled = true
token = ""
media_url = "https://mimicwx.<微服域名>/api/onebot/ws"
```

`[onebot].token` 留空时复用 `[api].token`。必须配置其中一个 Token；没有 Token 时拒绝启用。
推荐用 `Authorization: Bearer <token>`，也支持 `?access_token=<token>` 和 `?token=<token>`。
凭据只写入运行配置，不提交到 Git。

安装社区维护的 `koishi-plugin-adapter-onebot`，选择 `protocol: ws`，
endpoint 使用 `ws://HOST:8899/onebot/ws`，selfId 使用 `get_login_info` 返回的微信账号 ID。
懒猫入口为 `wss://mimicwx.<微服域名>/api/onebot/ws`。

现有微信会话迁移可使用本仓库 `integrations/koishi` 的 TypeScript 包装器。
它保留 `wechat-robot` 平台身份，使现有用户权限、群 ID 和 ChatLuna 路由继续适用。
协议通信由社区 OneBot 适配器完成。云崽专用适配器已移除。

```sh
cd integrations/koishi
npm ci
npm run lint
npm run typecheck
npm test
npm pack
```

将生成的插件包及 `koishi-plugin-adapter-onebot@6.9.5` 安装到 Koishi，启用 `mimicwx-onebot`。
在面板配置正向 WebSocket、endpoint、selfId 和 token。
`allowedTargets` 可限定允许收发的微信用户和群 ID；留空允许全部。
测试迁移时填写测试群及指定私聊 ID，验证后按需扩展。
停用旧微信适配器和 `wechat-adapter-compat`，避免同一账号重复接收消息。
新插件不需要旧适配器的群名和去重补丁。
私聊的 OneBot 会话 ID 使用 `private:<wxid>`；用户权限保留，但按旧私聊 channel ID
固定的单独路由需要对应调整。

## 反向 WebSocket 与 AstrBot

MimicWX 可以同时监听正向连接和主动连接反向服务端。反向连接使用 OneBot 11
`Universal` 模式，共用事件推送和动作请求通道。断线后自动重连。

在 AstrBot 创建 OneBot v11 适配器，监听 MimicWX 能访问的地址和端口。将以下配置
添加到 MimicWX 的 `config.toml`，并重启 MimicWX 接口进程加载。

```toml
[[onebot.reverse]]
url = "ws://astrbot:6199/ws"
token = "在 AstrBot 适配器配置的独立 Token"
numeric_ids = true
original_images = true
```

- 可以重复配置 `[[onebot.reverse]]` 接入不同的服务端，支持 `ws://` 和 `wss://`。
- `token` 用于向该服务端发送 `Authorization: Bearer <token>`，独立于正向 Token。
- `numeric_ids = true` 将微信用户和消息 ID 映射为数字，并以群 ID 的数字部分展示群聊。
  消息查询、引用和 @ 使用连接已知的映射。正向连接继续使用原始微信 ID。
- `original_images = true` 为这个反向连接的普通图片地址加上 `original=1`，也用于引用和历史消息。
  微信原图或无独立高清源的完整图尚不可用时，该地址返回 404。语音和表情包沿用原有地址。
- 群聊和私聊均通过 OneBot 转发。需要限制机器人回复的会话时，请在 AstrBot 等接入端配置访问控制。

## 支持的动作

- `get_login_info`、`get_status`、`get_version_info`。
- `get_friend_list`、`get_group_list`、`get_group_info`、`get_stranger_info`、`get_group_member_info`。
- `send_private_msg`、`send_group_msg`、`send_msg`、`get_msg`。
- `get_forward_msg`：按 `forward` 段中的 `id` 查询已读取的合并转发节点。
- `get_image`：查询当前运行实例注册的媒体文件、大小及下载地址。
- `get_group_msg_history`、`get_friend_msg_history`：从微信本地数据库的只读快照分页读取历史，每页最多 100 条。
- `can_send_image`、`can_send_record`；`mark_msg_as_read` 为兼容性空操作。

消息支持数组和 CQ 码。文本、图片和语音保留发送顺序，发送失败返回错误。
群聊 `at` 段以微信成员 ID 定位。普通消息里的 `@群昵称` 在当前群成员唯一匹配时也会转换为原生提醒。
邮箱、不明确的昵称以及带 `auto_escape: true` 的字符串按文字发送。
发送回执会核对微信记录中的 `atuserlist`，带引用的消息同时核对原消息 ID。
图片接受 HTTP(S) URL、`base64://`、`data:` 和 `file://`，限制 20 MiB。
语音接受 HTTP(S) URL、`base64://`、`data:audio/*;base64,` 或
`data:audio/*` 的 Base64 内容，限制 2 MiB；发送前转成单声道 24 kHz PCM WAV，
再由微信原生“发语音”控件和虚拟麦克风录制，时长限制 0.5–55 秒。
OneBot 在目标会话完成桌面提交后返回，文本和图片的数据库回执转为后台确认。
语音同样先返回 `confirmation_pending: true`，后台按目标会话、自发消息和时长核验落库。
响应中的 `confirmation_pending: true`、`verified: false` 表示已提交、尚未确认落库，
不能视为已经送达。后台仍按实际接收方、完整文本或自发图片检查回执，
最长等待 20 秒；结果写入日志并通过 REST WebSocket 的 `send_confirmation` 事件广播。
`POST /send` 和 `POST /send_image` 保留同步等待数据库确认的行为。
发送目标必须对应已知且名称唯一的联系人或群，避免桌面搜索发送给同名会话。
会话列表仅精确匹配首行标题；粘贴前再次检查可见输入框对应的会话，无法确认时停止发送。
会话列表会按需滚动查找未显示的条目。搜索回退仅点击名称精确匹配的可见会话结果，
不会直接按回车打开“搜一搜”或选择不明确的第一项。
文本回执同时核对实际接收方 ID 和完整内容，不会把其他群里的相同文字当成发送成功。
后台等待文本回执时会每 500 毫秒唤醒数据库监听，以覆盖微信延迟落盘
及文件通知未及时到达的情况。超时不会自动重新发送，避免重复提醒。
多个后台确认共享刷新节奏，每 500 毫秒最多主动触发一次数据库扫描；
每条数据库回执只确认一个发送请求，连续相同文字或图片不会共用同一条回执。
发送前检查输入框焦点和完整粘贴内容；存在弹窗、已有草稿或内容不匹配时取消发送。
搜索结果确认后不再发送 Esc，避免触发主窗口的退出操作。
文字粘贴和 Enter 后按输入框实际状态继续执行，最长等待 1.5 秒，
不再固定额外等待 300/500 毫秒；完整内容和发送后清空均须通过校验。

同一 OneBot 连接的发送动作按接收顺序执行，多段文字和图片不会被其他连接的发送插入。
查询动作、入站事件和心跳可在发送期间继续处理；所有出站 WebSocket 消息仍由同一任务写入。
每个连接最多保留 64 个执行中及排队动作，超过上限明确返回错误。

ChatLuna 本地存储的 `serverPath` 应使用 Koishi 自身可访问的内网地址；
需要登录的懒猫入口会将后台文件请求重定向到 HTML 登录页。
若微信桥接容器无法访问该内网地址，在 Koishi 适配器的 `inlineMediaPrefixes` 中填写
对应的 `/chatluna-storage/temp/` URL 前缀。ChatLuna Memes 使用本地地址时，
也应加入 `http://127.0.0.1:5140/chatluna-memes/` 前缀。适配器先在 Koishi 内下载这些图片，
再通过 OneBot Base64 图片段发送，保留表情包标记。
随机图和 `?q=` 检索可在配置的前缀内跟随最多 5 次重定向，整个下载限时 20 秒；
重定向到其他站点的 HTTP(S) 图片地址交回 OneBot 下载，不把 Koishi 内网地址交给微信容器。
同站点跳出配置前缀、非 HTTP(S) 地址、HTML 和超过 20 MiB 的图片响应会明确报错。
不会把撤回、加好友、群管理等尚未实现的动作伪装成成功。

## 从旧微信兼容脚本迁入

- 私聊、群聊转换为 `post_type: message`，保留发送者、会话和时间。
- 群名通过 `get_group_info`、`get_group_list` 返回，替代 Koishi 侧的 `getGuild` 补丁。
- 入站引用使用 `reply` 段，Koishi 通过 `get_msg` 获取原作者和内容。
- 公众号文章及 `app_type=4/5` 的链接卡片发出标准 `share` 段，
  同时附带标题、摘要和链接的 `text` 段，供不识别 `share` 的 ChatLuna 等消费者使用。
  原生引用 `app_type=57`、小程序、文件和音乐保持原有处理。
- 合并转发 `app_type=19` 读取微信保存的记录节点，同时发出 `forward` 段及带原作者和时间的正文。
  引用合并转发时也可从同一会话的历史记录补齐正文。转发内的图片和语音目前以类型占位。
- 引用图片在具有可访问媒体地址时保留为 `image`，避免只传递 `[图片]`。
- 图片、表情等 XML 属性使用标准实体解码，恢复 CDN URL 中的查询参数分隔符，
  同时保留 URL 百分号编码，避免把 `&amp;filekey` 当作真实参数名。
- `atuserlist` 与机器人名称提及转换为 `at` 消息段，供 Koishi 判断 @触发。
- 去重位于协议入口，缓存及去重记录各限制 4096 条。
- 自发消息保留在缓存中供引用查询，不再次触发 AI。
- 每 100 毫秒读取 `session.db` 的即时会话快照，提前派发发送者明确、
  UTF-8 正文少于 192 字节的普通文本，避免等待消息库批量落盘。
  群聊按快照中的发送者排除自发消息；私聊通过同库未读记录确认入站方向，
  即使私聊快照的发送者为空也可派发。已经打开并立即标为已读的私聊可能仍需等完整正文。
  微信会话摘要约在 200 字节截断；达到保护阈值的长文本、引用和媒体继续等完整正文。
  优先使用同库未读记录中的真实服务端 ID；尚未取得时使用 `fast_<chat>_<local_id>`。
  正文到达后补全缓存，
  保留临时 ID 的查询入口，并按会话和本地消息序号去重，避免触发两次 AI。
  同一会话在两次轮询之间有多条消息时，中间消息仍由消息库补发，可能保持原有延迟。
- 数字 `real_sender_id` 通过当前消息库的 `Name2Id` 解析，优先按发送者身份判断自发消息，
  避免自发图片的状态位被误判为群友消息。缺少发送者的群消息不向 OneBot 广播。
- 群消息中的发送者前缀在与已解析身份一致时剔除；兼容 LF、CRLF 和旧版正文内发送者。
- 好友列表的 `nickname` 是实际昵称，`remark` 是备注；消息事件的
  `sender.nickname` 使用实际昵称，并额外提供 `sender.remark` 与
  `sender.display_name`。目前没有读取微信的群昵称，`sender.card` 留空，
  不把个人备注当作群昵称；`get_group_member_info` 使用相同规则。
  Koishi 和 ChatLuna Character 在群聊、私聊中均使用实际昵称，
  个人备注仍可通过原始 OneBot 数据读取。
  微信原生引用在桌面定位时使用 `display_name`，保持与微信界面的备注显示一致。
- Koishi 历史拉取保留字符串会话 ID，避免将 `*@chatroom` 转成无效的数字。
- 连接建立发送生命周期事件，每 30 秒发送真实在线状态心跳。
- 主数据库、WAL 内容更新或 WAL/SHM 文件轮换后刷新读取连接，保留消息游标；
  读取连接关闭时不执行自动 checkpoint。
- 账号目录的四位十六进制后缀从 wxid 中剔除，避免与旧用户身份及 @目标不一致。

## 微信扩展与能力限制

正向连接和默认的反向连接中，账号 ID 使用原始 `wxid_*`，群 ID 使用原始 `*@chatroom`，消息 ID 使用字符串，
避免微信 64 位消息 ID 在 JavaScript 中丢精度。这是 OneBot 11 的微信 ID 扩展，
不是 QQ 整数 ID，不能假设所有仅支持 QQ 整数的客户端均兼容。

出站 `reply` + 文字通过微信桌面的原生“引用”菜单发送引用卡片，
桥接按微信真实消息 ID 从当前会话的本地历史读取原消息，使用时间和相邻消息
的顺序核对可见气泡。原消息不在当前列表时，通过微信「聊天记录」搜索，再跳到
原会话位置。只有同文气泡能唯一定位且引用栏里的原作者与内容一致时才发送。
后台回执还会核对微信引用 XML 的原消息 ID 和回复正文。
本地历史中缺少原消息、聊天记录搜索无法定位或引用栏校验失败时返回错误。
`reply` 可与文字及群聊 `at` 一起发送，图片可另行发送。
出站群聊 `at` 使用微信群成员候选框。候选必须唯一且昵称核对成功；
同名、未入群或 `at all` 的结构化请求返回错误。
引用只接受目标群聊或私聊里的原消息。传入微信真实消息 ID 时可直接查本地历史。
临时消息 ID 须先解析到已落库的原消息。
`get_msg` 查询实时消息与最近拉取的历史消息缓存。
历史页按时间正序返回。第一页只传会话 ID 和可选的 `count`，接着将响应中的
`next_cursor` 原样作为 `cursor` 请求上一页，直到 `has_more` 为 `false`。
分页覆盖当前账号在微信本地保存的全部历史。历史查询不会重新下发实时事件。
旧客户端仍可使用 `message_seq` 排除该序号及其之后的记录；跨分片遍历请使用 `cursor`。
`history_source` 表示读取来源。数据库暂不可用时，无 `cursor` 的请求可以回退到进程内缓存；
持有数据库游标的请求返回可重试错误，避免把缓存的末尾误判为历史结束。
群成员列表、撤回和群管理尚未实现；语音发送已实现，
但依赖当前微信容器中的 PulseAudio 虚拟麦克风，`can_send_record` 会按实际设备状态返回。

配置 `media_url` 后，入站图片、表情、语音及引用中的媒体使用标准 `image`/`record` 段，
地址指向该实例的媒体下载入口。`media_url` 必须是 HTTP(S) OneBot 入口，不带 Token、
查询参数或片段。懒猫使用已有的 `/api/onebot/ws` 入口，无需扩大 `public_path`；
普通 WebSocket 连接仍须通过原有 Token 验证。

媒体请求使用 `?media=<随机文件标识>`，每个标识包含 256 位随机值，只能读取已注册的
一个媒体，不授予 OneBot 动作权限，不把微信 AES 密钥、CDN 参数或全局 Token 下发给模型。
链接在接口进程重启或缓存淘汰后失效，不能当永久收藏地址。最多保留 4096 个来源描述，
已下载图片缓存总量限制为 128 MiB，最多同时下载 4 个文件，单文件限制 20 MiB。
普通图片优先读取标为原图的 CDN 内容和本地 `_h.dat`，可用原图校验值也能验证本地文件。
常规地址仍可回退到 `preview` 或 `thumbnail`。带 `original=1` 的地址仅返回 `original`
或 `full`，后者是微信元数据确认没有独立高清版本的完整图。

微信刚发送的图片和表情可能先写入空内容，再补齐媒体信息。监听器会短暂等待完整内容，
再推进同一会话的消息游标，避免把表情永久缓存成文字占位；过期空消息不会阻塞后续消息。

只有 MD5 的收藏表情在下载时只读查询当前账号的 `emoticon/emoticon.db`，
使用 `kNonStoreEmoticonTable` 中的媒体地址与密钥，不在文字消息监听循环中查询。
加密表情使用 AES-128-CBC，IV 为消息中的 AES key，校验 PKCS#7 填充及可用的 MD5。
内容下载和解密发生在图片请求时，不阻塞 OneBot 入站文字消息。
缩略图回退在响应头 `x-mimicwx-media-source` 中标记为 `thumbnail`。

没有配置 `media_url` 时仍直接转发可访问 HTTP(S) 图片地址。
普通图片只有不透明 CDN 标识时，按当前会话及服务端消息 ID 只读查询
`packed_info_data`，定位该账号、会话及月份下已下载的 `.dat` 文件；
不会用 XML 中的网络地址标识拼接本地路径，也不搜索其他会话。
支持旧 XOR、V1 和当前 Linux 客户端的 V2 AES-ECB / 原文 / XOR 分段格式。
V2 密钥从当前账号对应的 `~/.xwechat/net/kvcomm/*.statistic` 派生，
兼容微信轮换统计文件时更换名称前缀，
逐文件验证 AES 填充、长度和图片文件头，不读取进程内存，不输出密钥。
WXGF 内的 HEVC 静态图像使用 ffmpeg 转为 PNG，最多 4096×4096、20 MiB，
转码限时 15 秒。
图片尚未下载、密钥或格式不支持时返回错误，不将加密文件当成图片。
解密、读取和转码均发生在媒体请求阶段，不阻塞消息监听或文字发送。
只有网络 CDN 标识、且未下载到本地的图片仍不能直接拉取；GIF 多帧理解由 Koishi 处理。
出站 `image` 段带 `data.sticker: true` 或 `data.sub_type: "sticker"` 时发送微信原生表情。
GIF 通过文件剪贴板发送，保留原有动画；PNG/JPEG/WebP 等静态图片先转换为 GIF。
原生 GIF 限制为 1 MiB、宽高各不超过 1000 像素；静态图片转换后最多 512×512。
发送回执必须匹配目标会话、自发消息、时间窗、消息类型 47 和表情文件 MD5。
临时 GIF 文件保留至回执结束，避免微信尚未读取文件就被删除；
客户端初始表情记录的 MD5 为空时先等待内容补齐。
不能用普通图片回执确认表情送达。未带表情标记的 GIF 仍按普通图片发送。
Koishi 中 `h.image(url, { sticker: true })` 的标记会通过适配器保留，
可供 ChatLuna 的表情输出使用，不需要闭源协议服务或事先收藏。
入站语音从当前账号的 `message/media_*.db` 中按会话、消息服务端 ID 和本地 ID
读取 SILK 数据，通过同一媒体入口输出 `audio/silk`，不依赖旧微信客户端的下载接口。
Koishi 中的 AI 提示词、搜索、语音转写和方言模型配置属于上层能力，保留在各自插件内。

## 沙盒测试

Linux 构建环境执行 `cargo test --locked --release`。
测试覆盖鉴权、消息 ID、去重、自发消息缓存、引用及 @、引用图片、
引用会话隔离、图片送达回执、CQ 码和错误响应。
`lazycat/Dockerfile` 的 builder 阶段先执行测试，再生成二进制。

`integrations/koishi/tests/onebot.test.ts` 使用独立 Koishi 实例和 WebSocket 沙盒，
测试真实 OneBot 适配器收发、引用、@、身份保留和允许收发列表，不使用生产配置或连接真实微信。
TypeScript 使用 `naily-eslint-config` 校验，Vite Plus 构建。

懒猫打包说明见 [lazycat/README.md](lazycat/README.md)。
