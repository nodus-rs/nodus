<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus 标识" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>用一条命令，把 AI 助手需要的能力包接入你的项目。</strong></p>

<p align="center">
  你可以从 GitHub、Git 或本地路径安装 skills、agents、rules 和 commands，
  Nodus 会帮你记录版本、锁定结果，并把真正需要的运行时文件写进仓库。
</p>

<p align="center">
  <a href="./README.md">English</a> • 简体中文
</p>

<p align="center">
  <a href="#install">安装</a> •
  <a href="#for-ai">给 AI 助手</a> •
  <a href="#quick-start">快速开始</a> •
  <a href="#common-tasks">常见任务</a> •
  <a href="#advanced">进阶说明</a> •
  <a href="#manifest">清单</a> •
  <a href="./CONTRIBUTING.md">参与贡献</a>
</p>

## Nodus 是什么？

Nodus 可以理解成一个“给项目里的 AI 助手装能力包”的工具。

如果某个包发布了 `skills/`、`agents/`、`rules/` 或 `commands/` 这类内容，Nodus 可以帮你：

- 从 GitHub、Git 或本地路径把它接入仓库
- 把你选择的依赖记录到 `nodus.toml`
- 把精确解析到的版本锁进 `nodus.lock`
- 把受管理文件写入 `.codex/`、`.claude/`、`.cursor/`、`.agents/`、`.github/` 或 `.opencode/`
- 清理旧的生成文件，同时不碰你自己手写的未受管理文件

对大多数人来说，最重要的命令只有一个：

```bash
nodus add <package>
```

如果你想让项目里的 agent 自动学会怎么使用 Nodus，最直接的起点通常是：

```bash
nodus add nodus-rs/nodus
```

这会把 Nodus 自己发布的能力包接进当前仓库，让 agent 可以直接读取它生成的 skills 和说明。如果这是一个全新的仓库，Nodus 还没法判断你在用哪个工具，那么第一次执行时补上 adapter 即可，比如 `--adapter codex`。

<a id="install"></a>
## 安装

你可以选择下面任意一种方式安装。

从 crates.io 安装：

```bash
cargo install nodus
```

在 macOS 或 Linux 上安装最新预构建版本：

```bash
curl -fsSL https://raw.githubusercontent.com/nodus-rs/nodus/main/install.sh | bash
```

通过 Homebrew 安装：

```bash
brew install nodus-rs/nodus/nodus
```

在 Windows 上通过 PowerShell 安装最新预构建版本：

```powershell
irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex
```

<details>
<summary>Windows 安装命令失败？</summary>

如果执行时报错，比如提示 `pwsh` 不存在，先安装 PowerShell 7，重启终端，再执行：

```powershell
winget install --id Microsoft.PowerShell --source winget
# 先重启终端，让 `pwsh` 生效。
pwsh -NoProfile -Command "irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex"
```

</details>

<a id="for-ai"></a>
## 给 AI 助手

如果你希望直接把需求交给 AI 助手处理，而不是自己研究 Nodus 命令，可以这样做：

1. 打开 [`PROMPT.cn.md`](./PROMPT.cn.md)
2. 将全文复制给任意 AI 助手，例如 OpenCode、Cursor、Claude、Codex
3. 再补一句你的目标，例如：
   - “帮我把 Nodus 接入当前项目，给 Codex 用”
   - “帮我安装 `https://github.com/wenext-limited/playbook-ios`，给 Claude 用”
   - “帮我把这个仓库现有的 Nodus 配置同步一下”
   - “帮我检查为什么 `nodus doctor` 失败了”

这份文档是专门写给 AI 助手看的，目标是让不熟悉命令行和 Nodus 的用户，也能直接通过 AI 完成安装、同步、更新和排障。

<a id="quick-start"></a>
## 快速开始

如果你想先让当前仓库里的 agent 学会 Nodus，不必先把所有概念都弄明白。可以先执行：

```bash
nodus add nodus-rs/nodus
```

这条命令会自动帮你做几件事：

- 如果仓库里还没有 `nodus.toml`，就先创建一个
- 把这个依赖写进 `nodus.toml`
- 默认解析最新 tag
- 把精确结果锁进 `nodus.lock`
- 为自动检测到或已经配置好的 adapter 写入需要的受管理文件

如果仓库里还没有 `.codex/`、`.claude/`、`.github/skills` 这类 adapter 线索，第一次可以显式指定：

```bash
nodus add nodus-rs/nodus --adapter <adapter>
```

执行完后，可以再跑一次：

```bash
nodus doctor
```

它会帮你检查当前仓库里的 manifest、lockfile、共享存储和受管理文件是否一致。

常见生成结果大概会出现在这些位置：

```text
.codex/skills/<skill-id>_<source-id>/
.claude/skills/<skill-id>_<source-id>/
.github/skills/<skill-id>_<source-id>/
.github/agents/<agent-id>_<source-id>.agent.md
.cursor/rules/<rule-id>_<source-id>.mdc
```

如果你只是普通用户，可以把流程理解成：

1. 安装 Nodus
2. `nodus add ...`
3. `nodus doctor`
4. 打开你的工具，让 agent 开始工作

## `nodus add`

这是最常用的命令。

从 GitHub 添加：

```bash
nodus add owner/repo --adapter <adapter>
```

从本地路径添加：

```bash
nodus add ./vendor/playbook --adapter <adapter>
```

固定到某个版本、分支、提交或版本范围：

```bash
nodus add owner/repo --tag v1.2.3
nodus add owner/repo --branch main
nodus add owner/repo --revision 0123456789abcdef
nodus add owner/repo --version '^1.2.0'
```

只安装包的一部分内容：

```bash
nodus add owner/repo --adapter <adapter> --component skills
nodus add owner/repo --adapter <adapter> --component skills --component rules
```

把它记为开发依赖：

```bash
nodus add owner/repo --dev --adapter <adapter>
```

让支持的工具在打开仓库时自动执行 `nodus sync`：

```bash
nodus add owner/repo --adapter <adapter> --sync-on-launch
```

先预览变更，不真正写入：

```bash
nodus add owner/repo --adapter <adapter> --dry-run
```

如果你不确定该选什么，通常先从下面这条开始就够了：

```bash
nodus add owner/repo --adapter <adapter>
```

<a id="common-tasks"></a>
## 常见任务

下面这些命令覆盖了大多数日常使用场景。

先初始化一个最小配置：

```bash
nodus init
```

查看当前已经配置了哪些依赖：

```bash
nodus list
```

在不改仓库的前提下查看某个包的信息：

```bash
nodus info owner/repo
nodus info ./vendor/playbook
nodus info installed_alias
```

用 AI 帮你看一下某个包图是否大致安全、适合接入：

```bash
nodus review
nodus review owner/repo
```

查看哪些依赖有更新：

```bash
nodus outdated
```

更新依赖并重写受管理文件：

```bash
nodus update
```

按照当前 manifest 和 lockfile 重新生成受管理文件：

```bash
nodus sync
```

如果你的流程要求 lockfile 不允许变化：

```bash
nodus sync --locked
```

如果必须严格使用 `nodus.lock` 里已经记录好的精确版本：

```bash
nodus sync --frozen
```

移除一个依赖：

```bash
nodus remove nodus
```

检查当前仓库状态是否健康：

```bash
nodus doctor
```

检查或安装更新版的 Nodus CLI：

```bash
nodus upgrade
```

生成 shell 补全：

```bash
nodus completion bash
nodus completion zsh
nodus completion fish
```

如果你只想记住最重要的几条，通常是这几条：

- `nodus add`
- `nodus sync`
- `nodus update`
- `nodus remove`
- `nodus doctor`

<a id="advanced"></a>
## 进阶说明

当前支持的平台：

- macOS（`x86_64`、`arm64`）
- Linux（`x86_64`、`arm64`/`aarch64`）
- Windows（`x86_64`、`arm64`）

默认情况下，Nodus 会把共享镜像、检出和快照存到这里：

```text
macOS:   ~/Library/Application Support/nodus/
Linux:   ~/.local/state/nodus/              (或 $XDG_STATE_HOME/nodus/)
Windows: %LOCALAPPDATA%\nodus\
```

如果你想换位置，可以在任意命令后加：

```bash
--store-path <path>
```

如果你要安装指定版本，可以这样做：

在 macOS 或 Linux 上：

```bash
curl -fsSL https://raw.githubusercontent.com/nodus-rs/nodus/main/install.sh | bash -s -- --version v0.1.0
```

在 Windows 上：

```powershell
$env:NODUS_VERSION='v0.1.0'; irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex
```

如果 Windows 上失败，可以先设置版本号，再通过 `pwsh` 执行：

```powershell
$env:NODUS_VERSION='v0.1.0'
pwsh -NoProfile -Command "irm https://raw.githubusercontent.com/nodus-rs/nodus/main/install.ps1 | iex"
```

## 什么时候用 `sync`，什么时候用 `update`

可以把它们简单理解成：

- `nodus sync`：按你现在已经记录好的内容，重新同步一遍
- `nodus update`：先去找允许范围内的新版本，再同步到新结果

如果你只是改了仓库内容、想重新生成受管理文件，通常用 `sync`。

如果你是想把依赖升级到更新的可用版本，通常用 `update`。

## Adapters

Nodus 只会为你的仓库真正使用的 adapter 写输出。

当前支持：

- `agents`
- `claude`
- `codex`
- `copilot`
- `cursor`
- `opencode`

你可以：

- 用 `--adapter` 显式指定
- 把 adapter 配到 `nodus.toml`
- 或让 Nodus 根据已有目录自动检测，比如 `.codex/`、`.claude/`、`.github/skills`

关于 `copilot`，需要知道的一点是：

- 它会把 GitHub Copilot 相关内容写到 `.github/skills/` 和 `.github/agents/`
- 当前版本只支持 skills 和 custom agents
- rules 和 commands 不会为 `copilot` 生成

<a id="manifest"></a>
## 清单

`nodus.toml` 是 Nodus 的主配置文件。你可以把它理解成“这个仓库想装哪些能力包”的清单。

一个最小但实用的例子：

```toml
[adapters]
enabled = ["codex"]

[dependencies]
nodus = { github = "nodus-rs/nodus", tag = "v0.3.2" }
```

更常见的几种写法：

```toml
[dependencies]
playbook = { path = "vendor/playbook" }
tooling = { github = "owner/tooling", version = "^1.2.0" }
shared = { github = "owner/shared", tag = "v1.4.0", components = ["skills"] }
paused = { github = "owner/paused", tag = "v1.0.0", enabled = false }

[dev-dependencies]
internal = { path = "vendor/internal" }
```

如果你写了 `enabled = false`，它的意思是：

- 这个依赖仍然保留在 `nodus.toml` 里
- 但暂时不会参与解析
- 也不会同步受管理文件
- 也不会写入 `nodus.lock`

你还可以把文件或目录直接映射进使用方仓库：

```toml
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
```

更完整的示例见 [examples/nodus.toml](./examples/nodus.toml)。

## 包布局

Nodus 会从这些约定路径里发现一个包的内容：

- `skills/<id>/SKILL.md`
- `agents/<id>.md`
- `rules/<id>.*`
- `commands/<id>.md`

包还可以额外声明：

- `content_roots`：发布额外目录
- `publish_root = true`：把根包自身一起导出
- `capabilities`：声明高权限或高敏感度行为

如果某个包声明了 `high` 敏感度能力，安装或更新时需要显式允许：

```bash
nodus sync --allow-high-sensitivity
nodus update --allow-high-sensitivity
```

## Relay

`nodus relay` 主要是给包维护者准备的。

如果你在“使用方仓库”里改了已经生成出来的运行时文件，想把这些修改回写到源包检出目录，就可以用：

```bash
nodus relay nodus --repo-path ../nodus
nodus relay nodus --watch
```

这属于比较进阶的工作流。对大多数普通用户来说，通常只需要：

- `add`
- `list`
- `info`
- `review`
- `outdated`
- `update`
- `sync`
- `remove`
- `doctor`

## 团队为什么使用 Nodus

- 用一条命令把仓库级 AI 工具内容接进来
- 精确版本会锁进 `nodus.lock`
- 生成文件是受管理、可清理的
- 不会覆盖未受管理文件
- 镜像、检出和快照可以在多个仓库之间复用

## 参与贡献

本地开发和发布检查请见 [CONTRIBUTING.md](./CONTRIBUTING.md)。

## 许可证

本项目基于 [Apache-2.0](./LICENSE) 许可证发布。
