<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus 标识" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>用一条命令，把 agent 包接入你的仓库。</strong></p>

<p align="center">
  Nodus 可以从 GitHub、Git URL 或本地路径安装 agent 包，锁定精确版本，
  并且只写入当前仓库实际会被 adapter 读取的运行时文件。
</p>

<p align="center">
  <a href="./README.md">English</a> • 简体中文
</p>

<p align="center">
  <a href="#install">安装</a> •
  <a href="#for-ai-assistants">给 AI 助手</a> •
  <a href="#quick-start">快速开始</a> •
  <a href="#cli-help">CLI 帮助</a> •
  <a href="#learn-more">继续了解</a> •
  <a href="./CONTRIBUTING.md">参与贡献</a>
</p>

## Nodus 是什么？

Nodus 是一个面向仓库级 agent tooling 的包管理器。

如果某个包发布了 `skills/`、`agents/`、`rules/` 或 `commands/` 之类的内容，Nodus 可以帮你：

- 从 GitHub、Git 或本地路径把它接入仓库
- 把你选择的依赖记录到 `nodus.toml`
- 把精确解析到的版本锁进 `nodus.lock`
- 把受管理文件写入 `.codex/`、`.claude/`、`.cursor/`、`.github/`、`.agents/`、`.opencode/` 等 adapter 目录
- 为已支持的 runtime 组合并写出受管理的 MCP server 配置，包括 `.mcp.json`、`.codex/config.toml` 和 `opencode.json`
- 清理旧的生成文件，同时不碰你自己维护的未受管理文件

对大多数团队来说，最常见的流程是：

```bash
nodus add <package> --adapter <adapter>
nodus doctor
```

## 安装

从 crates.io 安装：

```bash
cargo install nodus
```

在 macOS 或 Linux 上安装最新预构建版本：

```bash
curl -fsSL https://nodus.elata.ai/install.sh | bash
```

通过 Homebrew 安装：

```bash
brew install nodus-rs/nodus/nodus
```

在 Windows 上通过 PowerShell 安装最新预构建版本：

```powershell
irm https://nodus.elata.ai/install.ps1 | iex
```

<details>
<summary>Windows 安装命令失败？</summary>

如果命令失败，先安装 PowerShell 7，重启终端，再执行：

```powershell
winget install --id Microsoft.PowerShell --source winget
pwsh -NoProfile -Command "irm https://nodus.elata.ai/install.ps1 | iex"
```

</details>

## 给 AI 助手

如果你希望把 Nodus 的使用流程直接交给 AI 助手，可以把可直接抓取的提示词链接发给它：

- <https://nodus.elata.ai/zh-cn/prompts/README.md>

这份提示词会给助手更具体的 Nodus 操作说明，帮助它生成合适的 `nodus add` 命令，并最终用 `nodus doctor` 做验证。

## 快速开始

把一个包安装到当前仓库，并检查结果：

```bash
nodus add nodus-rs/nodus --adapter codex
nodus doctor
```

这套流程会：

- 如果仓库里还没有 `nodus.toml`，就先创建它
- 把依赖写进 `nodus.toml`
- 解析并锁定精确版本到 `nodus.lock`
- 为选定或检测到的 adapter 写入受管理运行时文件

如果包里发布了 `mcp_servers`，Nodus 现在也会把对应的 MCP 配置一起写入仓库里的受管理
runtime 输出。目前包括传统项目级 `.mcp.json`、Codex 的 `.codex/config.toml`，以及
OpenCode 的 `opencode.json`。

如果这个包本身是一个会暴露多个子包的 wrapper，`nodus add` 现在默认只记录 wrapper
本身，不会自动启用全部子包。你可以后续手动编辑 `nodus.toml` 里的 `members`，或者在
安装时显式传 `--accept-all-dependencies` 一次性启用全部子包。

如果你想装到用户级环境，而不是当前仓库，也可以显式使用 `--global`：

```bash
nodus add nodus-rs/nodus --global --adapter codex
```

## CLI 帮助

`nodus --help` 现在就是主要命令指南。

想了解整体流程时先看它，再按需打开子命令帮助：

```bash
nodus --help
nodus add --help
nodus sync --help
nodus doctor --help
```

大多数用户最常用的是这些命令：

- `nodus add <package> --adapter <adapter>`：把包装进当前仓库
- `nodus info <package-or-alias>`：安装前后查看包信息
- `nodus sync`：按当前已记录版本重建受管理输出
- `nodus update`：把依赖升级到更新但仍允许的版本
- `nodus remove <alias>`：移除依赖并清理它拥有的输出
- `nodus clean`：清理共享的 repository、checkout 和 snapshot 缓存，但不修改项目 manifest 或受管理输出
- `nodus doctor`：检查仓库、lockfile、共享存储和受管理输出是否一致

## 继续了解

- 文档：<https://nodus.elata.ai/docs/>
- 安装说明：<https://nodus.elata.ai/install/>
- 包命令生成器：<https://nodus.elata.ai/packages/>
- 使用者 manifest 示例：[examples/nodus.toml](./examples/nodus.toml)
- 包作者 manifest 示例：[examples/package-author.nodus.toml](./examples/package-author.nodus.toml)

如果你想了解包作者工作流、workspace 包装、managed exports 或 relay 这样的进阶主题，优先看网站文档和 `nodus --help`，而不是把这个 README 当成完整命令手册。
MCP 包也一样：包作者可以在 `nodus.toml` 里发布 `mcp_servers`，使用方则会按所选 adapter
拿到对应的受管理项目配置。

## 参与贡献

见 [CONTRIBUTING.md](./CONTRIBUTING.md)。

## License

Licensed under [Apache-2.0](./LICENSE).
