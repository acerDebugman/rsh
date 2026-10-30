# rsh

一个用 Rust 写的终端 coding agent：自研 tool loop、Codex 式 inline TUI、通用 skill 加载，以及 native / dsh 两种子代理后端。

- 主 agent 只与用户对话；子代理只把结果交回主 agent。
- 不启动子代理时，主 agent 也能直接加载并执行 `~/.agents/skills/*`（例如 `grill-me`）。
- 模型默认 `deepseek-flash`，key 读 `~/.config/ds.key`。

## 构建

本机 cargo 的 crates.io 被替换为内网镜像，且 IPv6 不通；先加载开发环境脚本（仅开发用）：

```bash
source scripts/dev-env.sh   # 起本地 IPv4 CONNECT 代理 + 工作区内 CARGO_HOME/TMPDIR
cargo build --release
```

普通网络环境直接 `cargo build --release` 即可。

## 使用

```bash
rsh                          # 交互式 TUI
rsh exec --yolo "用 Python 写一个 todo CLI 并跑测试"
rsh --resume <session-id>    # 恢复会话（也可 --continue 恢复当前目录最新会话）
```

TUI 按键：

| 键 | 行为 |
|---|---|
| `Enter` | 发送；忙时忽略，问题待答时提交答案 |
| `Ctrl+J` | 换行 |
| `Esc` | 取消当前 turn / 清空输入 / 取消提问 |
| `Ctrl+C` | 取消；空输入再按退出 |
| `!cmd` | 直接执行 shell，不经过模型 |
| `/help`、`/model`、`/clear`、`/compact [指令]`、`/skill <name> [task]`、`/quit` | 斜杠命令 |

会话保存在 `~/.rsh/sessions/<cwd-slug>/<session-id>/events.jsonl`（`RSH_HOME` 可覆盖）。

## Skills

扫描 `~/.agents/skills/*/SKILL.md` 与 `<workspace>/.agents/skills/*/SKILL.md`，项目级覆盖全局同名。

```bash
/skill grill-me 我要用 Rust 写一个终端 coding agent，请就范围、权限、测试追问我
```

模型也可以调用 `skill` 工具；`disable-model-invocation: true` 的技能只能由用户 `/skill` 显式加载。

## 子代理

`subagent` 工具参数 `backend`：

- `native`（默认）：进程内独立 context，除 `subagent`/`ask_user` 外全部工具，深度 1，超时 600s。
- `dsh`：优先 `dsh --profile acp`（ACP v1 over stdio，模型经 `set_config_option` 设为 `deepseek-flash`）；握手/传输失败自动降级 `dst run --output-format stream-json`，再降级 `dsh --profile headless`。

## 配置

`~/.config/rsh/config.toml`（`RSH_CONFIG` 可覆盖）：

```toml
model = "deepseek-flash"
base_url = "https://api.deepseek.com"
api_key_file = "~/.config/ds.key"
max_steps = 50
max_tokens = 16384
temperature = 0.2

[permissions]
mode = "danger-full-access"   # ask | read-only | danger-full-access
```

环境变量：`RSH_MODEL`、`RSH_BASE_URL`、`RSH_API_KEY_FILE`、`RSH_PERMISSION_MODE`、`RSH_HOME`、`RSH_LIVE`、`RSH_DSH_TRANSPORT=cli`（强制 dsh 走 CLI）。

## 权限

- **默认 `danger-full-access`**：主 agent 和子代理都不逐条确认。这是有意的产品选择，也意味着一条坏命令可以直接改坏文件或泄露数据。
- `ask`：每次 bash 命令单独审批（TUI 里回答 allow / deny / allow-session）。
- `read-only`：读操作放行，写操作与变更型 bash 拒绝。
- 子代理 ACP 的 `request_permission` 自动回 `allow-once`。

## 测试

```bash
cargo test                                   # 单测 / 契约测试，不烧 token

RSH_LIVE=1 cargo test --test e2e_project     # 真实模型：Python todo CLI + unittest
RSH_LIVE=1 cargo test --test e2e_grill_me    # 真实模型 + pty：grill-me 追问并产出结果文件
```

live 测试串行执行；grill-me 用例约 4 分钟，结果写在临时 workspace（或仓库根）的 `grill-me-result.md`。

## 已知限制

- inline viewport 依赖终端回答 CPR；拿不到时自动降级为全屏（不接管 alt screen）。
- composer 目前只支持在末尾编辑（没有方向键移动/选区）。
- `read-only` 是对工具层的约束，不是内核级沙箱；`ask` 的审批同样不能阻止 bash 内部再派生进程。
- dsh ACP 冷启动约 2 秒；v1 每个子任务新建会话，不跨任务复用。

## 设计文档

- [开发计划](design/rsh-development-plan.md)
- [设计规格](docs/superpowers/specs/2026-10-31-rsh-coding-agent-design.md)
- [术语表](CONTEXT.md)
- [决策记录](docs/adr/)
- [终端输入调研](design/terminal-input-research.md)
