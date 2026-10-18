# 使用指南

## 聊天

运行 `qqtui` 选择账号。首次登录时，在打开的 QQ 页面完成登录，然后回到终端按 Ctrl+C 进入聊天。

在左侧用 `j/k` 选择会话，Enter 打开；也可直接用鼠标点击。按 `i` 开始输入，Enter 发送，Alt+Enter 换行，Esc 返回普通模式。

普通模式下的常用操作：

| 按键 | 操作 |
|---|---|
| h/l、Tab | 切换面板 |
| Ctrl+P | 查找会话 |
| ]q / [q | 下一个／上一个未读会话 |
| G | 在消息区回到最新消息 |
| r | 回复选中消息 |
| gr、Ctrl+T | 跳到引用消息／返回 |
| gu | 跳到进入群聊时的最早未读消息 |
| a、Ctrl+V | 添加附件／粘贴 |
| Ctrl+E | 选择表情 |
| v / o | 预览图片／打开图片、文件或链接 |
| Space | 更多操作 |

`gu` 使用进入群聊时保存的未读起点，未读数归零后仍可跳转；按 `Ctrl+T` 返回原阅读位置。

按 `?` 查看当前快捷键。合并转发中同样用 `j/k` 选择消息、Enter 打开、Esc 返回。

发送失败时，按 Ctrl+O 查看记录或恢复草稿。若结果显示未知，先确认消息是否已发出再重试。

## 账号与后台

每个窗口使用一个账号；再开一个终端运行 `qqtui`，即可选择其他账号。也可用 `qqtui --account <QQ号>` 直接打开。

退出 TUI 后 QQ 继续在线。需要停止后台时运行 `qqtui backend stop`；需要打开 QQ 登录页面时运行 `qqtui backend view`。多账号时，后台操作会先让你选择账号；加 `--account <QQ号>` 可直接指定。

启动遇到问题，先运行 `qqtui doctor` 检查，再用 `qqtui backend logs -f` 查看日志。其他命令见 `qqtui --help`。

## 配置

配置文件位于 `~/.config/qqtui/config.toml`，可修改布局和按键。例如：

```toml
show_avatars = false

[keymap]
send = "enter"
newline = "alt-enter"
```

指定账号的配置位于 `~/.config/qqtui/accounts/<QQ号>/config.toml`。自定义 XDG_CONFIG_HOME 时，配置保存在该目录下。
