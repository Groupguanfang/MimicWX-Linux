# OneBot 11 与 Koishi

## 连接

登录微信、数据库就绪后，在 `config.toml` 中启用：

```toml
[api]
token = "运行时生成的密钥"

[onebot]
enabled = true
token = ""
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

## 支持的动作

- `get_login_info`、`get_status`、`get_version_info`。
- `get_friend_list`、`get_group_list`、`get_group_info`、`get_stranger_info`、`get_group_member_info`。
- `send_private_msg`、`send_group_msg`、`send_msg`、`get_msg`。
- `can_send_image`、`can_send_record`；`mark_msg_as_read` 为兼容性空操作。

消息支持数组和 CQ 码。文本、图片保留发送顺序，发送失败返回错误。
图片接受 HTTP(S) URL、`base64://`、`data:` 和 `file://`，限制 20 MiB。
发送目标必须对应已知且名称唯一的联系人或群，避免桌面搜索发送给同名会话。
会话列表仅精确匹配首行标题；粘贴前再次检查可见输入框对应的会话，无法确认时停止发送。
文本回执同时核对实际接收方 ID 和完整内容，不会把其他群里的相同文字当成发送成功。
不会把撤回、加好友、群管理等尚未实现的动作伪装成成功。

## 从旧微信兼容脚本迁入

- 私聊、群聊转换为 `post_type: message`，保留发送者、会话和时间。
- 群名通过 `get_group_info`、`get_group_list` 返回，替代 Koishi 侧的 `getGuild` 补丁。
- 入站引用使用 `reply` 段，Koishi 通过 `get_msg` 获取原作者和内容。
- 引用图片在具有可访问媒体地址时保留为 `image`，避免只传递 `[图片]`。
- `atuserlist` 与机器人名称提及转换为 `at` 消息段，供 Koishi 判断 @触发。
- 去重位于协议入口，缓存及去重记录各限制 4096 条。
- 自发消息保留在缓存中供引用查询，不再次触发 AI。
- 连接建立发送生命周期事件，每 30 秒发送真实在线状态心跳。
- 账号目录的四位十六进制后缀从 wxid 中剔除，避免与旧用户身份及 @目标不一致。

## 微信扩展与能力限制

账号 ID 使用原始 `wxid_*`，群 ID 使用原始 `*@chatroom`，消息 ID 使用字符串，
避免微信 64 位消息 ID 在 JavaScript 中丢精度。这是 OneBot 11 的微信 ID 扩展，
不是 QQ 整数 ID，不能假设所有仅支持 QQ 整数的客户端均兼容。

出站 `reply` 和 `at` 当前降级为可读文本，不产生微信原生引用卡片或真正的 @提醒。
`get_msg` 只查询进程内缓存，重启后旧缓存不可用。
群成员列表、撤回、群管理和语音发送尚未实现；能力接口报告不支持语音发送。

入站图片和表情有可访问 HTTP(S) 地址时传递 `image` 段；
只有加密 CDN 标识的媒体仍需后续下载解密。
语音目前只有上游解析出的时长和占位信息，不能直接复用旧 wechat-robot-client 的媒体下载接口。
Koishi 中的 AI 提示词、搜索、语音转写模型配置属于上层能力，保留在各自插件内。

## 沙盒测试

Linux 构建环境执行 `cargo test --locked --release`。
测试覆盖鉴权、消息 ID、去重、自发消息缓存、引用及 @、引用图片、CQ 码和错误响应。
`lazycat/Dockerfile` 的 builder 阶段先执行测试，再生成二进制。

`integrations/koishi/tests/onebot.test.ts` 使用独立 Koishi 实例和 WebSocket 沙盒，
测试真实 OneBot 适配器收发、引用、@、身份保留和允许收发列表，不使用生产配置或连接真实微信。
TypeScript 使用 `naily-eslint-config` 校验，Vite Plus 构建。

懒猫打包说明见 [lazycat/README.md](lazycat/README.md)。
