# zDesk

[English](README.md) | 简体中文

在浏览器中使用远程桌面，连接的是各平台自带的远程桌面服务器。zDesk 是一个小型网关，运行在你的机器旁边。它可以连接：

- **Windows 10 和 11 自带的远程桌面**，通过 RDP 及其现代图形管道。
- **macOS 内置的屏幕共享**，两种模式都支持：标准（Standard）和高性能（High Performance）。
- **Linux**，通过 [wlshare](https://github.com/andrewtheguy/wlshare)，这是我们自己为 wlroots 合成器编写的 VNC 服务器。
- 其他 RDP 和 VNC 服务器，通过各协议的基础功能。

Windows 和 macOS 主机无需安装任何软件，只需打开系统自带的共享功能。网关提供一个显示桌面的网页，因此任何有浏览器的设备，无论是 Windows、macOS、Linux、iOS 还是 Android，同样无需安装客户端即可访问上述所有桌面。

```text
浏览器 ── HTTPS 或 localhost ── zDesk ── RDP / VNC ── 你的桌面
```

`zdesk serve` 是单用户的：一个登录、一份目标列表、一个活动会话。原生安装包还包含 [`zdesk tui`](docs/guide.zh-CN.md#本地实例)，这是一个本地控制面，可以并排运行多个相互独立的网关实例。

> [!WARNING]
> **不保证向后兼容。** zDesk 的[方向](docs/architecture.md#direction)仍在调整中，任何版本都可能更改或移除某个配置项、WebSocket 协议或某项功能，且不再读取旧的写法。升级时请对照 [`zdesk.example.toml`](zdesk.example.toml) 检查你的配置。页面和网关属于同一个版本：升级时仍打开着的标签页会显示两者的版本号，并提示重新加载。

## 演示

### Windows -> 带一个远程虚拟显示器的 Mac

https://github.com/user-attachments/assets/982bf2d0-3fac-4a3c-8213-27412d10977d



### Windows -> 带两个远程虚拟显示器的 Mac

- 两个远程虚拟显示器分别显示在不同的浏览器标签页中，窗口可以在两个标签页之间直接跨显示器拖动。

https://github.com/user-attachments/assets/a31030c8-dab9-467c-9231-20329e138e7d




### iOS -> Windows
https://github.com/user-attachments/assets/21b869ad-d353-4c64-aac2-9b980893dd2f



### iOS -> Mac


https://github.com/user-attachments/assets/cde4e192-340f-47dd-86db-3d2d89049007



### Mac -> Windows，Retina/HiDPI 密度并带声音

- 以 Mac 的 Retina/HiDPI 密度显示 Windows 自带的远程桌面，并在浏览器中播放远程的声音。

https://github.com/user-attachments/assets/3dfd0dc2-dc7d-4d41-b99e-1340f685390c



### Mac -> Linux（Sway 上的 wlshare），Retina 密度并带声音

- 以 Mac 的 Retina 密度显示 Sway 上的 wlshare，带远程的声音，进入和退出沉浸式全屏时远程桌面会随浏览器调整大小。

https://github.com/user-attachments/assets/68b73718-9406-4003-a374-2829f2536395



## 功能

- **单一的浏览器客户端。** 只需要安全上下文（secure context）和 WebCodecs，无需安装任何东西：桌面上的 Chrome、Edge 或 Safari，Android 上的 Chrome，以及 iPhone 或 iPad 上的 Safari。Firefox 也能运行，但它的原生解码器既不支持高性能模式 Mac 的 HEVC，也不支持其 AAC-ELD，因此在 Firefox 中 Mac 的画面使用其 H.264，或在页面的软件解码器中使用其 HEVC。桌面使用时，建议将页面安装为 Chrome 或 Edge 应用。
- **按远程尺寸、感知像素密度的显示。** 使用默认图形管道的 RDP、wlshare 以及 Mac 的虚拟显示器可以按屏幕像素密度跟随浏览器窗口改变尺寸。Mac 的物理显示器按其报告的密度显示。标准 VNC 无法报告密度，按 1x 显示。
- **高性能模式 Mac 上两个可调整尺寸的虚拟显示器（alpha）。** 每个显示器有自己的浏览器标签页，每个标签页的窗口决定其显示器的尺寸。macOS 26 上 Apple 自己的查看器不提供这一功能：有两个虚拟显示器时，它会提示动态分辨率不可用。
- **远程自己的画面在浏览器中解码，或使用网关的图块。** Windows 主机的图形管道直通给浏览器合成，在局域网下以高质量绘制，或（alpha）以主机按浏览器链路调整的 H.264 绘制。高性能模式 Mac 的 HEVC 或 H.264 始终原样直通，两者都不随浏览器链路自适应：静止画面数据量很小，运动画面则很大。H.264 不需要页面中的 HEVC 解码器，因此任何构建都能让任意浏览器访问高性能模式 Mac。标准模式 Mac 自己的 Adaptive 画面直通（beta），在页面中解码，并按浏览器链路调节节奏：精度保持不变，较慢的链路只是更新更少、细化更晚。wlshare 直通的 VP9 同样跟随浏览器链路。由网关编码画面时，例如普通 VNC 服务器，或标准模式 Mac 的 Full 质量（在网关解压的无损 zlib），网关发送 64×64 图块：颜色少的图块用精确的 PNG，运动区域用 JPEG，静止后再精确地重发一次，发送速度与浏览器的绘制速度一致，最终收敛到无损。
- **浏览器中的远程声音，自适应 Opus 或无损 FLAC。** RDP 或 wlshare 会话在本地播放远程的声音，可以是比特率跟随浏览器链路的 Opus，也可以是适合局域网的无损 FLAC；Safari 的解码器无法播放 FLAC，因此不向 Safari 提供。高性能模式 Mac 始终发送其 AAC-ELD 声音。标准屏幕共享和普通 VNC 不传输声音。
- **双向剪贴板文本。** RDP 和 Apple 屏幕共享支持 Unicode。VNC 在有 Extended Clipboard 时支持 Unicode，否则回退为 Latin-1。浏览器允许时使用浏览器剪贴板自动同步；不允许时仍可使用 Clipboard 面板。
- **实验性的摄像头和麦克风重定向。** 在选择器中勾选后启动的 RDP 或 wlshare 会话，可以接收浏览器的设备，用于通话或录制。
- **重新连接而不丢失远程桌面。** 拥有会话的浏览器在重新加载或套接字断开后，有 60 秒的宽限期重新连接。接管或之后的新连接会启动一个新的网关会话，但优先支持的服务器会保持其桌面运行，因此重新连接该目标即可回到之前留下的窗口。
- **鼠标和键盘输入，以及触屏界面。** 已安装的 Chrome 或 Edge 应用可以收到普通标签页保留的组合键。在手机或平板上，手指相当于触控板，支持双指缩放和软键盘；手指操作不会作为触摸屏触点发送到远程。

## 支持的服务器

zDesk 围绕各平台原生的远程桌面服务器构建：Windows 自带的远程桌面、macOS 内置的屏幕共享，以及 Linux 上我们自己的 [wlshare](https://github.com/andrewtheguy/wlshare)。Windows 和 macOS 优先：大多数人连接的是它们，所以功能首先在它们上面测试和优化。wlshare 紧随其后，不过它是三者中集成最无缝的，因为 RFB 缺少的功能都加到了它里面。

- **Windows 10 和 11 的远程桌面**（`rdp`）。按密度跟随窗口，传输 Opus 或 FLAC 声音，同时支持摄像头和麦克风。画面是其图形管道，直通给浏览器，可以是高质量，或（alpha）自适应 H.264。
- **Mac 的高性能屏幕共享**（`vnc`，`subtype = "ard-high-performance"`）。按密度跟随窗口，支持一个或两个虚拟显示器，始终传输 AAC-ELD 声音。画面是直通的 HEVC 或 H.264，由 Mac 按自己的速率而非浏览器链路发送：静止画面数据很少，运动时很多。H.264 无需任何额外组件，HEVC 则需要能解码它的浏览器或页面的软件解码器。
- **Mac 的标准屏幕共享**（`vnc`，`subtype = "ard"`）。Mac 的物理显示器，单个或全部，按其自身尺寸和密度显示，画面是网关对 Mac 无损 Full 质量编码的图块，或直通的 Mac 自己的 Adaptive 画面（beta）；不支持声音、摄像头和麦克风。
- **Linux 上的 wlshare**（`vnc`，`subtype = "wlshare"`）。由于服务器是我们自己的，RFB 缺少的功能都加到了它里面：它可以切换输出、按密度跟随窗口、传输 Opus 或 FLAC 声音以及浏览器的摄像头和麦克风，并自行编码 VP9 供网关直通，随浏览器链路自适应；或者在链路带宽充裕时，使用它自己的精确图块，并为每个运动区域提供一路视频流，VP9 或主机的硬件 H.264。可以同时显示两个输出，第二个在单独的浏览器标签页中（alpha）。

其他所有 VNC 服务器都是普通的 `vnc` 目标，通过 RFB 基础功能访问：一个 1x 帧缓冲，不调整尺寸，没有声音，由网关发送图块。其他 RDP 服务器可能碰巧能用，但未经测试。对比表、它们的共同点以及各自直通的内容，见 [Prioritized servers](docs/architecture.md#prioritized-servers)（英文）。

## 快速开始

从[最新发布](https://github.com/andrewtheguy/zdesk/releases/latest)安装适合你平台的软件包。在 Debian 13 或 Ubuntu 24.04 及更高版本上：

```sh
curl -fsSLO https://github.com/andrewtheguy/zdesk/releases/latest/download/zdesk-linux-amd64.deb
sudo apt install ./zdesk-linux-amd64.deb
```

用附带的示例创建配置，并生成登录凭据：

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/zdesk /var/lib/zdesk
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/zdesk/zdesk.example.toml /etc/zdesk/zdesk.toml
zdesk gen-passwd admin
```

把生成的值和一个目标写入 `/etc/zdesk/zdesk.toml`，并写上网关读取主密码的文件：

```toml
[server]
site_passwd = "admin:$2b$..."
master_password_file = "master-password"

[[targets]]
name = "workstation"
protocol = "rdp"
host = "192.0.2.10"
username = "Administrator"
password_ref = "workstation"
```

然后把主密码写入该文件，再粘贴目标的密码，它用主密码加密（见 [目标密码](docs/guide.zh-CN.md#目标密码)）：`password master` 输出配置所需的 `[master]` 块，`password encrypt` 输出要以 `workstation = "…"` 的形式添加到 `[passwords]` 下的值。

```sh
(umask 077; trap 'stty echo' EXIT; trap 'exit 1' INT HUP TERM; stty -echo; printf 'Master password: '; IFS= read -r pw; echo; printf '%s' "$pw" > /etc/zdesk/master-password)
zdesk password master
zdesk password encrypt
```

启动它，然后打开 <http://localhost:52380>：

```sh
zdesk serve
```

页面需要安全上下文，因此从其他机器访问时，要把网关放在终止 TLS 的反向代理后面；`localhost` 可以直接使用。

另有 `.rpm`、macOS `.pkg`、Windows `.msi` 软件包以及容器镜像。镜像的配置设置了 `[server].master_password_file = "/run/secrets/zdesk_master"`，主密码挂载在该路径；[Container](docs/install.md#container)（英文）介绍了如何创建该文件，以及如何用镜像输出配置的 `[master]` 和 `[passwords]`：

```sh
docker run -d --name zdesk -p 52380:52380 \
  -e ZDESK_LISTEN=0.0.0.0:52380 \
  -v ./zdesk.toml:/opt/zdesk/etc/zdesk.toml:ro \
  -v ./zdesk_master:/run/secrets/zdesk_master:ro \
  ghcr.io/andrewtheguy/zdesk:latest
```

[Installing zDesk](docs/install.md)（英文）涵盖所有平台、升级和卸载。

## 文档

- [使用 zDesk](docs/guide.zh-CN.md)：浏览器客户端、启动会话、设置 Windows、Mac、wlshare 和 VNC 目标、`zdesk tui`

以下文档只有英文版：

- [Installing zDesk](docs/install.md)：软件包、首次配置、升级、卸载
- [`zdesk.example.toml`](zdesk.example.toml)：所有配置项，带注释
- [Known issues](docs/known-issues.md)：值得认出、无需重新调查的已知问题
- [Roadmap](docs/roadmap.md)：计划做什么，以及不做什么
- [Architecture](docs/architecture.md)：设计及其约束、客户端协议、引擎
- [The RDP client](docs/rdp-client.md)、[Apple RFB 003.889](docs/apple-vnc-889.md)：各协议的实现和测量结果
- [Development](docs/development.md)：从源码运行、检查、端到端测试、构建
- [Packaging](packaging/README.md)：软件包布局、构建脚本、发布

## 开发

需要带 rustup 的 Rust 和 [Bun](https://bun.sh)。

```sh
bun install --cwd frontend
cp zdesk.example.toml zdesk.toml
cargo run -- gen-passwd admin
# 把生成的凭据粘贴到 zdesk.toml，然后：
cargo run -- serve -c zdesk.toml
```

Cargo 会构建前端并将其编译进二进制文件，因此网关是唯一提供页面的程序。检查命令：

```sh
cargo clippy --all-targets -- -D warnings
cargo test --lib
bun run --cwd frontend check
```

`tests/` 下的端到端测试默认被忽略：VNC 和 wlshare 的测试需要 Docker 或 Podman（`cargo test --test vnc_e2e --test wlshare_e2e -- --ignored`）。RDP 探针借用一台真实的 Windows 主机，因此它们是按需运行的示例而不是测试（`cargo run --example rdp_client_probe -- <case>`），无头浏览器测试见 [`tests/playwright`](tests/playwright/README.md)。详情见 [Development](docs/development.md)，发布构建见 [Packaging](packaging/README.md)。

## 许可证

zDesk 采用 [`LICENSE`](LICENSE) 中的 MIT 许可证。
