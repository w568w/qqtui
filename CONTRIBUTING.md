# 开发与检查

项目保留一个 Rust crate；模块职责见 [架构](docs/ARCHITECTURE.md)，原生接口见 [桥接协议](docs/PROTOCOL.md)。桥接源码和兼容性数据在 bridge/，修改后需要重新构建二进制。

## 源码构建

Arch 开发环境需要 base-devel、Rust（至少 Cargo.toml 中的 rust-version）、SQLite、Node.js 和 Python。运行 `cargo build --locked` 构建，默认输出为 target/debug/qqtui。Cargo.lock 固定 Rust 依赖；Linux 链接系统 SQLite，C 工具链用于构建 QQ loader。

完整 QQ 宿主的运行依赖见 [AUR 配方](https://aur.archlinux.org/cgit/aur.git/tree/PKGBUILD?h=qqtui)。运行环境与首次登录见 [README](README.md)，平台适配见 [维护约定](docs/MAINTAINABILITY.md)。

## 检查

`python scripts/check.py` 执行格式、Rust 测试、Clippy、桥接及 loader 检查。终端与软件包集成检查的附加依赖和命令见 [验证方法](docs/VALIDATION.md)。也可单独执行 `cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`node --test tests/native.test.cjs tests/journal.test.cjs` 和 `python tests/loader.test.py`。

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
