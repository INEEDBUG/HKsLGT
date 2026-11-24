<p align="center">
  <img src="packaging/windows/league-standalone/native-renderer/artwork/toolbox.svg" width="76" height="76" alt="HKsLGT 交叉双剑图标">
</p>

<h1 align="center">HKsLGT</h1>

<p align="center">英雄联盟海克斯大乱斗工具箱</p>

<p align="center">
  <a href="https://github.com/INEEDBUG/HKsLGT/releases"><strong>下载 Windows Beta</strong></a>
  · <a href="#安装与升级">安装与升级</a>
  · <a href="#智能应用控制">智能应用控制</a>
  · <a href="https://github.com/INEEDBUG/HKsLGT/issues">报告问题</a>
</p>

<p align="center"><code>Windows x64</code> · <code>Rust + WebView2</code> · <a href="LICENSE">MIT</a></p>

## 大乱斗里用得到的功能

| 时机 | 工具箱能做什么 |
| --- | --- |
| 开游戏前 | 从总览打开检测到的 TCLS 客户端或 WeGame。登录后可按 Riot ID 查询玩家。 |
| 大乱斗选人 | 在「自动选择 / 禁用」设置候选与优先顺序，分别控制 OP.GG 和 RESG 的开关。 |
| 海克斯大乱斗 | 当前英雄有可用数据时，RESG 可显示强化符文与出装。小型辅助面板可单独打开。 |

英雄联盟客户端未连接时，战绩、玩家查询和对局数据会等待客户端。OP.GG 和 RESG 的显示取决于游戏阶段与数据可用性。Beta 中仍可看到部分旧工具项，例如窗口大小调整、领取工具和好友工具，这些功能的 Rust 迁移尚未完成。

## 安装与升级

1. 从 [GitHub Releases](https://github.com/INEEDBUG/HKsLGT/releases) 下载最新 Beta 的 `MaxGameStudioLeague_*_x64-setup.exe`。可用 `.sha256` 文件核对下载。
2. 关闭旧工具箱，打开安装包。可选择「仅为我安装」或「为所有用户安装」。所有用户安装会请求管理员权限。
3. 从旧版升级时，保留安装器识别出的程序和数据目录，直接覆盖即可。自定义目录时，请给程序和数据分别选位置。全新安装的程序目录应为空。
4. 从总览或平时的入口启动英雄联盟并登录，再打开工具箱需要使用的页面。

旧版 MaxGameStudio League 的 Beta 用户可从原更新入口获取迁移版。安装迁移版后，工具箱通过 HKsLGT 仓库检查后续 Beta。

### 智能应用控制

**先看 Windows 版本。** 按 `Win + R` 输入 `winver`。智能应用控制从 **Windows 11 22H2（内部版本 22621）** 开始提供，是否启用可在 Windows 安全中心查看。版本号相同的设备也可能有不同的开关状态。[微软版本说明](https://learn.microsoft.com/zh-cn/windows/security/application-security/application-control/app-control-for-business/appcontrol)

当前 Beta 安装包没有 Windows 可信发布者签名。如果安装或启动时明确出现「智能应用控制已阻止可能不安全的应用」，请先确认下载来自本仓库的 Releases，并核对 SHA-256。确认要继续试用后，可以在允许重新开启该功能的 Windows 11 版本上操作：

1. 进入 **Windows 安全中心 → 应用和浏览器控制 → 智能应用控制设置**。
2. 选择 **关闭**，再打开安装包。此设置作用于整台电脑。
3. 测试结束后，可在同一页面重新开启。旧版 Windows 关闭后可能需要重置或重装才能恢复，遇到这类提示时请先保留开启状态并更新 Windows。[微软常见问题](https://support.microsoft.com/zh-cn/windows/security/threat-malware-protection/smart-app-control-frequently-asked-questions)

## 反馈与开发

[提交 Issue](https://github.com/INEEDBUG/HKsLGT/issues/new) 时，请附工具箱版本、`winver` 显示的版本和复现步骤。安装问题请说明安装范围和目录类型，截图前请遮盖帐号与个人目录。

构建使用锁定版本的 [LeagueAkari](https://github.com/LeagueAkari/LeagueAkari) 界面源码和 Rust MSVC 工具链。仓库的 Windows 构建工作流会生成安装包，并在可丢弃的 Windows 环境实测从旧版覆盖升级。

## 许可

[MIT](LICENSE) 适用于 HKsLGT 的自有代码。LeagueAkari、安装组件和图标等第三方材料保留各自许可，随安装包附带声明。本项目由社区独立维护，英雄联盟和相关商标归各自权利人所有。
