# rsh 开发计划（Coding Agent v1）

> 状态：设计已确认，待实施
> 日期：2026-10-31
> 适用对象：接手 rsh 开发的 agent / skill / 人
> 关联文档：[Codex / CodeWhale 终端输入调研](terminal-input-research.md)

> **实现状态（2026-10-31）**：M1–M5 已完成；`cargo test` 与两个 live e2e（Python 项目、grill-me）通过。实现期偏差：`workspace-write` 改为 `read-only`；inline viewport 拿不到 CPR 时降级为全屏；composer 暂只支持末尾编辑；新增 `src/prompt.rs`（统一系统提示词）与 `Approver`/`SessionApprovals` 审批层。

---

## 0. 一页摘要

rsh 是一个 Rust 编写的终端 coding agent：主 agent 与用户对话，内部跑自己的 tool loop；子代理可以**进程内实现**，也可以**通过 ACP 驱动 dsh-tui 的 ACP profile**（失败降级 `dst run`）；主 agent 不启动子代理时，也能直接加载并执行 `~/.agents/skills/grill-me` 这类 skill。

终端交互借鉴 Codex：**inline viewport + 底部多行 composer**，历史进 scrollback，不抓鼠标。

v1 必须通过两个端到端测试：

1. 用真实模型完成一个 Python todo CLI 项目（含 unittest 并通过）。
2. 在 TUI 里加载 `grill-me` skill，完成多轮追问，并把最终结论写到 `grill-me-result.md`。

---

## 1. 目标与验收

### 1.1 目标

- 单二进制 `rsh`，可在任意目录启动，读写当前 workspace。
- 自带 agent loop（不依赖任何 agent 框架），流式输出 + 工具调用。
- 9 个内置工具：`bash`、`read_file`、`write_file`、`edit_file`、`glob`、`grep`、`ask_user`、`skill`、`subagent`。
- 通用 skill loader：扫描全局与项目 skill，注入 `SKILL.md` 与资源根目录，支持 “Call the Skill tool with X” 的二次跳转。
- 两种 subagent 后端：
  - `native`：进程内独立 context；
  - `dsh`：ACP 客户端驱动 `dsh --profile acp`，失败降级 `dst run --output-format stream-json`。
- 会话落盘（JSONL）与恢复；`/compact` 上下文压缩。

### 1.2 非目标（v1 不做）

- 鼠标选择/拖拽、图片输入、语法高亮、完整 Markdown 渲染。
- MCP 客户端、web 搜索、多用户/远程服务。
- 子代理递归（subagent 不能再调 subagent）。
- 对 dsh 交互式 TUI 的屏幕抓取（只走 ACP / `dst run`）。

---

## 2. 已定决策（Q1–Q19）

| # | 决策 | 结论 |
|---|---|---|
| Q1 | 交付范围 | 完整 MVP，按 M1–M5 里程碑推进 |
| Q2 | TUI 形态 | Codex 式 inline viewport，不抓鼠标；reasoning 默认折叠 |
| Q3 | 工具集 | 上述 9 个；`edit_file` 用精确字符串替换 |
| Q4 | 权限 | **主 agent 默认 `danger-full-access`**（用户确认）；保留 `ask` 模式；`--yolo` 等价全开 |
| Q5 | 提问 | 阻塞式 `ask_user` 工具（问题 + 选项 + 自由输入） |
| Q6 | Skills | 通用 loader + `/skill` 显式调用 + 二次跳转 |
| Q7 | native subagent | 独立 context；除 `subagent` 外全部工具；深度 1；并发 4；超时 10 分钟；只回最终文本 + 改动文件 |
| Q8 | dsh 后端 | C：ACP 优先，`dst run` 兜底；adapter trait 隔离 |
| Q9 | 模型/会话 | OpenAI 兼容流式；`deepseek-flash`；key 读 `~/.config/ds.key`；`/compact`；JSONL 落盘 + 恢复 |
| Q10 | 测试 | 真实 API + `RSH_LIVE=1` gate；Python todo；pty 驱动 grill-me |
| Q11 | 系统提示词 | 自写精简版，结构参考 dsh，不复制原文 |
| Q12 | ACP 客户端 | 用 `agent-client-protocol 3.6`（`process` feature），已编译验证；手写兜底 |
| Q13 | ACP 降级 | 仅 spawn/握手/协议失败降级 `dst run`，任务失败不降级 |
| Q14 | 子代理权限 | 自动应答 `request_permission`；配置可改；**默认 `danger-full-access`**（含越界/网络） |
| Q15 | `/compact` | LLM 摘要替换旧消息，保留最近 2 轮；写 `compaction` 事件；失败回退截断；支持 `/compact <指令>` |
| Q16 | 会话 | JSONL 每事件一行；`--resume` / `--continue` / `/resume` / `/new`；子代理会话独立存放 |
| Q17 | 渲染 | 纯文本 + 轻量样式；不做完整 Markdown/语法高亮 |
| Q18 | e2e | grill-me 固定 4 轮问答；项目用 Python 标准库；live 串行、10 分钟超时 |
| Q19 | ACP 会话 | 每任务 `session/new`，不复用；预留 `session_id` 参数 |

---

## 3. 架构总览

```
┌──────────────────────────── TUI (ratatui inline) ────────────────────────────┐
│  composer  │  streaming transcript  │  approval bar  │  slash popup          │
└───────────────▲───────────────────────────────────────────────▲──────────────┘
                │ AppEvent                                      │ render
┌───────────────┴───────────────────────────────────────────────┴──────────────┐
│                              App / Agent Loop                                 │
│  messages ──► LLM stream ──► text delta / tool_call delta ──► tool dispatch    │
│      ▲                                                              │         │
│      └──────────────── tool result ◄────────────────────────────────┘         │
└───────┬───────────────────────┬───────────────────────┬───────────────────────┘
        │                       │                       │
   LlmClient                ToolRegistry            SubagentManager
 (DeepSeek/OpenAI)      bash/files/search/ask      native | dsh(ACP→CLI)
        │                       │                       │
   SSE /chat/completions    workspace fs           dsh --profile acp (JSON-RPC)
                                                     └─ fallback: dst run (NDJSON)
```

事件流：终端按键 → `AppEvent` → 状态机 → `LlmClient` 流 → 渲染 + 工具执行 → 会话事件落盘。

---

## 4. 工程结构与依赖

### 4.1 目录

```
rsh/
  Cargo.toml
  README.md
  design/
    rsh-development-plan.md
    terminal-input-research.md
  scripts/
    dev-env.sh              # 本机 cargo 代理（见 §19）
  src/
    main.rs
    cli.rs
    config.rs
    error.rs
    llm/
      mod.rs                # LlmClient trait
      openai.rs             # /chat/completions + SSE
      types.rs              # Message / ToolCall / Usage
    agent/
      mod.rs
      run.rs                # turn/step 状态机
      context.rs            # token 估算、截断
      compact.rs            # /compact
      session.rs            # 运行时 session
    tools/
      mod.rs                # Tool trait + registry + schema
      bash.rs
      files.rs              # read/write/edit
      search.rs             # glob/grep
      ask_user.rs
      skill.rs
      subagent.rs
    skills.rs               # 发现、解析、注入
    subagent/
      mod.rs                # SubagentBackend trait + manager
      native.rs
      acp.rs
      dsh_cli.rs
    session/
      store.rs              # JSONL append/read
      resume.rs             # 重放
    tui/
      mod.rs
      app.rs                # 状态机 + 事件循环
      composer.rs
      render.rs
      approval.rs
      slash.rs
      resume_picker.rs
  tests/
    mock_llm.rs
    session_replay.rs
    skills.rs
    e2e_project.rs          # RSH_LIVE=1
    e2e_grill_me.rs         # RSH_LIVE=1 + pty
```

### 4.2 依赖（建议版本）

| crate | 用途 |
|---|---|
| `tokio` (full) | 异步运行时、进程、信号 |
| `reqwest` (`json`,`stream`,`rustls-tls`) | LLM HTTP/SSE |
| `futures-util` | 流处理 |
| `serde` / `serde_json` | 协议与会话 |
| `ratatui 0.30` + `crossterm 0.29` | TUI |
| `agent-client-protocol 3.6` (`process`) | dsh ACP 客户端 |
| `clap 4` (derive) | CLI |
| `anyhow` / `thiserror` | 错误 |
| `unicode-width` / `unicode-segmentation` | composer |
| `dirs` | 配置/数据目录 |
| `portable-pty` + `vt100`（dev） | pty e2e |

> 若 `ratatui 0.30` 的 inline viewport API 不满足要求，退回 `0.29`（Codex 同款），不要自研 viewport。

---

## 5. 数据模型与会话格式

### 5.1 内存模型

```rust
struct Message {
    role: Role,              // System | User | Assistant | Tool
    content: String,
    reasoning: Option<String>,   // 仅 assistant，存但默认不回传
    tool_calls: Vec<ToolCall>,   // assistant
    tool_call_id: Option<String>,// tool
}

struct ToolCall { id: String, name: String, arguments: serde_json::Value }

struct Session {
    id: String,
    cwd: PathBuf,
    model: String,
    messages: Vec<Message>,
    created_at: DateTime<Utc>,
}
```

### 5.2 磁盘布局

```
~/.rsh/
  config.toml
  sessions/<cwd-slug>/<session-id>/events.jsonl
  subagents/<session-id>/<subagent-id>.jsonl
  logs/rsh.log
```

`cwd-slug`：绝对路径去掉开头的 `/`，`/` 替换为 `-`，首尾加 `--`（例如 `--home-algo-proj--`）。

### 5.3 JSONL 事件（v1）

每行一个 JSON 对象，`type` 区分：

```jsonc
{"v":1,"ts":"2026-10-31T00:00:00Z","type":"session_start","session_id":"...","cwd":"/abs","model":"deepseek-flash"}
{"v":1,"ts":"...","type":"user_message","text":"..."}
{"v":1,"ts":"...","type":"assistant_message","text":"...","reasoning":"...","tool_calls":[{"id":"call_1","name":"bash","arguments":{"command":"ls"}}]}
{"v":1,"ts":"...","type":"tool_result","call_id":"call_1","name":"bash","ok":true,"content":"...","truncated":false}
{"v":1,"ts":"...","type":"compaction","summary":"...","replaces_until":12,"reason":"manual"}
{"v":1,"ts":"...","type":"turn_end","reason":"completed","usage":{"input":1,"output":2}}
```

恢复规则：顺序重放；遇到 `compaction` 用 `summary` 替换 `messages[..replaces_until]`（保留 system + 最近消息）；未知 `type` 忽略并告警。

---

## 6. LLM 客户端

### 6.1 接口

```rust
#[async_trait]
trait LlmClient: Send + Sync {
    async fn stream_chat(
        &self,
        req: ChatRequest,
        sink: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<ChatOutcome>;

    async fn complete(&self, req: ChatRequest) -> Result<String>; // compact 用
}
```

`LlmEvent`：`TextDelta(String)`、`ReasoningDelta(String)`、`ToolCallDelta{index,id?,name?,args_delta}`、`Usage(Usage)`、`Done(stop_reason)`。

### 6.2 协议要点

- `POST {base_url}/chat/completions`，`Authorization: Bearer <key>`；base 默认 `https://api.deepseek.com`。
- `stream: true`，SSE 行格式 `data: {json}`，结束 `data: [DONE]`；忽略空行/注释。
- tool_call 增量按 `index` 聚合：`id`/`name` 首次出现，`arguments` 逐段拼接，结束后 `serde_json::from_str`；解析失败返回可读错误给模型重试。
- `reasoning_content`：单独存 `Message.reasoning`，**默认不回传**；若真实 API 在带 tool 的历史里要求回传，用配置 `send_reasoning = true` 切换（M1 用 live 测试确认）。
- usage：请求带 `stream_options.include_usage=true`，取最后一个 chunk；缺失则用 `tiktoken`-free 估算（字符数/3）兜底。
- 重试：仅网络错误/429/5xx，指数退避 3 次；4xx 不重试。
- 超时：连接 30s，整体 10min（可配）。

### 6.3 请求结构

```json
{
  "model": "deepseek-flash",
  "messages": [...],
  "tools": [...],
  "tool_choice": "auto",
  "stream": true,
  "stream_options": {"include_usage": true},
  "max_tokens": 16384,
  "temperature": 0.2
}
```

> 依据 2026-10-30 实测：`deepseek-flash` 支持 tool calling，返回 `tool_calls`；context 1,048,576，max output 393,216。

---

## 7. Agent Loop

### 7.1 状态机

```
Idle → BuildingRequest → Streaming → (ToolCalls? ExecutingTools → BuildingRequest)
                                    └─ (NoToolCalls) → TurnDone
任意状态 → Cancelled / Failed
```

- `max_steps = 50`（每步 = 一次模型调用 + 其工具执行）；超出向用户报错并结束 turn。
- 一条 assistant 消息里的多个 tool call **顺序执行**，每个结果单独 `role=tool` 消息。
- 工具执行失败不终止 loop：把错误文本作为 tool result 返回，模型自行决定重试/放弃。
- 用户取消（`Esc`/`Ctrl+C`）：取消当前流 + 正在跑的工具（kill child），保留已产生文本，落盘。
- turn 结束后：`turn_end` 事件 + 更新 usage/上下文占用。

### 7.2 上下文构建

顺序：system prompt →（可选）skills 目录摘要 → 历史 messages（含 compaction 摘要）→ 当前 user。

工具结果超过 `tool_output_limit`（默认 30_000 字符）时截断，保留头尾并标注 `[truncated N chars]`；`bash` 保留最后 200 行优先。

---

## 8. 工具

统一 trait：

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> serde_json::Value;   // OpenAI function schema
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}
```

`ToolOutput { content: String, is_error: bool, changed_files: Vec<PathBuf> }`。

### 8.1 工具清单

| 名称 | 参数 | 行为 |
|---|---|---|
| `bash` | `command`, `timeout_ms?`, `cwd?` | `sh -c`（Windows 用 `cmd /C`）；捕获 stdout/stderr；返回 `[exit code: N]` 标记；超时 kill 进程组 |
| `read_file` | `path`, `offset?`, `limit?` | 按行读，带行号；二进制检测拒绝 |
| `write_file` | `path`, `content` | 自动建父目录；覆盖写；返回字节数 |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | 精确匹配；未命中返回 diff 提示；多处匹配且未 `replace_all` 报错 |
| `glob` | `pattern`, `path?` | 基于 `globset`/`walkdir`，按 mtime 排序，最多 200 条 |
| `grep` | `pattern`, `path?`, `include?` | 正则搜索，输出 `file:line: text`，最多 250 条 |
| `ask_user` | `question`, `options?` | 阻塞，TUI 弹问题卡；返回用户输入（选项或自由文本） |
| `skill` | `name` | 加载 `SKILL.md`，注入正文 + 资源根目录；`disable-model-invocation` 的技能拒绝自动调用 |
| `subagent` | `prompt`, `backend?`, `cwd?`, `timeout_secs?` | 见 §10 |

### 8.2 路径与安全

- 所有路径先 `canonicalize`（不存在的父目录用 `normalize`），默认限制在 `ctx.workspace` 内；越界需 `danger-full-access`。
- `bash` 默认继承环境；不主动注入 `DSH_*`。
- 默认权限为 `danger-full-access`（用户确认），但 `ask` 模式下 `bash` 必须走审批。

---

## 9. Skills

### 9.1 发现

搜索顺序（后者覆盖前者同名）：

1. `~/.agents/skills/*/SKILL.md`
2. `<workspace>/.agents/skills/*/SKILL.md`

解析 YAML frontmatter：`name`、`description`、`disable-model-invocation`。正文保留原文。

### 9.2 注入

`skill(name)` 返回：

```
<skill name="grill-me" base="/home/algo/.agents/skills/grill-me">
...SKILL.md 正文...
</skill>

Resource base directory: /home/algo/.agents/skills/grill-me
Resolve relative paths in the instructions against this directory.
```

模型看到后按正文执行；正文若写 “Call the Skill tool with X”，模型应再次调用 `skill(X)`（二次跳转）。

### 9.3 调用入口

- 模型：`skill` 工具；system prompt 里列出可用技能（`disable-model-invocation: true` 的不列）。
- 用户：TUI `/skill <name> [args]`，直接把技能正文作为 user 消息注入（绕过 `disable-model-invocation` 限制），args 附在正文后。
- 启动时若 `<workspace>/AGENTS.md` 存在，追加到 system prompt（截断 64 KiB）。

### 9.4 grill-me 特殊说明

`~/.agents/skills/grill-me/SKILL.md` 只有一句 `Call the Skill tool with "grilling"`，因此必须支持二级跳转；`grilling` 的实际内容在 `~/.agents/skills/grilling/SKILL.md`。

---

## 10. Subagent

### 10.1 统一接口

```rust
#[async_trait]
trait SubagentBackend: Send + Sync {
    fn id(&self) -> &'static str;             // "native" | "dsh-acp" | "dsh-cli"
    async fn run(&self, task: SubagentTask, cancel: CancellationToken) -> Result<SubagentResult>;
}

struct SubagentTask { prompt: String, cwd: PathBuf, timeout: Duration, permission: PermissionMode }
struct SubagentResult { status: Status, summary: String, files_changed: Vec<PathBuf>, backend: String, session_id: Option<String> }
```

`SubagentManager`：`Semaphore(4)`、超时 600s、每个任务写 `~/.rsh/subagents/<session>/<id>.jsonl`。

### 10.2 native

- 新建 `Session`（空 messages + 专用 system prompt），工具注册表剔除 `subagent`（防递归），`ask_user` 也剔除（子代理不能打断用户），其余保留。
- 循环同主 agent；最多 50 步；返回最后一条 assistant 文本。
- `files_changed`：`write_file`/`edit_file` 记录 + 结束时 `git status --porcelain`（若在 git repo）合并去重。
- 不把子代理完整 transcript 回灌主上下文，只回 `SubagentResult`。

### 10.3 dsh（ACP）

- 启动：`AcpAgent::from_str("<dsh_bin> --profile acp")`（默认 `dsh`，可用配置覆盖）；env 注入 `DEEPSEEK_API_KEY`（读 `~/.config/ds.key`）、`DSH_HOME`（可选）、`TMPDIR`。
- 流程：
  1. `initialize`（protocolVersion 1，clientCapabilities 最小）；
  2. `session/new {cwd, mcpServers: []}`；
  3. `session/set_config_option`：`configId="model"`，`value` = **JSON 字符串** `["deepseek-official","deepseek-flash"]`（不是裸模型名）；可选 `reasoning_effort`；
  4. `session/prompt`，收集 `session/update` 的 `agent_message_chunk`；
  5. `session/close`，关 stdin，等退出。
- 权限：收到 `session/request_permission` 时自动回 `allow-once`（默认 `danger-full-access`）；配置可改为 `reject-once` 或转发。
- 取消：`session/cancel` 通知，等 `stopReason="cancelled"`；未决 permission 回 `cancelled`。
- 错误：`-32602` / `-32601` / `-32603` 映射为可读错误；`session/load` 不存在，恢复只能用 `session/resume`（cwd 必须完全一致、不回放历史）。
- 注意：服务端请求 id 从 0 开始，会与客户端 id 冲突；必须按消息形状分发（`method+id` = 请求，只有 `method` = 通知，只有 `id` = 响应）。

### 10.4 dsh（CLI 兜底）

传输/握手失败时降级：

```bash
dst run --cwd <cwd> --provider deepseek-official --model deepseek-flash \
  --permission-mode danger-full-access --output-format stream-json --timeout <ms> "<prompt>"
```

解析 NDJSON 的 `result` 行取 `text`；`error` 行映射为失败。环境需 `DSH_HOME`/`TMPDIR` 可写。

### 10.5 选择策略

- 工具参数 `backend`：缺省 `native`；`dsh` 表示 ACP→CLI。
- system prompt 指引：需要“另一个独立 agent 并行探索/实现”时用 `native`；需要复用 dsh 的完整工具链/模型路由时用 `dsh`。

---

## 11. TUI

### 11.1 布局

```
┌ transcript (inline viewport，滚入 scrollback) ─────────────┐
│ 你 › ...                                                    │
│ rsh › 流式文本 ...                                          │
│   ⚙ bash  ls -la                            [ok 0.2s]      │
│                                                             │
├ status line: model · ctx 12% · mode · cwd ──────────────────┤
│ › 输入框（1–10 行，软换行）                                  │
│   / commands   ! shell   Ctrl+J newline                     │
└─────────────────────────────────────────────────────────────┘
```

- 使用 `ratatui::Terminal` + `Viewport::Inline(h)`（M2 先验证 API）；已完成消息用 `insert_before` 写入 scrollback。
- 不启用鼠标；启用 bracketed paste、Kitty `DISAMBIGUATE_ESCAPE_CODES`、focus change。
- 退出/panic/`Ctrl+Z` 必须恢复终端（guard + panic hook）。

### 11.2 按键

| 键 | 行为 |
|---|---|
| `Enter` | 发送（空闲）/ 入队（忙） |
| `Shift+Enter` / `Ctrl+J` | 换行 |
| `Esc` | 取消当前 turn / 关闭弹窗 |
| `Ctrl+C` | 取消；空输入再按退出 |
| `Ctrl+D` | 空输入退出 |
| `↑` / `↓` | 历史 / 弹窗选择 |
| `Tab` | 弹窗补全 |
| `Ctrl+O` | 展开/折叠 reasoning |
| `!cmd` | 直接执行 shell，不进模型 |
| `/` | 命令弹窗 |

### 11.3 斜杠命令

`/compact [指令]`、`/resume`、`/new`、`/skill <name>`、`/reasoning`、`/help`、`/quit`。

### 11.4 审批

`ask` 模式下 `bash` 在 composer 位置显示：

```
允许执行？ rm -rf target/
[y] 允许一次   [a] 本会话全部允许   [n] 拒绝
```

默认 `danger-full-access` 时不弹。

### 11.5 渲染

- assistant 文本：逐段 append，按宽度软换行；代码块用 `│` 左边框 + 暗色背景，不做高亮。
- 工具调用：单行 `⚙ name args…`；完成后 `[ok/err Nms]`，`Ctrl+O` 展开结果。
- reasoning：默认只显示 `thinking…` 状态；`/reasoning` 或 `Ctrl+O` 展开。
- 错误：红色，保留在 transcript。

---

## 12. 权限

```toml
[permissions]
mode = "danger-full-access"   # ask | read-only | danger-full-access
```

| 模式 | 文件 | bash | 子代理 |
|---|---|---|---|
| `ask` | workspace 内自动，越界询问 | 逐条询问 | 继承 |
| `read-only` | 只读，写操作拒绝 | 变更型命令拒绝 | 继承 |
| `danger-full-access` | 全开 | 全开 | 全开（默认） |

- `--yolo` = `danger-full-access`。
- ACP `request_permission`：`danger-full-access` 下自动 `allow-once`；`ask` 下转发主 agent 的审批 UI（v1 可先自动拒绝并记录）。
- 文档与 `--help` 必须提示默认全开的风险。

---

## 13. 压缩 `/compact`

流程：

1. 收集当前 messages（不含 system），保留最近 2 轮（user+assistant+tool）。
2. 对更早部分调用一次 `complete()`，prompt：
   ```
   把下面的对话压缩成给后续开发用的摘要，保留：目标、约束、已改文件、关键决策、未完成事项。
   <对话>
   ```
3. 用 `[compaction summary]` 消息替换旧区间；追加 `compaction` 事件（`replaces_until` = 被替换的消息条数）。
4. 失败：回退到“按 token 截断最老消息”，并提示用户。
5. 自动提示：每轮结束估算 context 占用 > 80% 时，在 status line 提示 `/compact`，不自动执行。

`/compact <指令>`：把指令附加到摘要 prompt（例如“保留 API 设计”）。

---

## 14. 配置

路径：`~/.config/rsh/config.toml`（`RSH_CONFIG` 可覆盖）。

```toml
model = "deepseek-flash"
base_url = "https://api.deepseek.com"
api_key_file = "~/.config/ds.key"
max_steps = 50
max_tokens = 16384
temperature = 0.2
tool_output_limit = 30000
send_reasoning = false

[permissions]
mode = "danger-full-access"

[tui]
reasoning = "hidden"        # hidden | shown
alternate_screen = "never"  # never（inline）| always

[subagent]
backend = "native"          # native | dsh
permission = "danger-full-access"
max_concurrent = 4
timeout_secs = 600

[subagent.dsh]
transport = "acp"           # acp | cli
dsh_bin = "dsh"
profile = "acp"
fallback = "cli"
dsh_home = ""               # 空 = 继承环境
```

环境变量覆盖：`RSH_MODEL`、`RSH_BASE_URL`、`RSH_API_KEY_FILE`、`RSH_PERMISSION_MODE`、`RSH_SUBAGENT_BACKEND`。

API key 读取顺序：`RSH_API_KEY` → `api_key_file`（去首尾空白）→ `DEEPSEEK_API_KEY`。

---

## 15. 系统提示词

自己写，结构参考 dsh（身份 → 环境 → 工具 → 工作流 → 技能 → 子代理 → 提问 → 安全 → 输出风格）。要点：

1. 身份：`You are rsh, a terminal coding agent.`
2. 环境：cwd、OS、日期、模型、权限模式、是否 git repo。
3. 工具：每个工具一句话；强调 `bash` 看 `[exit code: N]`、编辑前先读文件。
4. 工作流：先探索再改；小步修改；跑测试；不要臆造文件内容。
5. 技能：列出可用 skill（名称+描述）；说明“用户 `/skill` 注入的正文具有最高优先级”。
6. 子代理：何时用、`native` 与 `dsh` 的区别、子代理看不到当前对话。
7. 提问：`ask_user` 用于真正需要用户决策的点；grill-me 式追问一次只问一个。
8. 安全：默认全开，但仍避免 `rm -rf /`、凭证外泄；危险命令先说明。
9. 输出：简洁、可执行、少客套；代码块标注语言。

`AGENTS.md`（若存在）追加到 system prompt 末尾，注明来源与截断。

---

## 16. 测试

### 16.1 单元 / mock（不烧 token）

- `llm::openai`：SSE 解析 fixture（文本、tool_call 增量、usage、`[DONE]`、畸形行）。
- `session`：事件追加/重放/compaction 重建。
- `skills`：frontmatter 解析、优先级覆盖、二次跳转、`disable-model-invocation`。
- `tools`：edit 精确匹配/多匹配、路径越界、bash 退出码。
- `agent`：mock `LlmClient` 驱动 3 步循环（文本→工具→文本）。
- TUI：`ratatui::backend::TestBackend` 快照（composer 高度、审批条）。

### 16.2 live（`RSH_LIVE=1`）

- `e2e_project`：
  - 临时目录；`rsh exec --yolo "<固定 prompt>"`；
  - prompt：用 Python 标准库写 `todo.py`（add/list/done）+ `test_todo.py`（unittest），运行测试；
  - 断言：文件存在、`python3 -m unittest -q` 退出码 0。
- `e2e_grill_me`：
  - `portable-pty` 启动 `rsh`；
  - 发送 `/skill grill-me` + 任务描述（明确要求“最终结论写到 `grill-me-result.md`”）；
  - 脚本对每次 `ask_user` 回固定答案（“按你的建议”，**最多 6 轮**），然后发送“请输出最终结论到 `grill-me-result.md`”；
  - 等待文件出现；
  - 断言包含“目标 / 约束 / 决策 / 风险 / 下一步”五节。
- 串行执行；单测 10 分钟超时；失败保留 pty 日志到 `target/e2e-logs/`。

### 16.3 手工验收

```bash
cargo run -- "在当前目录建一个 hello.py 并运行"
cargo run -- --resume <id>
cargo run -- exec --yolo "写一个 Python todo CLI 并跑测试"
```

---

## 17. 里程碑

| 里程碑 | 交付 | 验收命令 |
|---|---|---|
| **M1** 核心 | config、LLM 客户端、loop、9 工具、`rsh exec`、会话 JSONL | `cargo test`；`rsh exec --yolo "创建 a.txt 写入 hi 并 cat"` |
| **M2** TUI | inline viewport、composer、流式、审批、`!`、`/` | pty 冒烟：输入→流式→审批→退出；`cargo test` |
| **M3** Skills/会话 | skill loader、`ask_user`、`/compact`、`--resume`/`/resume` | skills 单测；resume 重放；手动 grill-me 前 2 轮 |
| **M4** 子代理 | native + dsh ACP + CLI 兜底 | `subagent` 工具单测；live ACP 测试 |
| **M5** e2e/文档 | 两个 e2e、README、本文件更新 | `RSH_LIVE=1 cargo test --test e2e_project --test e2e_grill_me` |

每个里程碑结束提交一次，提交信息格式 `M1: ...`。

---

## 18. 风险与对策

| 风险 | 对策 |
|---|---|
| `reasoning_content` 回传策略不确定 | M1 用 live 测试两种模式；配置开关 |
| ratatui inline viewport 的 scrollback 行为因终端而异 | M2 先做 3 个终端冒烟（tmux/kitty/gnome-terminal）；失败退回全屏 + `/inline` |
| ACP crate 3.x API 新、文档少 | 已编译验证；先写最小 example 跑通再集成；保留手写 NDJSON 兜底 |
| `dsh` 启动慢（~2.1s Node boot） | 子代理提示“冷启动”；常驻会话后续再做 |
| 默认 danger-full-access 的安全风险 | README/启动 banner 明示；`ask` 模式可切换 |
| Cargo 依赖（本机 IPv6 不通） | `scripts/dev-env.sh` 起本地 IPv4 CONNECT 代理；不改全局配置 |
| 子代理并发导致 API 限流 | Semaphore(4) + 429 退避 |
| 大工具输出撑爆上下文 | 30k 截断 + compact |

---

## 19. 本机环境（重要）

- Rust：`rustc 1.96.0` / `cargo 1.96.0`。
- `deepseek-flash`：`https://api.deepseek.com`，key `~/.config/ds.key`（36 字节，`sk-` 开头）。
- `dsh`：`0.2.1-rc.2`；ACP profile 为 `dsh --profile acp`；`dst` 为 dsh-tui 的自动化 CLI。
- **cargo 问题**：全局 `~/.cargo/config.toml` 把 crates.io 替换为 `nora.tdengine.net`，本机 IPv6 不通导致 cargo 超时。已用本地 IPv4 CONNECT 代理验证可行：

```bash
python3 .tmp/net/connect_proxy.py 18080 &
export CARGO_HTTP_PROXY=http://127.0.0.1:18080
export CARGO_HOME=$PWD/.tmp/cargo-home
cargo build
```

`scripts/dev-env.sh` 固化这套逻辑（仅本机开发用，不进入产品运行时）。

- dsh ACP 相关环境（子代理）：`DEEPSEEK_API_KEY`（从 ds.key 读）、`DSH_HOME`（默认继承 `~/.dsh`，测试用临时目录）、`TMPDIR` 必须可写。
- 子代理默认 `danger-full-access`；dsh 侧对应 `DSH_PERMISSION_MODE=danger-full-access`。

---

## 20. 给下一个 agent 的交接清单

按顺序：

1. 读本文件 §2（决策）、§5（事件格式）、§7（loop）、§10（子代理）、§16（测试）。
2. 跑 `scripts/dev-env.sh`，确认 `cargo build` 能拉依赖。
3. 从 M1 开始：先写 `llm/openai.rs` 的 SSE 解析 + mock 测试，再写 loop，再接工具。
4. 每完成一个工具，补一条单测；每完成一个里程碑，跑对应验收命令。
5. M2 前先写一个 20 行的 ratatui inline viewport spike，确认 `insert_before` 行为；不通过就改用全屏。
6. M4 前先写一个独立 `acp_smoke` example（`agent-client-protocol` 连接 `dsh --profile acp`，发 “Reply OK”），通过后再集成进 `subagent/acp.rs`。
7. 不要修改 `~/.cargo/config.toml`；不要提交 `.tmp/`、`target/`、`grill-me-result.md`。
8. 测试产物写到 `target/e2e-logs/`；临时目录用 `<workspace>/.tmp/`（不要用 `/tmp`）。

### 20.1 未决/待验证

- [ ] `ratatui 0.30` 的 `Viewport::Inline` + `insert_before` 是否满足滚动历史需求。
- [ ] DeepSeek 在带 tool 历史的请求里是否接受/忽略 `reasoning_content`。
- [ ] `AcpAgent::from_str("dsh --profile acp")` 能否直接启动（否则用 `node <bin.js>`）。
- [ ] `dsh` 的 `session/set_config_option` 在 `reasoning_effort` 上的默认值是否需要显式设置。
- [ ] Python 解释器在测试环境是 `python3`（3.14.4 已确认）。

---

## 附录 A：ACP 实测帧（dsh 0.2.1-rc.2）

```jsonc
// → initialize
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"rsh","version":"0.1.0"}}}
// ← result
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"deepseek-harness-acp","version":"0.0.1"},"agentCapabilities":{"sessionCapabilities":{"close":{},"list":{},"resume":{}}},"authMethods":[]}}

// → session/new（mcpServers 必填）
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/abs/path","mcpServers":[]}}
// ← {"sessionId":"...","configOptions":[...]}

// → 设置模型（值必须是 JSON 字符串）
{"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"sessionId":"...","configId":"model","value":"[\"deepseek-official\",\"deepseek-flash\"]"}}

// → prompt
{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"...","prompt":[{"type":"text","text":"..."}]}}
// ← 通知：{"method":"session/update","params":{"sessionId":"...","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"..."}}}}
// ← result：{"stopReason":"end_turn"}

// → cancel（通知，无 id）
{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"..."}}
```

关键坑：服务端 `request_permission` 的 `id` 从 0 开始，与客户端 id 冲突；按形状分发。`session/load` 不存在。

---

## 附录 B：DeepSeek API 实测

- `GET /models` 返回 `deepseek-flash`（1,048,576 ctx / 393,216 max output）与 `deepseek-v4-pro`。
- `POST /chat/completions`，`tools` 标准 OpenAI schema；实测返回：

```json
{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_...","type":"function","function":{"name":"add","arguments":"{\"a\":2,\"b\":2}"}}]},"finish_reason":"tool_calls"}]}
```

- 文本请求会带 `reasoning_content`（推理内容），`content` 可能为空直到推理结束。

---

## 附录 C：参考

- Codex TUI：`/home/algo/rust_space/codex/codex-rs/tui`（inline viewport、composer、审批）
- CodeWhale：<https://github.com/Hmbown/CodeWhale>（全屏工作台对照）
- 终端输入调研：[terminal-input-research.md](terminal-input-research.md)
- ACP 探针报告：`.tmp/acp-probe/REPORT.md`（本地，未提交）
- `agent-client-protocol`：<https://github.com/agentclientprotocol/rust-sdk>
- DeepSeek API：<https://api-docs.deepseek.com>
