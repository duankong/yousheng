<img src="ui/public/grovecall.png" alt="友声 GroveCall 图标，一只展翅抬喙的黑白小鸟" width="72" height="72">

# 友声 GroveCall

友声是一款 Windows 游戏组队语音软件。每位玩家都可以在自己的电脑上主持房间，发一条邀请链接，和朋友在房间里通话、发文字。

**[下载 Windows x64 安装包](https://github.com/duankong/yousheng/releases/latest/download/yousheng-windows-x64-setup.exe)** · [查看最新版本和源码](https://github.com/duankong/yousheng/releases/latest)

「友声」取自《诗经·小雅·伐木》中的“嘤其鸣矣，求其友声”。图标里抬起喙的小鸟，呼应了林间鸣鸟寻找同伴的画面。[诗经原文](https://ctext.org/book-of-poetry/fa-mu/zhs)

## 第一次和队友通话

1. **每个人**打开友声，第一次填写昵称。之后点击自己的头像，可以改昵称、换头像或恢复默认头像。
2. **房主**打开软件后，本机房间会自动开启。点「邀请好友 → 复制链接」，把链接发给队友。等好友进来，首页会切换到聊天和成员列表。
3. **队友**打开邀请链接，或在首页点「加入房间」并粘贴链接。进房后可以发文字，看到「语音已连接」后就能通话。

### 声音与设置

- **快捷调音**　首页和通话的右下角都提供麦克风和扬声器。悬停后可拖动滑块调整音量，单击图标可切换静音。调完扬声器音量时，会以新音量播放一声短促的试听音。
- **自己的声音**　在「软件设置 → 声音」选择麦克风和扬声器，调整音量、降噪和回声消除。修改会即时应用并记在本机。可选持续开麦、语音激活或全局按键说话。使用语音激活时可以调整触发阈值。
- **队友的音量**　点成员旁的音量控件，单独调大、调小或静音对方。这只影响你自己听到的声音。
- **软件更新**　在「软件设置 → 软件更新」可以手动检查更新，也可以关闭启动时自动检查。回到首页后可下载并安装更新，安装程序启动前会关闭空房。

### 房间与聊天记录

加入好友房间后，本机主持的房间会关闭。访客可以随时退出，房主可以关闭整个房间。成功加入过的好友房间会记在本机，之后可以从首页的「历史房间」快捷加入。

自己的发言在右侧，队友的发言在左侧。按 Enter 发送，Shift + Enter 换行。等待房主确认期间可以继续发下一条。如果提示「发送未确认」，先看聊天里是否已有这条消息，再决定是否把原文放回输入框。

房主关房或退出友声后，所有人的会话就会结束。**房间保留本次会话最近 100 条文字消息，关房后清空。** 每位玩家的身份和声音设置保存在自己的电脑上。

## 开发与验证

在 Windows x64 上安装 Node.js 22.12 或更高版本、Rust stable MSVC 和含 Windows SDK 的 Visual Studio C++ Build Tools。桌面端使用系统 WebView2。仓库根目录下运行：

```powershell
.\scripts\desktop.ps1 dev
.\scripts\desktop.ps1 test
.\scripts\desktop.ps1 test -Integration
.\scripts\desktop.ps1 build
```

脚本会下载锁定版本的依赖，并校验官方 LiveKit Server 和 WebRTC 压缩包。安装程序位于 `src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/`。

`test -Integration` 会启动随包的语音服务和独立客户端，并运行本机中转场景。桌面冒烟测试可将 `DUIPIN_DATA_DIR` 指向临时目录，以隔离测试身份、设置和历史房间。

开发界面位于 `ui/src/features`。Rust 的 `chat` 管理当局消息，`lobby` 管理房间成员与语音资格，`network` 承载连接和媒体包，`voice` 负责音频采集、处理和播放。本机主持房间时会启动随包的 LiveKit Server。

邀请默认使用 `duipin://`。聊天工具若不允许直接打开此协议，可以粘贴进客户端。要为自己的分发版本提供 HTTPS 邀请，可将 `invite/index.html` 托管在自己的 HTTPS 地址，并在构建前将环境变量 `DUIPIN_INVITE_BASE` 设为该页面地址。邀请凭证位于链接片段中。

连接优先尝试直连，也可经过 iroh 的共享中转。打开连接详情，可以查看最近一条文字消息的确认耗时。访客还可以看到当前走直连还是中继，以及到房主的网络往返时间。

## Windows 发布

普通本地构建用 `.\scripts\desktop.ps1 build`，可直接生成供本机安装的程序。正式发布用 `.\scripts\desktop.ps1 build -SignedRelease`，同时生成安装包和应用更新签名。构建前将 `TAURI_SIGNING_PRIVATE_KEY` 指向与项目更新公钥配对的私钥文件。如果密钥设有密码，将密码放入 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 环境变量。保管好私钥和密码，后续版本需要使用同一把密钥签名。[Tauri 更新签名文档](https://v2.tauri.app/plugin/updater/)

从发布提交重新构建并测试，用带注释的标签 `v<版本号>` 标记该提交。在仓库根目录运行以下示例，将版本号和路径替换成本次发布的值。`-OutputDirectory` 指定一个新目录。

```powershell
$version = (Get-Content src-tauri/tauri.conf.json -Raw -Encoding UTF8 | ConvertFrom-Json).version
$installer = "src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/友声_${version}_x64-setup.exe"
.\scripts\release-assets.ps1 -Revision "v$version" -Installer $installer -OutputDirectory (Join-Path $env:TEMP "yousheng-$version") -Notes '本次版本的更新说明'
```

脚本读取安装包旁的 `.exe.sig` 并输出四个文件。将它们上传到对应标签的 GitHub Release，把正式版标为 Latest，客户端即可从该 Release 的 `latest.json` 发现新版本。

| Release 文件 | 用途 |
| --- | --- |
| `yousheng-windows-x64-setup.exe` | 玩家直接安装，也是客户端下载并校验签名的安装包 |
| `yousheng-<版本号>-windows-x64.zip` | 同一安装包的压缩版 |
| `yousheng-<版本号>-source.zip` | 发布提交的完整受版本控制源码，包含依赖锁文件和构建脚本 |
| `latest.json` | 更新版本、说明、签名和该版本的安装包地址 |
