# qqtui

在终端里使用 QQ。

支持 Vim 风格操作、图片与表情、多账号登录。每个窗口使用一个账号，关闭窗口后 QQ 保持在线。

## 安装

支持 Arch Linux x86_64，推荐使用 Kitty。首次登录需要图形浏览器。

构建安装见 [构建指南](CONTRIBUTING.md#arch-打包)。

## 使用

运行 `qqtui`，选择账号并完成登录，即可开始聊天。若桌面 QQ 正在运行，请先退出。

按 `i` 输入，Enter 发送，Alt+Enter 换行。Esc 返回普通模式，`?` 查看帮助，`q` 退出。

更多操作和配置见 [使用指南](docs/USAGE.md)。参与开发请参阅 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 致谢

- [NapCatQQ](https://github.com/NapNeko/NapCatQQ/tree/bbd0d2aca3d470c20b33b1d4f95123335633bf26)：QQ 原生接口研究参考。
- [napcat-linux-launcher](https://github.com/NapNeko/napcat-linux-launcher/tree/557b5ea2de8867ae8d72f5054ab0cdd96c0aa9c7)：Linux 加载器参考。
- [QQNT-Toolbox](https://github.com/MeiYongAI/QQNT-Toolbox/blob/636e4a6cc7d57d6828de554b12032847cd10f6e3/src/main.js)：转发图片资源处理参考。
- [Concord](https://github.com/chojs23/concord/tree/0bdb321e7b8c9543fa2063035d17ac3b70ef2c25)、[iamb](https://github.com/ulyssa/iamb/tree/44e5a48be8414a5063cdbf79ac66e41455a0a78b)、[nchat](https://github.com/d99kris/nchat/tree/0bd238523e5af92cc9de863b38f8ef9a759f49ab)：布局与交互参考。

Rust 依赖及来源见 Cargo.lock。

## 许可证

第一方代码采用 [MPL 2.0](LICENSE)。
