# 懒猫微服

目标平台为 x86_64、lzcos 1.6.0 及以上；本次验证使用 lzcos 1.6.3 和 lzc-cli 2.0.8。
应用运行官方 Linux 微信 4.1.13.26 和 MimicWX OneBot 11。

## 构建

从微信官方 Linux 下载入口取得 `WeChatLinux_x86_64.deb`，保存为
`lazycat/downloads/wechat-4.1.13.26-amd64.deb`。
本项目不将微信安装包提交到 Git。Dockerfile 会校验固定版本和 SHA-256：

```text
a0db8caeee8d1e6cfe0aa5ce58710270f8d74a90ff4920f53a3fc22aa0956fe9
```

若官方已替换下载包，不能仅修改散列继续构建；须重新核对版本及密钥提取函数特征。
已有环境可以提供对应版本的官方安装包。

```sh
cd lazycat
lzc-cli project build
lzc-cli lpk lint ./cc.naily.mimicwx-v0.5.0.lpk
lzc-cli app install ./cc.naily.mimicwx-v0.5.0.lpk
```

Mac 使用 OrbStack 时，可为构建命令设置
`DOCKER_CONTEXT=orbstack BUILDX_BUILDER=orbstack`。
构建阶段执行 Rust release 测试；只清理 MimicWX 自身编译产物，保留依赖缓存，
防止长时间构建后 Cargo 因源文件时间戳较旧而错误复用旧二进制。

## 登录和连接

从应用的微信桌面入口扫码。登录后服务等待密钥和数据库就绪，再启用 OneBot。
OneBot 入口为 `wss://mimicwx.<微服域名>/api/onebot/ws`。
Token 保存在应用数据目录的 `state/api-token` 中；升级不会重新生成已有 Token。
连接参数及 Koishi 插件构建见 [ONEBOT.md](../ONEBOT.md)。

桌面及 REST API 使用懒猫身份认证；只有 `/api/onebot/ws` 绕过平台登录，
该端点始终验证业务 Bearer Token。没有 Token 时不会开启 OneBot。
接口状态显示已登录不代表所有消息能力均已实现，请核对 OneBot 能力表和限制。

重启使用懒猫应用管理命令，对运行中的应用依次停止、启动：

```sh
lpk-manager stop cc.naily.mimicwx
lpk-manager start cc.naily.mimicwx
```

## 持久数据和权限

- `wechat`：账号设置。
- `wechat-files`：新版微信数据库和媒体。
- `config`：MimicWX 配置。
- `state`：Token、数据库密钥和密钥提取日志。

Token、密钥和配置权限为 `0600`，不应上传到代码仓库或工单。
桌面和 MimicWX 以独立 `wechat` 用户运行；初始化过程以 root 设置文件权限、
附加 GDB。应用使用 `runc`，通过 `compose.override` 申请 `SYS_ADMIN` 和 `SYS_PTRACE`，
分别用于 fanotify 和本实例的密钥提取，不修改宿主机系统配置。
