# 使用 zDesk

[English](guide.md) | 简体中文

网关[安装](install.md)好之后，运维者和用户需要了解的内容：如何访问页面、会话以哪些选项启动、各类目标如何设置，以及本地多实例控制面。所有配置项都在带注释的 [`zdesk.example.toml`](../zdesk.example.toml) 中，其背后的设计见 [Architecture](architecture.md)。本页链接的其他文档都只有英文版。

## 客户端

客户端就是网关提供的页面，编译在网关的二进制文件中。它需要：

- **安全上下文。** 网关只使用纯 HTTP，没有 TLS 监听器，因此需要通过回环地址（`localhost`、`127.0.0.1`、`[::1]`、任何 `.localhost` 名称）或终止 TLS 的反向代理访问。通过纯 `http://` 访问局域网地址会被拒绝，页面会说明原因。
- **WebCodecs**，用于远程以视频形式发送的画面：`VideoDecoder` 用于 wlshare 的 VP9、高性能模式 Mac 的 HEVC 或 H.264，以及 RDP 主机的 H.264。没有它的浏览器，或解码器拒绝某种编码的浏览器，仍然可以启动：选择器会把需要它的选项置灰并说明原因，而网关的图块、标准模式 Mac 的 Adaptive 画面以及不含 H.264 的 RDP 管道在任何浏览器中都能工作。`AudioDecoder` 播放 Opus、Mac 的 AAC-ELD 和 FLAC；没有它的浏览器（Safari 16.4 到 18）不播放声音，更新的 Safari 的 `AudioDecoder` 无法解码 FLAC，可以播放 Opus 和 Mac 的声音。

桌面使用时，请在 Chrome 或 Edge 中把页面安装为应用。应用窗口会把普通标签页自己保留的组合键（⌘W、⌘T、⌘N 等）交给页面。在标签页中，**Menu → Immersive full screen** 也能做到这一点，并且还能拿到窗口通常拿不到的按键，例如 Super 键和 Alt+Tab。

桌面按 100% 显示，从不缩放以适应窗口：较大的桌面会滚动；而在以跟随窗口方式启动的会话中，则由窗口决定远程的尺寸。手机或平板按宽度适配并支持双指缩放，手指作为触控板，并提供软键盘。

已安装的应用有 **Window → Size to _width_×_height_**，它通过 [`window.resizeTo()`](https://developer.mozilla.org/en-US/docs/Web/API/Window/resizeTo) 把本地窗口调整为远程桌面的尺寸。它只调整本地窗口，从不缩放或调整远程桌面。它在已安装的 Chrome 和 Edge 应用窗口中有效，在其他浏览器中尽力而为；普通标签页无法调整窗口尺寸，移动浏览器会忽略这一请求。

在 macOS 上，窗口的圆角会遮住页面，因此与桌面同尺寸的窗口会丢失帧缓冲四角的像素。圆角半径是系统范围的设置；设为 1 点就足够方正，每个像素都能看到：

```sh
defaults write -g NSConvolutionOverride1 -float 1
```

退出并重新打开浏览器应用后生效。`defaults delete -g NSConvolutionOverride1` 恢复 Apple 的半径：macOS 26 上为 26 点，之前为 10 点。

## 启动会话

登录后页面显示目标选择器（picker）。点选一个目标会将其展开，其下显示选项，点击 Start 即以这些选项连接。这些选项按会话选择，在会话期间保持不变，并由浏览器按目标记住；它们都不是配置项。

| 目标 | 窗口决定尺寸 | 声音 | 直通 | 图形 |
|---|---|---|---|---|
| `rdp` | 是 | 关、Opus 或 FLAC | 图形管道，始终直通 | 高质量或自适应 H.264 |
| `rdp` 且 `egfx = false` | 否 | 关、Opus 或 FLAC | — | — |
| `vnc` | 否 | 无 | — | — |
| `vnc`，`wlshare` | 是 | 关、Opus 或 FLAC | 其 VP9，或其带运动视频的图块，始终直通 | — |
| `vnc`，`ard` | 否 | 无 | Mac 的 Adaptive 画面直通（beta），或不直通：网关对 Mac Full 质量编码的图块 | — |
| `vnc`，`ard` 且 `virtual_display` | 是 | 无 | 同 `ard` | — |
| `vnc`，`ard-high-performance` | 是 | 始终传输 | Mac 的 HEVC，或其 H.264（alpha），始终直通 | — |

- **尺寸（Size）。** 桌面要么保持一个固定尺寸，即目标的 `size = "1920x1080"`，未设置时为 1440×900；要么跟随窗口：要求远程按窗口的尺寸和浏览器的像素密度渲染，而不是在客户端缩放。固定尺寸同样跟随浏览器的密度。手机只提供固定尺寸。普通 `vnc` 目标以及使用 Mac 物理显示器的 `ard` 按远程自身的尺寸显示，不接受 `size`。
- **声音（Sound）。** Opus，或无损的 FLAC。不带声音启动的会话不向远程请求声音，因此主机会继续在原来的地方播放。会话中的 Mute 和 Unmute 只改变本浏览器是否收听。手机、平板和 Safari 以静音启动。没有 `AudioDecoder` 的浏览器会把 Sound 置灰；Safari 的 `AudioDecoder` 无法解码 FLAC，会把 FLAC 置灰，因此在那里勾选 Sound 即使用 Opus。在高性能模式 Mac 上，这类浏览器中菜单的 Audio 按钮显示 *Audio not supported*，无法解码 Mac 的 AAC-ELD 的浏览器中则显示 *AAC-ELD not supported*。
- **直通（Passthrough）。** 把远程自己的流原样发送给浏览器，而不是由网关编码成图块。高性能模式 Mac 的画面始终直通，可以选择的是哪一路流：其 HEVC，浏览器无法接收时置灰；或其 H.264（alpha），它提供给所有具备 `VideoDecoder` 的浏览器，页面中不需要 HEVC 解码器。在启用管道的 `rdp` 目标上，这不是一个选项：选择器会说明它始终直通，没有可勾选的框，无法合成管道的浏览器不能启动该目标。直通的 RDP 管道会跟随浏览器的链路：浏览器跟不上时，Windows 主机会绘制更少、更小的帧。Mac 直通的流则不会：Mac 按自己的速率发送，静止画面很少，运动时很多。在 `ard` 目标上，Picture 组为 *Apple's Adaptive (beta), passed through*（默认）或 *Gateway adaptive, settles to lossless*。Apple's Adaptive 是 Mac 自己的 8×8 图块，由本页面解码：每个变化区域先来一遍粗略的，随后是它的细化；在任何链路上精度都相同，较慢的链路只是更新更少，并在画面静止后才得到细化；它永远达不到无损。以它启动的会话在浏览器回来时会重新开始而不是恢复，与直通的 RDP 管道一样。Gateway adaptive 是 Mac 的无损 Full 质量，由网关解压后按 `video_quality` 以图块发送，与它发送任何 VNC 服务器的桌面一样：区域变化时用 JPEG，稳定后变为精确，适配链路，并在浏览器回来时恢复。在 Combined Display（见下文）下 Apple's Adaptive 会被置灰，因为 Combined Display 同样是网关的图块。在 `wlshare` 目标上，Picture 组为 *Video, passed through*（默认），即 wlshare 对整个桌面编码的 VP9；或 *Tiles with motion, passed through*，即 wlshare 自己的图块，适用于局域网：见 [wlshare](#wlshare-vnc-subtype--wlshare)。
- **显示器（Displays）。** 在共享 Mac 物理屏幕的 `ard` 目标上：*One at a time*（beta，默认）从主屏幕开始，在显示器菜单中提供其他屏幕，画面采用上面选择的方式；*Combined*（alpha）同时显示所有屏幕，相当于 Apple 查看器的 Combined Display，不提供单个屏幕。Combined Display 向 Mac 请求 Full 质量，由网关解码、合成并按 `video_quality` 以图块发送，与网关编码的任何画面一样跟随浏览器的链路。在 1x 浏览器上，2x 屏幕由网关缩小为 1x，因此只占用其自身像素四分之一的链路；2x 浏览器收到 Mac 原生的像素，1x 屏幕由浏览器自己放大绘制。仅屏幕共享的标准模式提供。见 [A passed stream and the browser's link](architecture.md#a-passed-stream-and-the-browsers-link)。
- **图形（Graphics）。** RDP 主机如何绘制其管道。*High quality*（默认）使用管道中除 H.264 以外的编码，适用于局域网：慢速链路只会得到更少的帧，而不会更粗糙。*Adaptive*（alpha）是主机按浏览器链路调整的 H.264：慢速链路更粗糙，快速链路更精细。其下，*H.264 full screen*（默认）用 H.264 绘制整个桌面，*H.264 region* 只对像视频那样运动的部分使用 H.264，其余部分按 High quality 的方式绘制，文字因此保持清晰，但自适应较少。在 `VideoDecoder` 无法解码主机 H.264 的浏览器中，Adaptive 会被置灰。要让主机无损绘制，见[精确画面](#精确画面)。
- **摄像头（Camera）** 和 **麦克风（Microphone）**（实验性）。在 `rdp` 或 `wlshare` 目标上，这两个勾选项向远程提供本浏览器的设备，之后在会话的菜单中打开：见[摄像头和麦克风](#摄像头和麦克风)。

一个网关只持有一个会话。重新加载或连接断开通常会恢复会话；启用图形管道的 RDP 会话则会重新连接引擎，因为浏览器的合成器状态已经丢失。另一个浏览器若发现所有者仍连接着，会被询问是否接管；在所有者重新连接的宽限期内到达的浏览器可以直接占用该槽位。两种情况都从选择器开始，使用它自己的选项。远程会保留自己的桌面，因此再次启动该目标会回到相同的窗口。

见 [What a session is started with](architecture.md#what-a-session-is-started-with)。

## 目标密码

目标从不写出自己的密码。它只给出密码的名称，配置在 `[passwords]` 中以该名称保存加密后的密码：

```toml
[[targets]]
name = "workstation"
protocol = "rdp"
host = "192.0.2.10"
username = "Administrator"
password_ref = "workstation"
```

这两个表由命令输出供你粘贴，从不手动输入：

```sh
zdesk password master -c zdesk.toml     # 每个配置一次：输出 [master]
zdesk password encrypt -c zdesk.toml    # 输出 [passwords] 的一个值
```

```toml
[master]
salt = "…"
check = "…"

[passwords]
workstation = "$ZD;1;AES-256-GCM;…"
```

`password encrypt` 会要求输入两次密码，或从管道读取一行，并用配置中已有的 `[master]` 加密，它只读取配置。名称在粘贴时由你决定，也就是 `password_ref` 写的名称。`vnc_password_ref` 以同样方式引用 VNC 服务器自己的密码。两个输出中都没有机密信息，因此可以放在任何组装配置的地方：编辑器、Helm values 文件、模板化的 ConfigMap。本地实例的配置由 TUI 负责写入：它的 `p` 会询问名称和密码，并自己写入两个表。

值也可以留空，`workstation = ""`，作为尚未设置的密码的占位符：在粘贴真实的值之前，目标使用空密码应答；如果配置中的名称全是占位符，就不需要 `[master]` 或主密码。

其他所有值都用 AES-256-GCM 加密，密钥由你的**主密码**和配置自己的 salt 派生。提供服务的网关从 `[server].master_password_file` 指定的文件读取主密码：可以是容器的 secret、systemd 的 credential，或只有网关账户能读的文件。该文件只包含主密码；末尾的换行会被忽略。TUI 把自己的主密码保存在钥匙串中（macOS 钥匙串、Windows 凭据管理器，或 Linux 桌面的 Secret Service：GNOME Keyring、KWallet），首次需要时生成，因此你从不需要输入。除 TUI 外没有任何东西读取钥匙串。

| 启动方式 | 主密码来源 |
|---|---|
| `zdesk serve` | `[server].master_password_file`；目标引用加密值而未设置它时拒绝启动，从不询问 |
| `zdesk tui` | 钥匙串；没有时生成并保存在那里；钥匙串无响应时拒绝启动，从不询问 |
| `zdesk password` | `[server].master_password_file`，否则询问 |

本地实例没有 `[server]` 块，因此其主密码总是来自钥匙串。

主密码错误会在启动时报告。更换主密码意味着在新的 `[master]` 下重新设置每个密码。

## Windows (`rdp`)

```toml
[[targets]]
name = "workstation"
protocol = "rdp"
host = "192.0.2.10"
username = "Administrator"
password_ref = "workstation"
```

RDP 客户端是网关自己实现的，针对 Windows 10 和 11 的远程桌面（NLA）测试过。它传输桌面、指针、键盘、鼠标、调整尺寸、剪贴板、声音，以及浏览器的摄像头和麦克风。不传输触摸。

- 主机通过图形管道（MS-RDPEGFX）绘制，每个会话都把它直通给浏览器合成。这几乎把画面的全部工作都从网关上拿走了。它需要 WebGL 2 和跨源隔离（cross-origin isolated）的页面；如果代理丢掉了网关的两个隔离响应头，就会破坏这一点；不满足条件的浏览器不能启动该目标。主机如何绘制管道，高质量或自适应 H.264（alpha），在选择器中选择。
- `egfx = false` 让主机改用位图更新绘制，由网关以图块发送：适用于无法合成管道的浏览器。这时桌面保持固定尺寸：没有管道时，Windows 通过重建共享来调整尺寸，而本网关不接受这种方式。
- **Alpha：** `virtual_displays = 2` 请求主机提供两个显示器，每个都是会话的尺寸。第二个相对第一个的位置在选择器中该目标下的 **Second display** 选择：右、左、上或下，与客户端自己的第二个显示器的位置对应。会话从浮动菜单 Display 选择器中的 *All Displays* 开始，它在此处保留第一个显示器，并在菜单的 Display 按钮下链接到第二个，即 **Open Display 2 ↗**：它在同一浏览器的新标签页中打开 `/display/2`，只显示该显示器，有自己的指针和键盘；再次点击会把那个标签页切到前台。选择器中的 `Display 1` 和 `Display 2` 在本页面单独显示一个显示器。在两个标签页之间拖过边缘的窗口会出现在另一个显示器上，指针也一样，无论它们的窗口如何摆放。当每个标签页都在各自的显示器上全屏时，边缘相接，窗口会出现在拖到的位置；两幅画面之间隔着什么，窗口就差出多少。选择 *All Displays* 时，Display 选择器和菜单的 **Info** 也提供同样的链接。页面打开时就显示该显示器。在跟随窗口的会话中，那个标签页的窗口决定显示器 2 的尺寸。它需要持有会话的浏览器的登录，并且一次只在一个标签页中显示：另一个标签页正在显示它时，新打开的标签页会提示它正在使用，并在 **Take over**（接管）之前询问，就像浏览器接管会话前会询问一样；被接管的标签页会说明这一点并提供 **Take it back**。那个标签页菜单中的 **Disconnect** 会断开它，单独选择一个显示器会关闭它。在它没有被显示时打开，会提示不可用。启用管道时，持有会话的浏览器只合成两个显示器一次并显示其中一个；第二个显示器的标签页从同一画面绘制，因此没有任何东西被解码两次。已在一台 Windows 11 主机上检验，其中两个显示器的直通只通过无头浏览器的套接字检验过，尚未用肉眼检查。

  第二个显示器的标签页有自己的菜单，位于一个显示该显示器编号 2 的按钮后面，而会话页面上这个按钮显示 1 而不是 ☰。菜单里只有该标签页自己的内容：**Immersive full screen**、该显示器的尺寸和密度，以及 **Disconnect**；其他所有内容都留在会话页面的菜单中。

  第二个显示器的标签页适用于有两个物理显示器的客户端，每个显示器显示一个。只有一个显示器的客户端不打开该标签页，在此处像只有一个显示器时那样使用第一个，或者选择一个显示器。手机或平板上不支持该标签页，尽管那里没有任何东西阻止你打开它。

### 精确画面

默认情况下，Windows 主机以有损方式绘制大部分桌面，使用 RemoteFX Progressive 和 ClearCodec 的 NSCodec，会让文字和颜色略微模糊。这由主机自己的策略决定，zDesk 中没有任何设置能覆盖它。要让它精确绘制每个像素：

1. 在主机上打开本地组策略编辑器（`gpedit.msc`），进入“计算机配置 > 管理模板 > Windows 组件 > 远程桌面服务 > 远程桌面会话主机 > 远程会话环境”（Computer Configuration > Administrative Templates > Windows Components > Remote Desktop Services > Remote Desktop Session Host > Remote Session Environment）。启用 *为 RemoteFX 自适应图形配置图像质量*（*Configure image quality for RemoteFX Adaptive Graphics*），并设为 **无损**（**Lossless**）。没有组策略编辑器的 Windows 版本可以在提升权限的命令提示符中通过注册表进行同样的设置：

   ```text
   reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services" /v ImageQuality /t REG_DWORD /d 1 /f
   ```

   如果域的组策略设置了该项，则以域的组策略为准。
2. 运行 `gpupdate /force`，然后启动新会话：已经在运行的会话保持它启动时的质量。
3. 在选择器中，把 **Graphics** 保持为 *High quality*。这样主机只使用精确的编码，即 planar 和未压缩的矩形，并原样传给浏览器。
4. 在会话中打开菜单的 **Info**：*Host's picture* 一行显示 *Lossless (Planar)* 或类似内容。*Lossy (…)* 则列出主机改用的编码，说明策略还没有作用到该会话。

无损的代价很高，因为主机也会精确绘制运动的内容：仅打开时的桌面就约 7.6 MB，窗口中的视频约每秒 19 MB，这还是主机自身压缩之前的数字；该压缩随数据一起到达浏览器，可让 planar 绘制减少约三分之一，滚动减少约一半。这适合局域网。在这样的主机上，自适应 H.264 不会改变任何东西：Lossless 下它不绘制 H.264。测量数据见 [Lossless, by the host's policy](rdp-client.md#lossless-by-the-hosts-policy)。

其他 RDP 服务器，比如较旧的 Windows 或 xrdp，可能碰巧能用，但未经测试。见 [The RDP client](rdp-client.md)。

## Mac (`vnc` 加 Apple 子类型)

```toml
[[targets]]
name = "mac"
protocol = "vnc"
subtype = "ard"   # 或 "ard-high-performance"
host = "192.0.2.11"
username = "andrew"
password_ref = "mac"
```

Mac 不需要安装额外的软件。为目标提供 Mac *账户*的用户名和密码，而不是屏幕共享密码：这样会选择 Apple Remote Desktop 认证，连接会进入该用户自己的屏幕，而不是登录窗口。Mac 必须在远程管理（Remote Management）的按用户访问列表中授予该账户观察（Observe）和控制（Control）权限；默认的“所有用户”设置会拒绝连接，报出的错误与凭据错误相同。见 [Remote Management access](apple-vnc-889.md#remote-management-access)。

- **`subtype = "ard"`** 是屏幕共享的标准模式：Mac 的物理显示器，一次一个或全部，按其自身密度显示。一次一个屏幕的会话以选择器中的 Adaptive（beta，默认）启动，它直通 Mac 自己的 Adaptive 画面供页面解码；或以 Gateway adaptive 启动，即网关对 Mac 无损 Full 质量编码的图块，`video_quality` 对它起作用。两者都只在虚拟 Mac 上试过。Combined Display（alpha）会话是网关同时编码所有屏幕的图块。它不支持调整尺寸和声音；Mac 继续在自己的输出设备上播放。
  - **连接了多个显示器的 Mac 最好设置 `virtual_display = true`**（见下文），它把一个显示器调整为浏览器窗口的尺寸。Combined Display 同时显示所有屏幕，按 100% 显示，因此只适合两个并排、能放进一个浏览器窗口的小显示器，再大就得不停地滚动。它没有画面上限：每个屏幕都完整显示，无论有多少个、加起来多大，都按浏览器密度对应的像素显示，因此 1x 浏览器上的 2x 屏幕仍由网关缩小（见上文）。只在虚拟 Mac 上试过。
- 所有 Apple 子类型都传输 Mac 原生的粘贴板。
- **`subtype = "ard-high-performance"`** 是 Apple 查看器中的高性能模式，属于**实验性**功能。Mac 会停用其物理显示器，把所有窗口放到一个由会话决定尺寸的虚拟显示器上，最大 3840×2160 后备像素。画面和声音通过 Mac 的媒体流传输，即通过 SRTP 的 HEVC 和 AAC-ELD：
  - **Alpha：** `virtual_displays = 2` 请求 Mac 提供两个虚拟显示器，即 Apple 查看器的“2 Virtual Displays”，第二个在第一个右侧：位置由 Mac 决定，因此选择器不提供其他方向。要放到其他方向，请在会话打开期间到 Mac 的“系统设置 → 显示器 → 排列”中调整：把第二个显示器拖到第一个的左侧、上方或下方。会话会跟着变化，每个标签页保持自己的显示器，指针穿过它们现在共享的那条边；Mac 可能会为下一次会话保留这一排列，没有保留时它又回到右侧。会话从 Display 选择器的 *All Displays* 开始，它在此处保留第一个显示器，并在同一浏览器的新标签页中打开 `/display/2` 显示第二个，与 RDP 目标一样，选择器也可以单独显示其中任一个；在跟随窗口的会话中，那个标签页的窗口决定第二个显示器的尺寸。Mac 为每个显示器发送一路流，第二路发到 UDP 端口 5902，网关只直通正在显示的显示器。与 RDP 一样，在两个标签页之间拖过边缘的窗口会出现在另一个显示器上；不建议在只有一个物理显示器的客户端上使用第二个显示器的标签页，手机或平板上也不支持。
  - Mac 向网关的 UDP 端口 5900 和 5901 发送数据，有第二个显示器时还有 5902，因此两者之间的防火墙或 NAT 必须放行。
  - Linux 网关的 `net.core.rmem_max` 至少要 4194304，供画面的套接字使用，低于该值时日志会发出警告：默认的 212992 在 Retina 尺寸下会丢失关键帧。
  - 网关不解码 Mac 的任何一路流：画面以选择器中选择的 HEVC 或 H.264 直通。
  - H.264 直通不需要在页面中添加任何东西：网关请求 Mac 用 H.264 代替 HEVC，浏览器用自己的解码器解码，而能解码 H.264 的浏览器比能解码 HEVC 的多。因此默认构建无需软件解码器就能为任何浏览器（包括 Firefox）提供该目标，选择器也向所有浏览器提供 H.264。
  - HEVC 直通只有在页面的启动探测找到解码路径时才发送 HEVC。支持情况取决于浏览器、平台和 GPU，因此以选择器为准。网关还可以提供 [`zdesk.example.toml`](../zdesk.example.toml) 中描述的可选软件画面解码器。
  - Mac 推流时会把自己的扬声器静音，因此会话始终传输 AAC-ELD 声音。页面会单独探测它；如果无法解码声音，会话会在没有音频的情况下继续，并在菜单中说明原因。
- **非官方：** 在 `ard` 目标上设置 `virtual_display = true`，会在这样的虚拟显示器上以标准模式打开，支持调整尺寸，但没有媒体流和声音，期间物理显示器被停用。这是访问连接了多个显示器的 Mac 的推荐方式，因为超过两个小屏幕时，Combined Display 在浏览器中并不实用。Apple 的查看器从不提供这种组合；只在 macOS 26 上测试过。

Apple 没有公开这一协议的任何文档。两种模式都是逆向工程得来的，只在测量过的 Mac 上保证正确，macOS 的更新随时可能改变其中任何部分。见 [Apple RFB 003.889, as measured](apple-vnc-889.md)。

## wlshare (`vnc`, `subtype = "wlshare"`)

```toml
[[targets]]
name = "wayland"
protocol = "vnc"
subtype = "wlshare"
host = "192.0.2.12"
username = "me"
password_ref = "wayland"
```

[wlshare](https://github.com/andrewtheguy/wlshare) 是本项目自己的 VNC 服务器，面向基于 wlroots 的 Wayland 桌面。这个子类型让网关列出 wlshare 的私有 RFB 扩展：它的 VP9 流，原样直通并随浏览器链路自适应；像素密度；输出列表；声音；以及在勾选后启动的会话中，作为桌面上 PipeWire 设备的浏览器摄像头和麦克风。不设置子类型时，同一服务器按任何 VNC 服务器的方式读取。

- **画面（Picture）。** 选择器的 Picture 组为 *Video, passed through*（默认）：wlshare 对整个桌面编码的 VP9，按浏览器的链路调整。或 *Tiles with motion, passed through*：静止的部分为精确的 PNG，每个运动区域有自己的一路 VP9 流，停下后再精确地重发。图块适用于带宽充裕的链路：wlshare 花在图块上的时间只是 VP9 的一小部分，最终画面是精确的，代价是数倍的字节数，大面积运动时达每秒数十 Mbit。其运动区域按 `video_quality` 编码，`render_adaptive` 对它们不起作用。

- **Alpha：** 恰好有两个输出（显示器或无头输出）的桌面会列出 *All Displays* 并从它开始：第一个留在此处，第二个在同一浏览器的新标签页中打开 `/display/2`，有自己的指针和键盘，与有两个虚拟显示器的 `rdp` 目标一样。`virtual_displays = 2` 请求 wlshare 提供两个，就像请求 Windows 主机或 Mac 那样：在 sway 上，wlshare 会让合成器创建第二个无头输出，以两个启动的会话在请求一个（默认）时保持一个；在其他合成器上，输出就是合成器已有的那些。

示例使用 wlshare 运行所用的账户，通过 RSA-AES 上的 PAM 校验。如果 wlshare 改用 `password_file`，请把该服务器密码放在 `vnc_password_ref` 下，并省略 `username` 和 `password_ref`。

见 [密度](wlshare-density.md)、[输出](wlshare-outputs.md)、[音频](wlshare-audio.md)、[摄像头](wlshare-camera.md) 和 [麦克风](wlshare-microphone.md)。

## 其他 VNC 服务器 (`vnc`)

```toml
[[targets]]
name = "workstation-vnc"
protocol = "vnc"
host = "192.0.2.13"
vnc_password_ref = "workstation-vnc"
```

普通目标用 `vnc_password_ref` 进行经典 VNC 认证，或用 `username` 和 `password_ref` 进行 RSA-AES 认证。网关请求 ZRLE，所有现行的 VNC 服务器都支持它，除此之外只读取 Raw；较老的标准编码、Tight 以及其他厂商的或有损的编码都不在列表中。目标按服务器自身的尺寸、以 1x 显示，不传输声音、摄像头或麦克风。标准 RFB 无法表明其像素是 HiDPI；见 [HiDPI over standard RFB](standard-rfb-hidpi.md)。

## 摄像头和麦克风

**Camera** 和 **Microphone** 是 `rdp` 或 `wlshare` 目标在选择器中的勾选项，用于向远程提供本浏览器的摄像头和麦克风。两者都是**实验性**的，默认关闭，并像其他选项一样按目标记住；设备本身在每个会话中从浮动菜单打开。它们面向临时需要的场景，例如一次通话或录制，并以尽可能低的开销发送：浏览器的 H.264 不经转码直通，以及低固定比特率的单声道语音 Opus。Apple 目标和普通 `vnc` 目标都不提供这两项。

在 RDP 上，摄像头需要一台会重定向摄像头的 Windows 主机：工作站版本的 Windows，或安装了远程桌面会话主机角色的 Windows Server。Windows 主机只有在其上有程序录音时才会启动麦克风。

## 本地实例

原生安装包含一个终端控制面，可以在一台机器上运行多个网关，每个都有自己的配置和浏览器源（origin）：

```sh
zdesk tui
```

它只监听回环地址，默认端口为 52380，`--port` 或 `ZDESK_TUI_PORT` 可以更改；端口已被其他程序占用时拒绝启动。打开 <http://zdesk.localhost:52380>；每个运行中的实例位于 `http://<instance>.zdesk.localhost:52380`。

| 按键 | 操作 |
|---|---|
| `↑`/`↓`、`k`/`j` | 选择实例 |
| `n` | 创建实例 |
| `e` | 用 `$VISUAL` 或 `$EDITOR`（`vi`，Windows 上为记事本）编辑其 `zdesk.toml`；编辑会经过检查，在实例重启后生效 |
| `p` | 把密码加密到其 `[passwords]` 中，会询问名称和密码；没有 `[master]` 的配置会先得到一个 |
| `s`、`x`、`r` | 启动、停止、重启实例 |
| `a` | 启动所有已停止的实例 |
| `o` | 在浏览器中打开运行中的实例 |
| Enter | 显示其设置 |
| `R` | 重新扫描实例目录 |
| `q`、Esc | 停止所有实例并退出 |

实例根目录下的每个直接子目录就是一个实例：Linux 上为 `~/.local/share/zdesk/instances`（或 `$XDG_DATA_HOME/zdesk/instances`），macOS 上为 `~/Library/Application Support/zdesk/instances`，Windows 上为 `%LOCALAPPDATA%\zdesk\instances`；`--instances-dir` 可以指定其他目录。根目录只有你的账户能访问，因为配置中包含凭据。实例的配置与网关的配置有相同的 `[branding]` 和 `[[targets]]`，但没有 `[server]` 块：TUI 占用端口，并把每个子域名代理到该实例私有的 Unix 套接字或命名管道。

引用了加密密码的配置需要主密码才能启动（见[目标密码](#目标密码)）。TUI 从钥匙串中获取主密码，首次时在那里生成，然后只把每个 worker 自己配置的密钥交给它。从另一个账户的钥匙串复制来的实例配置在本账户下打不开：删除其 `[master]` 和 `[passwords]`，再用 `p` 重新设置每个密码。

私有文件和 worker 端点可以防止其他本地用户拿到凭据，但共享的回环监听器不验证操作系统用户：机器上的任何账户都可以打开首页并操作运行中的实例。不要在与不应访问这些桌面的用户共享的机器上运行 `zdesk tui`。

见 [Local multi-instance control plane](architecture.md#local-multi-instance-control-plane)。
