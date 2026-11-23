# 友声

在 Windows 上和朋友组队语音。每个人安装同一个客户端。任意玩家都可以在自己的电脑上开大厅，好友通过邀请加入。

「友声」取自《诗经·小雅·伐木》：“嘤其鸣矣，求其友声。” [诗经原文](https://ctext.org/book-of-poetry/fa-mu/zhs)

## 玩家使用

1. 第一次打开友声时填写昵称。之后点自己的头像可以修改昵称、选择本机图片或恢复默认头像。
2. 打开软件即可浏览保存在本机的频道。点击已有频道会自动开启本机大厅并加入，也可以从首页创建频道。
3. 复制大厅邀请发给好友。好友可直接打开邀请或在客户端粘贴链接。加入同一频道即可通话、发送文字。频道空了仍然保留。房主能删除自己的频道，开大厅的人能删除任意频道。
4. 在「声音设置」选择麦克风和扬声器，调整音量、语音激活阈值、降噪和回声消除。发言方式有持续开麦、语音激活和全局按键说话。点队友的音量控件可以单独放大或静音对方。

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
