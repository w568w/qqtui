# 本地桥接协议 v2

传输为用户本地 IPC：Linux 使用私有 Unix Socket，Windows 使用命名管道。UTF-8 NDJSON，每行一个 JSON-RPC 2.0 对象，单帧最多 8 MiB。请求 ID 为 JSON 安全整数。一个连接可同时发送请求并接收事件。

业务请求携带 account 字符串；修改操作同时携带预期 epoch，SDK 上下文已变化时拒绝执行；当前账号不匹配时拒绝调用 native。peer 为 {"chat_type":1,"uid":"u_..."}，聊天动作初始支持私聊 1、群聊 2、设备传输 8、已有群临时会话 100；QQ 邮箱提醒 103 仅支持读取与已读操作。会话快照同时保留其他 QQ 入口用于展示。

账号、QQ UID、会话和消息标识保留字符串形式，不能经过浮点转换。Native 消息 ID 与本地请求 ID 分开保存。

## 方法

| 方法 | 参数 | 返回 |
|---|---|---|
| host.status | 无 | protocol_version、phase、account、epoch、seq、errors、capabilities；诊断包括 native_account_path 和 profile_dir |
| accounts.list | 无 | items：QQ 记住的 account、name；不返回凭据 |
| events.subscribe | 无 | 当前宿主状态，并开始接收事件 |
| conversations.snapshot | account | account、epoch、查询起始 seq、conversations、可空 unread_total（原生主列表提醒计数） |
| conversations.children | account、群助手 peer | 群助手原生子会话列表 |
| conversations.read | account、epoch、原生 id、peer | 标记该会话／整个群助手分组已读；返回后须重读原生快照 |
| conversations.open | account、epoch、peer | 调用 QQ 原生添加最近会话 |
| contacts.list | account | items：Conversation 数组 |
| avatars.resolve | account、发送者原生 uid | 本机 path、资源 version |
| conversations.permissions | account、peer | state、scope、reason、可空 until、can_moderate |
| profiles.info | account、uid | uid、uin、nick、remark、signature |
| members.list | account、群 peer | items：uid、uin、name 数组 |
| messages.history | account、peer、before?／after?／around?、limit | messages、has_before、has_after、anchor_found；limit 为 1–100 |
| messages.get | account、peer、ids | messages |
| messages.forward | account、peer、path（1–8 层原生消息 ID） | 合并转发中的 messages，按原生顺序返回 |
| messages.send | account、epoch、peer、request_id、text、segments?、reply_to?、attachments | receipt |
| messages.receipt | account、peer、request_id、native_id?、message_id? | 核对后的 receipt |
| messages.read | account、epoch、peer | 原生调用结果 |
| messages.recall | account、epoch、peer、id | 原生调用结果 |
| messages.quote | account、peer、id（包含引用的消息 ID） | 返回被引用的消息记录；先按 ID 查询，必要时按原生序号查询 |
| messages.sequence | account、peer、seq | 原生序号对应的 messages，用于回应提示定位 |
| media.resolve | account、peer、id、element_id、forward_path? | path（本机文件绝对路径）、可选 warning（例如原图失效而退回缩略图） |
| stickers.list | account、favorites? | items；favorites=false 只读取 QQ 基础表情 |
| stickers.resolve | account、目录中的不透明 id | 收藏表情的本机文件路径 |
| reactions.set | account、epoch、群 peer、id、emoji_id、emoji_type、set | 原生提交结果；set 明确指定添加或取消 |
| reactions.members | account、群 peer、id、emoji_id、emoji_type、cookie? | members、下一页 cookie 或 null |
| reactions.refresh | account、群 peer、seq | 目标原生消息的最新状态 |

会话记录包含 peer、id、name、unread 字符串、pinned、preview、last_id 和 children。顺序沿用原生 sortedContactList，置顶来自 topFlag，摘要来自 abstractContent。

消息记录包含 account、peer、id、seq、time、sender、sender_uid、is_self、text、attachments、reply_to、reply_seq、send_status、correlation，以及 recalled 状态。notice_names 保存按 UID／QQ 号查询得到的通知参与者名称；reaction_notice 的 msgseq 引用必须排除共享该序号的灰条通知。新增可选 parts 按原生顺序保存文字与附件引用；forward 保存合并转发卡片的 XML。原生历史中的撤回灰条也会转换为 recalled，正文显示原生撤回操作者和可用提示，清除附件与引用；不依赖此前收到实时撤回事件。附件包含 element_id、kind、name、path、size；草稿附件可带仅供本地显示的 display_name。

商城表情提供 image 附件，media.resolve 依据原生 emojiId 读取 QQ 公共资源并缓存在私有状态目录。

QQ 默认表情在原始本地资源存在时提供 kind 为 emoji 的附件，parts 在原文位置引用它。text 保留规范化的文字后备用于复制、搜索和引用摘要；资源缺失或旧缓存缺少有序元素时使用文字后备。

合并转发通过 QQ 原生 getMultiMsg 读取。path 从当前会话中的根消息开始，逐层验证父记录包含目标子记录，最多八层。转发内容独立于会话时间线。

发送附件额外携带本机 path；图片带 width、height 和 kind=image，收藏图片表情使用 kind=sticker，基础表情使用 kind=face 与 element_id（QQ 表情 ID）。文字 segments 的普通段为 {"text":"正文"}，群成员段带 text、uid、uin。引用使用同一会话内的原生消息 ID。

receipt 包含 request_id、account、peer、native_id、message_id、state 和可选 error。state 为 submitting、submitted、confirmed、failed 或 unknown。

## 事件

通知为 {"jsonrpc":"2.0","method":"event","params":{...}}。params 包含 epoch、递增 seq、account 和 kind：

- notification：notification.items（key、title、body）为 QQ 原生 notifiedType=2 的实时重要提醒，保留免打扰群中的原生 @ 提醒，排除旧状态和自发消息；标题带原生未读数。
- recent.invalidated：重新读取原生会话快照。
- messages：携带新增或更新的 messages。
- message.recalled：携带 message_peer 和原生 message_seq，清除对应消息正文、附件及引用。
- messages.invalidated：重新读取当前会话消息。
- send.updated：携带 receipt。
- notices.updated：只更新对应通知的文字、parts 和 notice_names，不覆盖消息或回应状态。
- reactions.updated：只更新 messages 中对应消息的回应字段，保持正文。
- stickers.invalidated：QQ 表情资源更新，选择器需重新读取目录。
- status：宿主登录或在线状态变化。
- avatars.invalidated：头像原生状态变化，取消旧查询并刷新可见头像。
- permissions.invalidated：当前群权限可能变化，重新读取原生状态。

慢消费者超过队列或帧限制时连接关闭，客户端重新订阅并拉取快照。

设备传输 8 的图片沿用 QQ 原生文件元素；不接受引用或撤回请求。群助手未读使用 unreadChatCnt，聊天未读使用 unreadCnt。主列表和群助手列表分别订阅原生视图 1、2，保留服务给定的排序和窗口位置。

历史锚点 before、after、around 互斥，使用原生消息 ID 的十进制字符串。around 从同一消息的前后读取连续窗口；原消息不存在时返回最新页并设置 anchor_found=false。前后分页分别更新当前窗口的起点和终点标志；最新页和 around 同时初始化两个标志。宿主就绪时通过 history_navigation 能力声明该接口，通过 request_context 声明预期上下文校验，前端发现旧宿主时要求重新启动宿主。

epoch 在宿主启动或原生账号上下文改变时更新；账号切走后再切回也产生新的值。发送准备跨越此变化时不会调用原生发送，旧列表查询被取消，旧监听器不会重新参与通知。已保存的同一 request_id 回执仍按原请求去重返回。

回应记录包含 emoji_id、emoji_type、count（十进制字符串）、clicked、label、可选本机 path。emoji_type 显式使用 QQ 类型 1 或 Unicode 类型 2。reaction_notice 为已识别的原生回应通知 XML，客户端使用 XML 解析器读取目标序号，不显示为通用系统消息。

消息与元素标识在进入原生服务前检查无符号 64 位范围。所有修改操作要求预期 epoch；消息发送的已保存重复回执可以跨宿主代次读取，不触发再次发送。

合并转发返回的消息附带 forward_path（根卡片及嵌套记录 ID），资源请求须原样带回。路径最多八层，每层必须经原生 getMultiMsg 验证；相同 ID 位于不同记录时不合并下载上下文。消息的 system_notice 为原生灰条语义，reaction_notice 保留原生回应 XML 供有界解析与消息序号定位。

每个账号实例提供自己的 IPC 端点；账号选择在 CLI/TUI 启动层完成，RPC 连接始终只路由到一个实例，不在连接内隐式切号。

QQ 邮箱提醒的 arkElement 卡片解析 view=mail 下的发件人、主题、摘要与官方 mail.qq.com HTTPS 链接。邮箱提醒与普通聊天统一调用原生 setMsgRead(Peer)；群助手先逐个提交子会话的原生已读，再清除聚合入口。

## 版本与本地持久化

宿主状态带 protocol_version=2。前端在就绪前核对版本，版本不匹配时提示更新并重启宿主。联系人与成员使用项目 DTO。

头像 uid 使用消息的 sender_uid。为可见发送者调度查询，按账号、连接、epoch 和请求 token 接受结果；暂不可用时使用固定占位。version 来自原生本地文件状态，用于识别更新。

SpeechPermission.can_moderate 表示 QQ 原生群主／管理员身份，可申请撤回其他人的消息，最终权限由原生撤回接口校验。

SpeechPermission.state 为 allowed／blocked／unknown；scope 为 none／all／self／unknown，reason 表达可确认的原生限制，until 是可空的原生结束时间字符串。全员禁言结合本人成员角色判断豁免，个人禁言按实际到期时间判断。查询结果不能代替提交前的原生权限检查。

回执字符串状态保持不变。桥接 SQLite worker 在原生提交前事务性保存请求标识、关联标识、指纹和回执；已终结记录保留最小去重信息。首次从旧 JSON 日志备份并迁移，失败时拒绝提交；不因重启、客户端超时或结果未知而自动再次发送。
