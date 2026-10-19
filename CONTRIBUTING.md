# 开发与检查

项目保留一个 Rust crate；模块职责见 [架构](docs/ARCHITECTURE.md)，原生接口见 [桥接协议](docs/PROTOCOL.md)。桥接源码和兼容性数据在 bridge/，修改后需要重新构建二进制。

## 源码构建

Arch 开发环境需要 base-devel、Rust（至少 Cargo.toml 中的 rust-version）、SQLite、Node.js 和 Python。运行 `cargo build --locked` 构建，默认输出为 target/debug/qqtui。Cargo.lock 固定 Rust 依赖；Linux 链接系统 SQLite，C 工具链用于构建 QQ loader。

完整 QQ 宿主的运行依赖见 [AUR 配方](https://aur.archlinux.org/cgit/aur.git/tree/PKGBUILD?h=qqtui)。运行环境与首次登录见 [README](README.md)，平台适配见 [架构](docs/ARCHITECTURE.md)。

## 检查

`python scripts/check.py` 执行格式、Rust 测试、Clippy、桥接及 loader 检查。`--terminal` 加入终端集成测试，`--package <pkg.tar.zst>` 加入包沙箱测试。

终端测试需要 Kitty、Xvfb、xauth 和 Python Pillow；桌面通知测试另需 python-dbus、python-gobject 及 D-Bus 工具。包沙箱测试需要 bubblewrap 和已安装 QQ；可直接运行 `python tests/package.test.py <pkg.tar.zst> --qq-path <QQ 安装根目录>` 指定安装位置。

行为变更附带相应回归。自动化测试使用临时目录和模拟标识，QQ 接入测试使用专用账号或会话。

## Arch 打包

Arch 包使用 [AUR 配方](https://aur.archlinux.org/cgit/aur.git/tree/PKGBUILD?h=qqtui)。prepare 阶段下载锁定依赖，build/check 使用 Cargo 的 frozen 模式。打包规范见 [Arch Rust 打包指南](https://wiki.archlinux.org/title/Rust_package_guidelines)。

先安装 base-devel、git，以及 AUR 中的 linuxqq、novnc 和它们的依赖，然后构建：

```sh
git clone https://aur.archlinux.org/qqtui.git
cd qqtui
makepkg -si
```

维护配方时，用 `makepkg --printsrcinfo > .SRCINFO` 同步元数据。

## 实机验证

使用专用账号或会话核对联系人、历史、权限、会话顺序、未读与跨设备同步。发送后核对 QQ 历史及回执，重启宿主后检查请求去重。升级时逐个恢复账号，核对草稿及发送记录。

完整安装测试在干净 Arch 环境中构建、安装，并以普通用户运行 qqtui 完成首次登录。包沙箱使用临时 HOME 和独立命名空间，挂载宿主的 /usr 和 /etc。Windows CI 检查前端与命名管道。
