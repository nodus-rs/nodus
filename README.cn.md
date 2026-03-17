<p align="center">
  <img src="./assets/nodus-mark.svg" alt="Nodus 标识" width="144">
</p>

<h1 align="center">Nodus</h1>

<p align="center"><strong>用一条命令把 agent package 加入你的仓库。</strong></p>

<p align="center">
  支持从 Git 引用或本地路径解析，锁定精确修订版本，快照包内容，
  并为 Claude、Codex 和 OpenCode 生成受管理的运行时文件。
</p>

<p align="center">
  <a href="./README.md">English</a> • 简体中文
</p>

<p align="center">
  <a href="#install">安装</a> •
  <a href="#quick-start">快速开始</a> •
  <a href="#commands">命令</a> •
  <a href="#manifest">清单</a> •
  <a href="./CONTRIBUTING.md">参与贡献</a>
</p>

## Nodus 是什么？

Nodus 面向这样一类仓库：希望消费 agent package，但不想手动拼装运行时目录。

你只需要把它指向一个 GitHub 仓库或本地路径，Nodus 就会解析包、固定依赖、把精确修订版本锁进 `nodus.lock`、将包内容快照到共享本地存储中，并只写入你所选适配器真正需要的受管理文件。

```bash
nodus add obra/superpowers --adapter codex
nodus add obra/superpowers --adapter codex --component skills
nodus info obra/superpowers
nodus relay superpowers --repo-path ../superpowers
nodus doctor
```

这套安装流程的设计目标是保持可预测：

- `nodus add` 会记录依赖，并立即执行同步
- `nodus info` 会打印依赖别名、本地包路径或 Git 引用解析后的元数据
- `nodus.lock` 会保存精确的 Git 修订版本和受管理输出
- 过期的受管理文件会被清理
- 非受管理文件永远不会被覆盖
- 高敏感度包必须显式选择允许

包作者仍然可以从 `skills/`、`agents/`、`rules/` 和 `commands/` 发布内容，但作为使用方，你主要会和 `nodus add`、`nodus info`、`nodus update`、`nodus relay`、`nodus sync`、`nodus doctor` 打交道。

<a id="install"></a>
## 安装

从 crates.io 安装已发布的 crate：

```bash
cargo install nodus
```

在 macOS 或 Linux 上安装最新的预构建二进制：

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash
```

安装指定版本，或选择自定义安装目录：

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --version v0.1.0
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --install-dir /usr/local/bin
```

当发布版本包含校验和资源时，可验证下载的归档文件：

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --verify
```

从默认目录或自定义目录卸载：

```bash
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --uninstall
curl -fsSL https://raw.githubusercontent.com/WendellXY/nodus/main/install.sh | bash -s -- --uninstall --install-dir /usr/local/bin
```

你也可以从 GitHub release 的资源中下载适用于当前平台的预构建二进制归档，然后在本地运行仓库根目录下的 `install.sh`。

从当前检出目录构建或安装：

```bash
cargo install --path .
```

安装完成后，运行：

```bash
nodus <command>
```

默认情况下，Nodus 会将共享镜像、检出和快照存储在平台本地应用数据目录中：

```text
macOS:   ~/Library/Application Support/nodus/
Linux:   ~/.local/state/nodus/              (或 $XDG_STATE_HOME/nodus/)
Windows: %LOCALAPPDATA%\nodus\
```

你可以对任意命令通过 `--store-path <path>` 覆盖该位置。

<a id="quick-start"></a>
## 快速开始

如果仓库还没有清单文件：

```bash
nodus init
```

然后添加一个包：

```bash
nodus add obra/superpowers --adapter codex
```

如果只想安装该包中的部分制品类型：

```bash
nodus add obra/superpowers --adapter codex --component skills --component rules
```

这一条命令会：

- 在未传入 `--tag`、`--branch` 或 `--revision` 时解析最新 tag
- 将依赖写入 `nodus.toml`
- 在需要时持久化适配器选择
- 将精确状态锁定到 `nodus.lock`
- 在所选运行时根目录下生成受管理文件

验证结果：

```bash
nodus doctor
```

用于可重复的 CI：

```bash
nodus sync --locked
```

如果你想严格安装 `nodus.lock` 中已经记录好的 Git 修订版本，而不是继续跟随 branch 的最新提交：

```bash
nodus sync --frozen
```

当某个包声明了 `high` 敏感度能力时：

```bash
nodus sync --allow-high-sensitivity
```

需要时可使用自定义共享存储根目录：

```bash
nodus --store-path /tmp/nodus-store sync
```

移除已配置的依赖，并清理它对应的受管理输出：

```bash
nodus remove superpowers
```

如果你在本地维护某个依赖仓库，并希望把受管理运行时目录中的修改回写到该仓库：

```bash
nodus relay superpowers --repo-path ../superpowers
```

完成设置后，你的仓库会在 `nodus.toml` 中拥有固定依赖，在 `nodus.lock` 中拥有精确解析状态，并在你选择的适配器根目录下拥有受管理的运行时文件。

## 团队为什么使用 Nodus

- 从 Git 或本地路径添加包，而不用手动把文件复制进 `.agents/`、`.codex/`、`.claude/`、`.cursor/` 或 `.opencode/`
- 一次安装，只为仓库实际使用的运行时生成输出
- 在多个项目之间复用共享镜像、检出和按内容寻址的快照
- 通过明确的所有权管理生成文件，从而安全清理过期输出
- 使用 `nodus doctor` 校验安装状态，并在 CI 中用 `nodus sync --locked` 强制执行

## 当前可用能力

Nodus 当前支持：

- 本地路径依赖
- 从 tag 或 branch 解析的 Git 依赖
- 通过 `nodus.lock` 存储锁定状态的确定性同步
- 为 Agents、Claude、Codex、Cursor 和 OpenCode 生成受管理输出
- 仓库级适配器选择，可自动检测、显式指定或持久化保存
- 使用 `nodus doctor` 校验共享存储状态、锁文件状态和受管理文件

后续计划：

- 远程注册表
- 包发布工作流
- 签名或来源验证
- 全局安装范围
- Claude 插件模式

## 参与贡献

本地开发流程和发布检查请见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

本项目基于 [Apache-2.0](LICENSE) 许可证发布。

<a id="manifest"></a>
## 清单

根项目如果只是消费依赖，并不要求必须填写 `api_version`、`name` 或 `version`。

一个最小化的使用方清单如下：

```toml
[adapters]
enabled = ["codex"]

[dependencies]
superpowers = { github = "obra/superpowers", tag = "v0.1.0" }
```

你也可以选择性过滤某个依赖贡献的制品类型：

```toml
[dependencies]
superpowers = { github = "obra/superpowers", tag = "v5.0.2", components = ["skills"] }
```

你也可以使用本地路径依赖：

```toml
[dependencies]
local_playbook = { path = "vendor/playbook" }
```

当依赖是从 branch 而不是 Git tag 同步时，你还可以选择单独保留该包自己的语义化版本，而不是仅依赖传输引用：

```toml
[dependencies]
axiom = { github = "CharlesWiltgen/Axiom", branch = "main", version = "2.34.0" }
```

你也可以把依赖固定到某个精确的 Git commit：

```toml
[dependencies]
superpowers = { github = "obra/superpowers", revision = "0123456789abcdef0123456789abcdef01234567" }
```

可选能力仍然受支持：

```toml
[[capabilities]]
id = "shell.exec"
sensitivity = "high"
justification = "Run repository checks."
```

### 支持的字段

- `api_version`（可选）
- `name`（可选）
- `version`（可选）
- `capabilities`
- `[adapters]`
- `adapters.enabled`
- `[dependencies]`
- `dependencies.<alias>.github`
- `dependencies.<alias>.url`
- `dependencies.<alias>.path`
- `dependencies.<alias>.tag`
- `dependencies.<alias>.branch`
- `dependencies.<alias>.revision`
- `dependencies.<alias>.version`
- `dependencies.<alias>.components`

未知的清单字段会被忽略，并给出警告。

### 适配器选择

Nodus 只会为选中的适配器生成输出。解析顺序如下：

1. `nodus add` 或 `nodus sync` 上显式传入的 `--adapter <agents|claude|codex|cursor|opencode>` 参数
2. `nodus.toml` 中持久化保存的 `[adapters] enabled = [...]`
3. 检测到的仓库根目录标志：
   - `.agents/` => Agents
   - `.claude/` => Claude
   - `.codex/` => Codex
   - `.cursor/` => Cursor
   - `.opencode/` 或 `AGENTS.md` => OpenCode
4. 在 TTY 中进行交互式提示
5. 在非交互环境中报错，并给出指导信息

当 Nodus 通过标志、自动检测或交互提示解析出适配器时，它会把 `[adapters] enabled = [...]` 写入 `nodus.toml`，从而让后续的 `sync`、`doctor` 和 CI 运行保持确定性。

## 包发现

Nodus 通过顶层目录校验并发现包内容：

- `skills/<id>/SKILL.md` => skill
- `agents/<id>.md` => agent
- `rules/<id>.*` => rule
- `commands/<id>.*` => command

当你在仓库根目录运行 Nodus 时，这些目录会被视为该仓库提供给消费者的包源码。Nodus 不会把根项目自身的 `skills/`、`agents/`、`rules/` 或 `commands/` 镜像到 `.codex/` 或 `.claude/` 这样的受管理运行时目录中；受管理输出只会针对已解析的依赖生成。

包有效性规则：

- 依赖仓库必须至少包含 `skills/`、`agents/`、`rules/`、`commands/` 之一，或者在 `nodus.toml` 中声明至少一个依赖
- 其他文件和目录允许存在，并会被忽略
- `skills/` 下的条目必须是目录
- 每个 skill 都必须包含带有 YAML frontmatter 的 `SKILL.md`，其中包含：
  - `name`
  - `description`
- `agents/` 下的条目必须是 `.md` 文件
- `rules/` 和 `commands/` 下的条目必须是文件

<a id="commands"></a>
## 命令

### `nodus add`

```bash
nodus add <url>
```

默认情况下，Nodus 会解析最新 Git tag，将该 tag 写入 `nodus.toml`，然后立刻执行一次普通的 `nodus sync`。

你仍然可以显式固定某个特定 tag：

```bash
nodus add <url> --tag <tag>
```

也可以显式跟踪某个 branch 或精确 commit：

```bash
nodus add <url> --branch <branch>
nodus add <url> --revision <commit>
```

你也可以显式选择一个或多个适配器：

```bash
nodus add <url> --adapter codex
nodus add <url> --adapter claude --adapter opencode
```

你还可以把依赖限制为仅包含特定组件类型：

```bash
nodus add <url> --component skills
nodus add <url> --component skills --component agents
```

你也可以为此命令覆盖共享仓库存储根目录：

```bash
nodus --store-path /tmp/nodus-store add <url>
```

行为：

- 接受完整 Git URL，或 `obra/superpowers` 这样的 GitHub 简写
- 从仓库名推导依赖别名
- 将共享 bare mirror 拉取到共享存储根目录中
- 在共享存储根目录下为已解析修订版本物化共享检出
- 当未提供 Git 选择器时解析最新 tag
- 将 `tag`、`branch` 或 `revision` 写入 `nodus.toml`
- 校验发现到的包布局或依赖包装清单
- 创建或更新 `nodus.toml`
- 只在调用方清单中记录你直接添加的依赖
- 让常规同步流程递归解析远程仓库 `nodus.toml` 中声明的依赖
- 当适配器是自动推断或显式提供时，持久化保存该选择
- 当提供 `--component` 时，持久化保存依赖组件选择

示例：

```bash
nodus add obra/superpowers
```

### `nodus init`

创建最小化的 `nodus.toml`，并生成 `skills/example/SKILL.md`。

### `nodus info`

```bash
nodus info <package>
```

显示解析后的包元数据，不会修改当前项目。

示例：

```bash
nodus info obra/superpowers
nodus info ./vendor/superpowers
nodus info superpowers
nodus info obra/superpowers --tag v0.4.0
nodus info obra/superpowers --branch main
```

行为：

- 接受当前仓库中的依赖别名、本地包目录、完整 Git URL，或 `owner/repo` 这样的 GitHub 简写
- 如果参数匹配当前仓库中的直接依赖别名，则使用 `nodus.toml` 里固定的来源进行解析
- 当没有提供 Git ref 覆盖时，会直接检查本地包目录
- 当检查 Git 引用且未提供 `--tag` 或 `--branch` 时，会解析最新 Git tag
- 如果 Git 仓库没有 tag，则回退到默认分支
- 输出解析后的来源、包根目录、选中的组件、发现到的 artifact id、依赖、适配器以及声明的 capability

### `nodus remove`

从 `nodus.toml` 中移除一个依赖，然后运行常规同步流程来更新 `nodus.lock` 并清理受管理运行时文件。包参数既可以是依赖别名，也可以是 `owner/repo` 这样的仓库引用。

### `nodus relay`

```bash
nodus relay <dependency> [--repo-path <path>] [--via <adapter>] [--watch]
```

把 `.codex/`、`.claude/`、`.cursor/`、`.agents/`、`.opencode/` 等受管理运行时目录中的修改，回写到你本地维护的直接 Git 依赖检出。

行为：

- 只支持 `nodus.toml` 中的直接 Git 依赖
- 需要当前有效的 `nodus.lock`，并以锁定快照作为回写基线
- 将维护者本地关联信息持久化到 `.nodus/local.toml`
- `--via <adapter>` 会在 `.nodus/local.toml` 中持久化一个首选适配器提示，用于在 relay 元数据需要记录哪个适配器应被视为规范来源时使用；别名：`--relay-via`、`--prefer`
- 自动写入 `.nodus/.gitignore`，保证该本地关联配置默认不纳入版本控制
- 会校验关联检出是 Git 仓库，且其 `origin` 与依赖 URL 一致
- 只把变更过的源文件写回本地检出，不会自动 commit 或 push
- 配合 `--watch` 使用时，会持续轮询受管理输出，并在检测到新修改后自动回写，直到你主动停止命令
- 如果多个受管理变体的内容不一致，或关联源码与受管理输出都偏离了锁定基线，则会失败

示例：

```bash
nodus relay superpowers --repo-path ../superpowers
nodus relay superpowers --via claude
nodus relay superpowers --watch
```

### `nodus sync`

解析根项目及其已配置依赖，递归跟随依赖清单中声明的嵌套依赖，对发现的内容生成快照，写入 `nodus.lock`，并为已解析依赖生成受管理的运行时输出。

选项：

- `--store-path <path>`：覆盖共享仓库存储根目录
- `--locked`：如果 `nodus.lock` 会发生变化则失败
- `--frozen`：严格按 `nodus.lock` 安装 Git 修订版本；如果锁文件缺失或已过期则失败
- `--allow-high-sensitivity`：允许声明了 `high` 敏感度能力的包
- `--adapter <agents|claude|codex|cursor|opencode>`：覆盖并持久化该仓库的适配器选择

如果某个依赖已经在 `.nodus/local.toml` 中配置了 relay 链接，而对应的受管理输出仍有尚未回写的修改，`sync` 会直接失败，而不是覆盖这些修改。

### `nodus doctor`

检查以下内容：

- 根清单可以被成功解析
- 共享依赖检出在共享存储根目录下存在
- 共享仓库镜像在共享存储根目录下存在，且 origin URL 符合预期
- 发现到的布局是有效的
- Git 依赖位于预期的锁定修订版本
- `nodus.lock` 是最新的
- 受管理文件所有权条目在内部保持一致
- 不会出现阻止同步的非受管理文件冲突

## 受管理文件

Nodus 只管理它自己写入的文件。

受管理文件会记录在 `nodus.lock` 中。同步时，Nodus 会：

- 写入或更新受管理文件
- 删除那些已经不再需要的过期受管理文件
- 拒绝覆盖已有的非受管理文件
- 拒绝覆盖已经通过 `.nodus/local.toml` 关联、但尚未 relay 的受管理修改

## 锁文件与存储

`nodus.lock` 记录：

- 依赖别名
- 源类型（`path` 或 `git`）
- 源 URL 或路径
- 请求的 tag
- 精确 Git 修订版本
- 内容摘要
- 依赖组件选择（如果相对于包默认值做了收窄）
- 发现到的 skills / agents / rules / commands
- 声明的 capabilities
- 受管理运行时所有权条目

解析后的包会被快照到：

```text
<store-root>/store/sha256/<digest>/
```

同步是从这些快照中生成输出，而不是直接从可变的工作树中生成。

## 共享存储

共享依赖状态使用三个磁盘位置：

- 共享远程镜像位于 `<store-root>/repositories/<repo-name>-<url-hash>.git`
- 共享检出位于 `<store-root>/checkouts/<repo-name>-<url-hash>/<rev>/`
- 共享按内容寻址的快照位于 `<store-root>/store/sha256/<digest>/`

这样可以让已拉取的仓库、物化检出和包快照在项目之间共享。项目特定状态则只保留在各自仓库的锁文件和已生成运行时输出中。

## 运行时输出映射

当前适配器行为如下：

- Nodus 只会为该仓库选中的适配器生成输出
- Nodus 会先根据每个依赖自己导出的组件进行过滤，再执行适配器特定的生成逻辑
- 如果已经存在多个适配器根目录，Nodus 会安装所有检测到的适配器
- Agents：发现到的 skills 会复制到 `.agents/skills/<skill-id>_<source-id>/`
- Agents：发现到的 commands 会复制到 `.agents/commands/<command-id>_<source-id>.md`
- Claude：发现到的 skills 会复制到 `.claude/skills/<skill-id>_<source-id>/`
- Claude：发现到的 agents 会复制到 `.claude/agents/<agent-id>_<source-id>.md`
- Claude：发现到的 commands 会复制到 `.claude/commands/<command-id>_<source-id>.md`
- Claude：发现到的 rules 会复制到 `.claude/rules/<rule-id>_<source-id>.md`
- Codex：发现到的 skills 会复制到 `.codex/skills/<skill-id>_<source-id>/`
- Codex：发现到的 rules 会复制到 `.codex/rules/<rule-id>_<source-id>.rules`
- Cursor：发现到的 skills 会复制到 `.cursor/skills/<skill-id>_<source-id>/`
- Cursor：发现到的 commands 会复制到 `.cursor/commands/<command-id>_<source-id>.md`
- Cursor：发现到的 rules 会复制到 `.cursor/rules/<rule-id>_<source-id>.mdc`
- OpenCode：发现到的 skills 会复制到 `.opencode/skills/<skill-id>_<source-id>/`
- OpenCode：发现到的 agents 会复制到 `.opencode/agents/<agent-id>_<source-id>.md`
- OpenCode：发现到的 commands 会复制到 `.opencode/commands/<command-id>_<source-id>.md`
- OpenCode：发现到的 rules 会复制到 `.opencode/rules/<rule-id>_<source-id>.md`

对于受管理目录和文件，`<source-id>` 是一个简短且确定性的后缀：

- Git 依赖使用锁定提交 SHA 的前 6 个字符
- 根包和本地路径包使用包内容摘要的前 6 个字符

在 `nodus.lock` 中，受管理运行时输出会用稳定的逻辑根路径来跟踪，例如 `.agents/skills/<skill-id>`、`.agents/commands/<command-id>.md`、`.claude/skills/<skill-id>`、`.codex/rules/<rule-id>.rules`、`.cursor/rules/<rule-id>.mdc` 以及 `.opencode/commands/<command-id>.md`。在同步和 doctor 期间，Nodus 会根据锁定的包来源，把这些逻辑路径重新展开为带后缀的具体目录或文件。

对于每个选中的运行时根目录，Nodus 还会写入一个受管理的 `.gitignore` 文件，用来忽略它自己以及该根目录下生成的运行时输出。

## 开发

运行验证套件：

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
