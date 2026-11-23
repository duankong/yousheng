# 友声

在 Windows 上和朋友组队语音。每个人安装同一个客户端。任意玩家都可以在自己的电脑上开大厅，好友通过邀请加入。

「友声」取自《诗经·小雅·伐木》：“嘤其鸣矣，求其友声。” [诗经原文](https://ctext.org/book-of-poetry/fa-mu/zhs)

## 玩家使用

1. 第一次打开友声时填写昵称。之后点自己的头像可以修改昵称、选择本机图片或恢复默认头像。
2. 打开软件即可浏览保存在本机的频道。点击已有频道会自动开启本机大厅并加入，也可以从首页创建频道。
3. 复制大厅邀请发给好友。好友可直接打开邀请或在客户端粘贴链接。加入同一频道即可通话、发送文字。频道空了仍然保留。房主能删除自己的频道，开大厅的人能删除任意频道。
4. 打开「软件设置 → 声音」，选择麦克风和扬声器，调整音量、语音激活阈值、降噪和回声消除。发言方式有持续开麦、语音激活和全局按键说话。点队友的音量控件可以单独放大或静音对方。
5. 打开「软件设置 → 软件更新」，可以关闭「启动时自动检查更新」，也可以随时手动检查。发现新版后，退出或停止大厅，点「下载并安装」。首次安装可从 GitHub Release 下载 `yousheng-windows-x64-setup.exe`。[下载友声](https://github.com/duankong/yousheng/releases/latest)

停止大厅或关闭主机程序会结束所有人的会话。重新在同一 Windows 用户下开大厅，会恢复原频道和归属。文字只保存在开服者内存里，每个频道保留当局最近 100 条，停服即清空。玩家的音频偏好与身份保存在自己的 Windows 用户目录。

## 开发与验证

在 Windows x64 上安装 Node.js 22.12 或更高版本、Rust stable MSVC 和含 Windows SDK 的 Visual Studio C++ Build Tools。桌面端使用系统 WebView2。仓库根目录下运行：

```powershell
.\scripts\desktop.ps1 dev
.\scripts\desktop.ps1 test
.\scripts\desktop.ps1 test -Integration
.\scripts\desktop.ps1 build
```

脚本会下载锁定版本的依赖，并校验官方 LiveKit Server 和 WebRTC 压缩包。安装程序位于 `src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/`。

`test -Integration` 会启动随包的语音服务和独立客户端，并运行本机中转场景。桌面冒烟测试可将 `DUIPIN_DATA_DIR` 指向临时目录，以隔离测试身份与频道。

开发界面位于 `ui/src/features`。Rust 在 `src-tauri/src` 中按职责组织：`channels` 管理频道归属和删除规则，`chat` 管理当局消息，`lobby` 验证成员与签发语音凭证，`network` 承载大厅连接和媒体包，`voice` 负责音频采集、处理和播放。大厅运行时才启动随包的 LiveKit Server。

邀请默认使用 `duipin://`。聊天工具若不允许直接打开此协议，可以粘贴进客户端。要为自己的分发版本提供 HTTPS 邀请，可将 `invite/index.html` 托管在自己的 HTTPS 地址，并在构建前将环境变量 `DUIPIN_INVITE_BASE` 设为该页面地址。邀请凭证位于链接片段中。

连接优先尝试直连，也可经过 iroh 的共享中转。跨运营商的可达性、实际延迟和游戏开黑时的音质需要在目标网络和设备上测量。

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
