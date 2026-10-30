# rsh Coding Agent — Design Spec

- 状态：已实现并通过 live e2e（2026-10-31）
- 日期：2026-10-31
- 详细实现计划（实现细节以它为准）：[design/rsh-development-plan.md](../../../design/rsh-development-plan.md)
- 领域术语：[CONTEXT.md](../../../CONTEXT.md)
- 决策记录：[docs/adr/](../../../docs/adr/)

> 本 spec 定义 **做什么、为什么、验收标准**；`design/rsh-development-plan.md` 定义 **怎么做**（模块、接口、协议、里程碑）。两者冲突时，以本 spec 的范围与验收为准，实现细节以计划为准。

---

## 1. 问题

终端里的 coding agent 目前分成两类：一类是 Codex 这种“聊天流 + 工具循环”，交互自然但缺少可编程的子代理与技能生态；另一类是 DeepSeek Harness 这种完整 harness，能力强但启动重、终端交互与外部集成耦合。

rsh 要在**一个 Rust 二进制**里同时做到：

1. 自研 agent loop，不依赖 agent 框架；
2. Codex 式的终端输入体验（inline viewport + 底部 composer）；
3. 通用 skill 加载，能直接执行 `~/.agents/skills/grill-me`；
4. 子代理既能进程内实现，也能通过 ACP 驱动 dsh-tui 的 ACP profile，失败时降级到 dsh-tui 的 `dst run`。

## 2. 目标与非目标

### 2.1 目标

- `rsh` 单二进制，在任意目录启动，读写当前 workspace。
- 主 agent 与用户对话；子代理只与主 agent 交互，不直接面对用户。
- 不启动子代理时，主 agent 也能加载并执行 skill（grill-me 是验收用例）。
- 通过两个真实模型的端到端测试：简单项目开发、grill-me 多轮追问并产出文件。

### 2.2 非目标（v1）

- 鼠标选择/拖拽、图片输入、语法高亮、完整 Markdown 渲染。
- MCP 客户端、web 搜索、远程服务、多用户。
- 子代理递归（深度固定 1）。
- 对 dsh 交互式 TUI 的 pty 抓屏（只走 ACP / `dst run`）。

## 3. 用户与场景

- **开发者**：在项目目录运行 `rsh`，用自然语言让 agent 读代码、改文件、跑测试；长任务用子代理并行探索。
- **流程使用者**：用 `/skill grill-me` 让 agent 按 grilling 流程对自己连环追问，最后把结论写入文件。
- **自动化**：`rsh exec --yolo "…"` 在 CI/脚本里跑单次任务；`rsh --resume <id>` 继续上次会话。

## 4. 需求

### 4.1 终端交互（UX）

- **UX-1** 采用 bottom-anchored inline viewport；已完成消息写入终端 scrollback；退出后历史仍可查看。
- **UX-2** 底部 composer 支持多行、软换行、CJK/emoji 光标、括号粘贴；`Enter` 发送，`Shift+Enter`/`Ctrl+J` 换行。
- **UX-3** 流式渲染 assistant 文本；reasoning 默认折叠，`Ctrl+O`/`/reasoning` 展开。
- **UX-4** 工具调用显示为单行 `⚙ name args…`，完成后显示状态与耗时；结果可展开。
- **UX-5** `Esc` 取消当前 turn；`Ctrl+C` 取消，空输入再按退出；`Ctrl+D` 空输入退出。
- **UX-6** `/` 打开命令菜单：`/compact`、`/resume`、`/new`、`/skill`、`/reasoning`、`/help`、`/quit`。
- **UX-7** `!cmd` 直接执行 shell 并把输出写入 transcript，不经过模型。
- **UX-8** 不启用鼠标捕获；启用 bracketed paste、Kitty 键盘协议（仅 DISAMBIGUATE）、focus 事件。
- **UX-9** 任何退出路径（正常、panic、Ctrl-Z、SIGTERM）都必须恢复终端模式。

### 4.2 Agent loop（LOOP）

- **LOOP-1** 自研 turn/step 状态机：一次模型调用 + 其工具执行为一步；`max_steps = 50`。
- **LOOP-2** 一条 assistant 消息中的多个 tool call 顺序执行，每个结果单独作为 `role=tool` 消息回填。
- **LOOP-3** 工具失败不终止 loop，错误文本作为工具结果返回，由模型决定重试或放弃。
- **LOOP-4** 流式解析文本、reasoning、tool_call 增量（按 index 聚合 arguments）。
- **LOOP-5** 支持取消：中断 HTTP 流、kill 运行中的子进程、落盘已产生内容。
- **LOOP-6** 每轮结束记录 usage；上下文占用超阈值时提示 `/compact`，不自动压缩。

### 4.3 工具（TOOL）

必须提供 9 个工具，schema 见计划 §8：

`bash`、`read_file`、`write_file`、`edit_file`、`glob`、`grep`、`ask_user`、`skill`、`subagent`。

- **TOOL-1** 所有文件工具限制在 workspace 内，除非权限模式为 `danger-full-access`。
- **TOOL-2** `edit_file` 使用精确字符串替换；未命中或多处匹配（未指定 `replace_all`）返回可读错误。
- **TOOL-3** `bash` 捕获 stdout/stderr，返回 `[exit code: N]`；超时 kill 进程组。
- **TOOL-4** 工具输出超过 30k 字符时截断，保留头尾并标注。
- **TOOL-5** `ask_user` 阻塞等待用户在 TUI 中回答（选项或自由文本），返回给模型。

### 4.4 Skills（SKILL）

- **SKILL-1** 发现 `~/.agents/skills/*/SKILL.md` 与 `<workspace>/.agents/skills/*/SKILL.md`，项目级覆盖全局同名。
- **SKILL-2** 解析 frontmatter：`name`、`description`、`disable-model-invocation`。
- **SKILL-3** 注入正文时附带资源根目录，并声明“相对路径按该目录解析”。
- **SKILL-4** 支持二次跳转：正文要求“Call the Skill tool with X”时，模型再次调用 `skill(X)`。
- **SKILL-5** `disable-model-invocation: true` 的技能不进入模型可见列表，但用户可用 `/skill <name>` 显式加载。
- **SKILL-6** 启动时若存在 `<workspace>/AGENTS.md`，追加到 system prompt（上限 64 KiB）。

### 4.5 子代理（SUB）

- **SUB-1** 统一接口 `SubagentBackend`，至少两种：`native`、`dsh`。
- **SUB-2** `native`：独立 context、同模型、除 `subagent`/`ask_user` 外全部工具、深度 1、并发 4、超时 10 分钟；只回最终文本 + 改动文件。
- **SUB-3** `dsh`：优先 ACP（`dsh --profile acp`），JSON-RPC over stdio；spawn/握手/协议失败时降级 `dst run --output-format stream-json`；任务失败不降级。
- **SUB-4** ACP 流程：`initialize` → `session/new`（`mcpServers: []`、绝对 cwd）→ `set_config_option(model)` → `session/prompt` → 收集 `session/update` → `session/close`。
- **SUB-5** ACP 的 model 值必须是 JSON 字符串 `["deepseek-official","deepseek-flash"]`；reasoning effort 可选。
- **SUB-6** `session/request_permission` 自动应答 `allow-once`（默认全开）；配置可改为拒绝或转发。
- **SUB-7** ACP 请求 id 从 0 开始，与客户端 id 冲突；按消息形状分发。
- **SUB-8** 子代理结果包含 `status`、`summary`、`files_changed`、`backend`、`session_id`。
- **SUB-9** 子代理 transcript 存 `~/.rsh/subagents/<session>/<id>.jsonl`，不进入用户 resume 列表。

### 4.6 会话与压缩（SESS）

- **SESS-1** 会话以 JSONL 事件追加落盘：`session_start`、`user_message`、`assistant_message`、`tool_result`、`compaction`、`turn_end`。
- **SESS-2** 恢复时顺序重放；`compaction` 事件用摘要替换 `replaces_until` 之前的历史。
- **SESS-3** `rsh --resume <id>`、`rsh --continue`、TUI `/resume`、`/new`。
- **SESS-4** `/compact [指令]`：保留最近 2 轮，旧历史用一次独立模型调用生成摘要；失败回退截断。
- **SESS-5** 未知事件类型忽略并告警；schema 带 `v` 版本号。

### 4.7 模型与配置（MODEL）

- **MODEL-1** OpenAI 兼容 `/chat/completions`，SSE 流式，`tools` 标准 schema。
- **MODEL-2** 默认 `model=deepseek-flash`，`base_url=https://api.deepseek.com`。
- **MODEL-3** API key 读取顺序：`RSH_API_KEY` → `api_key_file`（默认 `~/.config/ds.key`）→ `DEEPSEEK_API_KEY`。
- **MODEL-4** 捕获 `reasoning_content`；默认不回传，配置 `send_reasoning` 可切换。
- **MODEL-5** 重试仅限网络错误/429/5xx，指数退避 3 次；4xx 不重试。
- **MODEL-6** 配置路径 `~/.config/rsh/config.toml`，环境变量可覆盖（见计划 §14）。

### 4.8 权限与安全（PERM）

- **PERM-1** 默认 `danger-full-access`：主 agent 与子代理都不逐条确认（用户确认的产品决策）。
- **PERM-2** 保留 `ask` 与 `read-only` 模式；`--yolo` 等价全开。
- **PERM-3** `ask` 模式下 `bash` 在 composer 位置显示审批条：`y` 允许一次、`a` 本会话允许、`n` 拒绝。
- **PERM-4** README 与启动 banner 必须提示默认全开的风险。

### 4.9 CLI（CLI）

- **CLI-1** `rsh [PROMPT]` 启动 TUI。
- **CLI-2** `rsh exec [--yolo] "<task>"` 无 TUI 单次执行，输出最终文本。
- **CLI-3** `rsh --resume <id>` / `--continue`；`rsh sessions` 列出会话；`rsh skills` 列出技能。
- **CLI-4** `--help` 说明权限默认值与配置路径。

## 5. 架构

```
TUI (ratatui inline) ── AppEvent ──► App / Agent Loop ──► LlmClient (SSE)
        ▲                                   │
        └──────── render ◄────────── ToolRegistry ──► workspace
                                            │
                                     SubagentManager
                                      ├─ native (in-process)
                                      └─ dsh (ACP → CLI fallback)
```

模块边界与文件布局见计划 §4。每个模块一个职责：

- `llm`：只负责 HTTP/SSE 与增量聚合，不认识工具。
- `agent`：turn/step 状态机、上下文、压缩，不直接做 IO。
- `tools`：每个工具独立实现 `Tool` trait，注册表统一分发。
- `skills`：只做发现/解析/注入，不执行。
- `subagent`：后端 trait + 进程/协议管理，不关心 TUI。
- `tui`：只消费 `AppEvent` 并渲染，不直接调用模型。
- `session`：事件落盘与重放。

## 6. 数据流

1. 用户输入 → `AppEvent::Submit` → 构建请求（system + 历史 + user）。
2. `LlmClient::stream_chat` → 文本增量渲染、tool_call 增量聚合。
3. 流结束：若有 tool calls → 依次执行 → 结果回填 → 回到 2；否则 turn 结束。
4. 每个事件追加 JSONL；turn 结束记录 usage。
5. 子代理：`subagent` 工具 → `SubagentManager` → backend → 结果作为 tool result 返回主 loop。

## 7. 接口契约

### 7.1 工具

```rust
trait Tool {
    fn name(&self) -> &str;
    fn schema(&self) -> serde_json::Value;          // OpenAI function schema
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}
struct ToolOutput { content: String, is_error: bool, changed_files: Vec<PathBuf> }
```

### 7.2 会话事件

```jsonc
{"v":1,"type":"user_message","text":"..."}
{"v":1,"type":"assistant_message","text":"...","reasoning":"...","tool_calls":[...]}
{"v":1,"type":"tool_result","call_id":"...","name":"bash","ok":true,"content":"..."}
{"v":1,"type":"compaction","summary":"...","replaces_until":12}
{"v":1,"type":"turn_end","reason":"completed","usage":{...}}
```

### 7.3 子代理结果

```json
{"status":"ok","summary":"...","files_changed":["src/a.rs"],"backend":"dsh-acp","session_id":"..."}
```

### 7.4 配置

见计划 §14；默认值必须与 §4.8 的权限决策一致。

## 8. 错误处理

- 模型/网络错误：可重试的退避重试；不可重试的显示在 transcript，turn 结束但会话保留。
- 工具错误：作为 tool result 返回，不 panic。
- ACP：`-32602`/`-32601`/`-32603` 映射为可读错误；传输失败降级 CLI；降级原因写入结果。
- 终端：panic hook + 信号 guard 恢复 raw mode/alt screen/cursor。
- 磁盘：会话写入失败降级为内存会话并提示，不阻断对话。

## 9. 测试与验收

### 9.1 自动化

- 单元：SSE 解析、tool_call 聚合、edit 语义、路径越界、skill 解析/跳转、会话重放/compaction、mock loop。
- TUI：`TestBackend` 快照（composer、审批条、slash 菜单）。
- live（`RSH_LIVE=1`，串行，10 分钟超时）：
  - **A1 项目开发**：固定 prompt 让 agent 用 Python 标准库写 todo CLI + unittest；断言文件存在且 `python3 -m unittest -q` 通过。
  - **A2 grill-me**：pty 启动 TUI，`/skill grill-me` + 任务；对每次提问回固定答案（最多 6 轮），要求写入 `grill-me-result.md`；断言含“目标/约束/决策/风险/下一步”。

### 9.2 手工

- `rsh` 输入 → 流式 → 工具 → 审批（ask 模式）→ 退出后 `stty` 正常。
- `rsh --resume <id>` 重放一致。
- `/compact` 后继续对话不丢关键上下文。

## 10. 里程碑

| 里程碑 | 范围 | 验收 |
|---|---|---|
| M1 | LLM 客户端、loop、9 工具、`rsh exec`、JSONL | `cargo test` + 真 API 改文件 |
| M2 | inline TUI、流式、审批、`!`、`/` | pty 冒烟 + 快照 |
| M3 | skills、ask_user、`/compact`、resume | 单测 + 手动 grill-me 前两轮 |
| M4 | native + dsh ACP + CLI 兜底 | ACP 集成测试（live gate） |
| M5 | 两个 e2e + 文档 | A1、A2 通过 |

## 11. 待验证项（实现期确认，不阻塞设计）

| 项 | 失败时的退路 |
|---|---|
| ratatui 0.30 `Viewport::Inline` + `insert_before` 行为 | 退回 ratatui 0.29，或全屏 + `/inline` |
| DeepSeek 带 tool 历史时是否接受 `reasoning_content` | `send_reasoning` 配置切换 |
| `AcpAgent::from_str("dsh --profile acp")` 能否直接启动 | 改为 `node <bin.js> --profile acp` |
| dsh `reasoning_effort` 默认值 | 显式 `set_config_option` |
| cargo 依赖拉取（本机 IPv6 问题） | `scripts/dev-env.sh` 本地代理 |

## 12. 参考

- 详细计划：[design/rsh-development-plan.md](../../../design/rsh-development-plan.md)
- 终端输入调研：[design/terminal-input-research.md](../../../design/terminal-input-research.md)
- 术语表：[CONTEXT.md](../../../CONTEXT.md)
- ADR：[0001](../../../docs/adr/0001-inline-viewport.md)、[0002](../../../docs/adr/0002-dsh-subagent-acp.md)、[0003](../../../docs/adr/0003-default-full-access.md)、[0004](../../../docs/adr/0004-session-jsonl.md)
