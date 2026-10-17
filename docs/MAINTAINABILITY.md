# 平台与维护

## 模块职责

| 边界 | 实现 | 职责 |
|---|---|---|
| 应用与缓存 | app、model、store | 账号、会话、消息和持久化状态 |
| 本地传输 | backend、platform | IPC、JSON-RPC、请求关联和连接生命周期 |
| 系统集成 | platform | 用户目录、QQ 安装发现、文件及链接打开 |
| 剪贴板 | media::ClipboardAccess | Wayland 的 wl-clipboard、X11 的 xclip、Windows 的 arboard |
| QQ 服务 | bridge/native.cjs | 原生 SDK 调用、参数校验、会话及操作状态 |
| QQ 资源 | bridge/qq-resources.cjs | 表情目录、媒体解析和下载 |
| 宿主与显示 | runtime/linux、viewer/linux | QQ 进程、隔离显示和登录查看器 |

QQ 安装发现发生在配置读取阶段，使用 PATH、桌面登记和包管理器信息。`setup --qq-path` 可显式指定安装位置。

## QQ 兼容性

bridge/compatibility.json 记录平台、架构、版本和 wrapper 指纹。启动时检查版本，加载时检查模块指纹。支持新版本时，更新兼容配置并验证原生接口及资源布局。

SDK 的接口名、消息类型、参数结构和资源路径集中在桥接适配层。常见适配包括：

- 转发资源下载使用 downloadRichMediaInVisit，参数为 elem 和 triggerType；普通 downloadRichMedia 使用 downloadScene。
- 邮箱及聊天已读使用 setMsgRead(Peer)。
- onRecentContactNotification 的第一参数为会话数组，第二参数包含主列表提醒计数；初始化回调的 isOnlineMsg=false。
- notifiedType=2 表示提醒，notifiedType=1 表示仅更新计数。桌面通知通过 notify-rust 发出。

这些接口对应兼容配置中记录的 QQ 版本。

## 平台支持

完整宿主和发行包面向 Arch Linux x86_64。Windows 前端的适配涵盖命名管道、用户目录、安装发现、剪贴板和系统打开。

多账号目录适配位于 bridge/accounts.cjs。进程元数据和登录状态按实例保存，聊天数据通过原生 account_path 指向 QQ 数据目录。新增平台时需核对 SDK 的全局目录和账号目录语义。

## 存储与资源

Linux 使用系统 SQLite，最低版本为 3.51.2；该版本修复了并发 WAL 访问的 [Unix VFS 锁问题](https://sqlite.org/releaselog/3_51_2.html)。Windows 使用 bundled SQLite。

请求作用域、发送日志、草稿及图像资源的生命周期见 [架构](ARCHITECTURE.md)。回归入口见 [验证方法](VALIDATION.md)。
