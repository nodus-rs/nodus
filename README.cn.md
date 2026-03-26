<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus 标识" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>用一条命令把 agent package 加入你的仓库。</strong></p>

<p align="center">
  从 GitHub 或本地路径安装 skills、agents、rules 和 commands，
  锁定精确版本，并只写入你的仓库真正会使用的运行时文件。
</p>

<p align="center">
  <a href="./README.md">English</a> • 简体中文
</p>

<p align="center">
  <a href="#install">安装</a> •
  <a href="#quick-start">快速开始</a> •
  <a href="#common-tasks">常见任务</a> •
  <a href="#advanced">高级用法</a> •
  <a href="#manifest">清单</a> •
  <a href="./CONTRIBUTING.md">参与贡献</a>
</p>

## Nodus 是什么？

Nodus 是一个面向仓库级 AI 工具内容的包管理器。

如果某个包会发布 `skills/`、`agents/`、`rules/` 或 `commands/` 这类目录内容，Nodus 可以：

- 从 GitHub、Git 或本地路径把它加进来
- 把你请求的依赖写入 `nodus.toml`
- 把精确解析结果锁进 `nodus.lock`
- 把受管理文件写入 `.codex/`、`.claude/`、`.cursor/`、`.agents/` 或 `.opencode/`
- 清理过期生成文件，同时不碰未受管理的文件

对大多数用户来说，最重要的命令就是：

```bash
nodus add <package>
```

<a id="install"></a>
## 安装

从 crates.io 安装：

```bash
cargo install nodus
```

在 macOS 或 Linux 上安装最新预构建二进制：

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash
```

通过 Homebrew 安装：

```bash
brew install WendellXY/nodus/nodus
```

<a id="quick-start"></a>
## 快速开始

给 Codex 安装一个包：

```bash
nodus add WendellXY/nodus --adapter codex
```

这一条命令会：

- 如果当前仓库还没有 `nodus.toml`，先自动创建它
- 把依赖记录到 `nodus.toml`
- 默认解析最新 tag
- 把精确解析到的 revision 锁进 `nodus.lock`
- 为你选择的 adapter 写入受管理运行时文件

验证结果：

```bash
nodus doctor
```

典型输出文件大致长这样：

```text
.codex/skills/<skill-id>_<source-id>/
.claude/skills/<skill-id>_<source-id>/
.cursor/rules/<rule-id>_<source-id>.mdc
```

## `nodus add`

从 GitHub 添加：

```bash
nodus add owner/repo --adapter codex
```

从本地路径添加：

```bash
nodus add ./vendor/playbook --adapter codex
```

按 tag、branch、commit 或 semver 范围固定版本：

```bash
nodus add owner/repo --tag v1.2.3
nodus add owner/repo --branch main
nodus add owner/repo --revision 0123456789abcdef
nodus add owner/repo --version '^1.2.0'
```

只安装包的一部分内容：

```bash
nodus add owner/repo --adapter claude --component skills
nodus add owner/repo --adapter claude --component skills --component rules
```

添加只在当前仓库使用的开发依赖：

```bash
nodus add owner/repo --dev --adapter codex
```

让工具启动时自动执行同步：

```bash
nodus add owner/repo --adapter codex --sync-on-launch
```

先预览变更，不实际写入：

```bash
nodus add owner/repo --adapter codex --dry-run
```

<a id="common-tasks"></a>
## 常见任务

在不修改当前仓库的情况下查看包信息：

```bash
nodus info owner/repo
nodus info ./vendor/playbook
nodus info installed_alias
```

查看哪些依赖可以更新：

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

在 CI 里常用这两个：

```bash
nodus sync --locked
nodus sync --frozen
```

移除一个依赖：

```bash
nodus remove nodus
```

检查 manifest、lockfile、共享存储和受管理文件是否一致：

```bash
nodus doctor
```

生成 shell 补全：

```bash
nodus completion bash
nodus completion zsh
nodus completion fish
```

<a id="advanced"></a>
## 高级用法

当前支持的平台：

- macOS（`x86_64`、`arm64`）
- Linux（`x86_64`、`arm64`/`aarch64`）
- Windows（`x86_64`）

默认情况下，Nodus 会把共享镜像、检出和快照存到这里：

```text
macOS:   ~/Library/Application Support/nodus/
Linux:   ~/.local/state/nodus/              (或 $XDG_STATE_HOME/nodus/)
Windows: %LOCALAPPDATA%\nodus\
```

任意命令都可以通过 `--store-path <path>` 覆盖这个位置。

如果你需要安装某个指定版本，可以使用安装脚本：

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --version v0.1.0
```

## 什么时候用 `sync`，什么时候用 `update`

当你想让仓库状态与当前 manifest 和 lockfile 对齐时，用 `nodus sync`。

当你想先查找允许范围内的更新版本，再同步到那些新结果时，用 `nodus update`。

在 CI 中，如果 lockfile 不允许变化，用 `nodus sync --locked`。

如果安装必须严格使用 `nodus.lock` 里已经记录好的精确 revision，用 `nodus sync --frozen`。

## Adapters

Nodus 只会为当前仓库实际使用的 adapters 写入输出。

当前支持：

- `agents`
- `claude`
- `codex`
- `cursor`
- `opencode`

你可以用 `--adapter` 显式指定，也可以把它们持久化到 `nodus.toml`，或者让 Nodus 通过已有目录（例如 `.codex/`、`.claude/`）自动检测。

<a id="manifest"></a>
## 清单

一个最小但实用的使用方清单长这样：

```toml
[adapters]
enabled = ["codex"]

[dependencies]
nodus = { github = "WendellXY/nodus", tag = "v0.3.2" }
```

常见依赖写法：

```toml
[dependencies]
playbook = { path = "vendor/playbook" }
tooling = { github = "owner/tooling", version = "^1.2.0" }
shared = { github = "owner/shared", tag = "v1.4.0", components = ["skills"] }

[dev-dependencies]
internal = { path = "vendor/internal" }
```

直接依赖也可以把文件或目录映射进使用方仓库：

```toml
[dependencies.shared]
path = "vendor/shared"

[[dependencies.shared.managed]]
source = "prompts/review.md"
target = ".github/prompts/review.md"
```

更完整的示例见 [examples/nodus.toml](./examples/nodus.toml)。

## 包布局

Nodus 会从这些约定路径中发现包内容：

- `skills/<id>/SKILL.md`
- `agents/<id>.md`
- `rules/<id>.*`
- `commands/<id>.md`

包也可以声明：

- `content_roots`，用于发布额外目录
- `publish_root = true`，用于把根包自身也一起导出
- `capabilities`，用于声明高权限或高敏感度行为

如果某个包声明了 `high` 敏感度能力，安装或更新时需要：

```bash
nodus sync --allow-high-sensitivity
nodus update --allow-high-sensitivity
```

## Relay

`nodus relay` 主要给包维护者使用：当你在使用方仓库里改了生成出来的运行时文件，想把这些修改回写到源仓库检出目录时，就用它。

```bash
nodus relay nodus --repo-path ../nodus
nodus relay nodus --watch
```

这是一个偏高级的工作流。对大多数用户来说，只需要 `add`、`info`、`outdated`、`update`、`sync`、`remove` 和 `doctor`。

## 团队为什么使用 Nodus

- 用一条命令把仓库级 AI 工具内容加进来
- 精确 revision 会锁进 `nodus.lock`
- 生成文件是受管理、可清理的
- 不会覆盖未受管理文件
- 镜像、检出和快照可以在多个仓库之间复用

## 参与贡献

本地开发和发布检查请见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

本项目基于 [Apache-2.0](LICENSE) 许可证发布。
