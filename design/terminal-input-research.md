# Codex / CodeWhale 终端命令输入方式调研

- 调研时间：2026-10-30
- Codex：本地源码 `/home/algo/rust_space/codex`（openai/codex，commit `cca36c568`），TUI crate 为 `codex-rs/tui`
- CodeWhale：开源仓库 [Hmbown/CodeWhale](https://github.com/Hmbown/CodeWhale) `main`（本机安装 v0.9.13，调研时最新 v0.10.2），TUI crate 为 `crates/tui`
- 目的：为 rsh（自研 agent CLI）的终端输入层做技术选型
- 调研方法：CodeWhale 在本机 tmux 里实机跑过（`/` 菜单、`F1` 帮助、`!` 输入）；Codex 本机未登录，交互部分以源码 + 官方文档为准，未做完整实机验证

---

## 0. 结论先行

1. **两者都不是“在 shell 里读一行命令”**，而是把终端切到 raw mode，用 `crossterm` 收按键、`ratatui` 绘制界面，并各自实现了一个“聊天式多行输入框（composer）”。
2. **命令输入 = composer + 前缀触发弹窗**：`/` 斜杠命令、`@` 文件、`!` shell；Codex 额外有 `$` 技能，CodeWhale 额外有 `#` 记忆、`Ctrl-K` 命令面板、内嵌 PTY 终端。
3. 输入链路上最大的差异是**谁拥有屏幕**：
   - **Codex**：主界面是底部锚定的 inline viewport，历史消息写进终端 scrollback；不抓鼠标；alt screen 只给 diff / 审批 / transcript 等全屏 overlay 用 → “终端原生聊天流”。
   - **CodeWhale**：默认 alt screen 全屏，接管鼠标，有工作栏、Hotbar、命令面板、内嵌终端；`/inline` 可退回 inline → “IDE 式工作台”。
4. 两者都**自研了输入框**（没用 `tui-textarea` / `reedline`），都处理了 paste burst、grapheme/CJK 宽度、终端恢复、子进程抢占 tty 这些坑。

---

## 1. 技术栈对照

| 维度 | Codex | CodeWhale |
|---|---|---|
| 语言 | Rust（edition 2024） | Rust 2024 |
| TUI 框架 | `ratatui 0.29`（patch 到 [nornagon/ratatui](https://github.com/nornagon/ratatui) 分支）+ 自研 `custom_terminal` | `ratatui 0.30.2` + vendored `codewhale-ratatui 0.1.3` 组件库 |
| 输入库 | `crossterm 0.28.1`（patch 到 nornagon/crossterm `color-query` 分支） | `crossterm 0.29` + 本地 patch（修 Windows 非 BMP 字符粘贴） |
| 异步 | tokio（`select!` 驱动事件循环） | tokio + 专用输入线程 |
| 其它 | 自研 `TextArea` / `ChatComposer` / 帧调度器 | `portable-pty 0.9`（内嵌终端）、`arboard`（剪贴板）、`unicode-segmentation` |
| 规模参考 | `chat_composer.rs` 8.2k 行、`textarea.rs` 2.5k 行、`app.rs` 10.9k 行 | `src/tui` 目录约 10.6 万行；`app/composer.rs` 2.1k 行、`command_palette.rs` 2.4k 行 |

> 关键结论：**两家的输入框和弹窗都是自己写的**。`ratatui` 只提供 Buffer / Widget / 布局，不提供“输入框语义”。

---

## 2. 终端输入链路

### 2.1 第一步：把终端切成“按键模式”

Codex 的 `set_modes()`（`codex-rs/tui/src/tui.rs:63`）：

```rust
execute!(stdout(), EnableBracketedPaste)?;
enable_raw_mode()?;
execute!(stdout(), PushKeyboardEnhancementFlags(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
))?;
execute!(stdout(), EnableFocusChange)?;
```

CodeWhale 的 `recover_terminal_modes()`（`crates/tui/src/tui/ui/terminal.rs:697`）做同一组事，但顺序和取舍不同：

```rust
// 仅 fullscreen 时抓鼠标
if use_mouse_capture { execute!(writer, EnableMouseCapture)?; }
execute!(writer, EnableBracketedPaste)?;
execute!(writer, EnableFocusChange)?;
// 只推 DISAMBIGUATE，不推 REPORT_EVENT_TYPES
execute!(writer, PushKeyboardEnhancementFlags(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
))?;
```

每一项的作用：

| 模式 | 解决的问题 |
|---|---|
| `enable_raw_mode()` | 关掉行缓冲和 `Ctrl-C`/`Ctrl-Z` 信号，按键直接到达进程；否则 `↑`、`Tab` 都被 shell 吃掉 |
| `EnableBracketedPaste` | 让一次粘贴变成单个 `Event::Paste(String)`，而不是几百个 `KeyCode::Char` + 换行（否则多行粘贴会在第一个 `\n` 直接发送） |
| Kitty keyboard protocol | 区分 `Enter` / `Shift+Enter` / `Ctrl+Enter`，以及 `Ctrl-/`、`Alt-1` 这类组合键 |
| `EnableFocusChange` | 失焦/聚焦事件，用于桌面通知、macOS IME 重组、暂停渲染 |
| `EnableMouseCapture` | 鼠标点击/拖拽/滚轮；Codex 完全不抓，CodeWhale 只在 fullscreen 抓 |

补充：Codex 的 alt screen 是**按需**的——只有 diff、审批、`Ctrl+T` transcript、backtrack 这些全屏 overlay 会临时 `enter_alt_screen()`，聊天主界面始终是 inline viewport；`tui.alternate_screen = auto|always|never` 和 `--no-alt-screen` 控制的是“overlay 要不要用 alt screen”（Zellij 下自动关闭，避免丢 scrollback）。

两家的**关键差异**：

- Codex 推了 `REPORT_EVENT_TYPES | REPORT_ALTERNATE_KEYS`，因此会收到 key release / repeat，代码里靠 `KeyEventKind::Press | Repeat` 过滤；好处是能拿到更精确的修饰键。
- CodeWhale 故意只推 `DISAMBIGUATE_ESCAPE_CODES`。源码注释说得很直白：更高等级会发 release 事件，现有 handler 会把它们当成重复按键误路由（`terminal.rs:1064` 附近）。
- CodeWhale 在 Windows 上绕过 crossterm 直接写 `\x1b[>1u` / `\x1b[<1u`，因为 crossterm 的 `PushKeyboardEnhancementFlags` 在 Windows 上无条件返回 `Unsupported`。

### 2.2 第二步：读事件

**Codex：tokio 事件流 + `select!`**

`app.rs:3831` 起，主循环同时等四路事件：

```rust
let tui_events = tui.event_stream();
tokio::pin!(tui_events);

loop {
    let control = select! {
        Some(event) = app_event_rx.recv() => app.handle_event(...),
        active = async { ... } => ...,
        Some(event) = tui_events.next() => app.handle_tui_event(...),
        app_server_event = app_server.next_event() => ...,
    };
}
```

`TuiEvent` 只有三种：`Key(KeyEvent)`、`Paste(String)`、`Draw`（`tui.rs:236`）。底层是 `crossterm::event::EventStream`，包在 `EventBroker` 里；需要把 stdin 让给 vim / `$EDITOR` 时**直接 drop 整个 EventStream**，用完再重建——注释解释了原因：crossterm 的 reader 线程即使不 poll 也会继续读 stdin，会偷走子进程的按键。

**CodeWhale：专用输入线程 + 通道**

`terminal_input.rs` 里有一个 `TerminalInputPump` 线程，循环：

```rust
event::read()  // 阻塞读
// 收到后打时间戳
TerminalInputMessage::Event(ObservedTerminalEvent { event, observed_at })
```

- 50ms 超时轮询（空闲 200ms），带心跳，避免主线程卡住时完全收不到输入。
- `observed_at` 是**收到按键的真实时间**，不是主循环处理时间；用于 paste burst 判定（否则主循环一卡，用户正常打字会被误判成粘贴）。
- 主循环每轮最多处理 16 个引擎事件 / 8ms，然后必须让位给输入（input fairness）。
- 把 tty 交给子进程（外部编辑器、终端 dock）前必须 `pause` 并等待 ack，失败就**不允许**启动子进程，否则按键会在子进程和 composer 之间被劈开。

> 取舍：Codex 的方案更“Rust async 原生”，但 crossterm 的 EventStream 在让出 stdin 时很别扭；CodeWhale 的方案更像传统 TUI，控制力更强，但要自己处理线程生命周期。

### 2.3 第三步：按键分发（KeyEvent → 状态机）

**Codex**（`app.rs:5838` → `chat_composer.rs:2463`）：

1. 全局快捷键优先：`Ctrl+T` transcript overlay、`Ctrl+L` 清屏、`Ctrl+G` 外部编辑器、`Esc Esc` backtrack。
2. 其余交给 `ChatComposer`；composer 先看 `ActivePopup`：

```rust
enum ActivePopup { None, Command(CommandPopup), File(FileSearchPopup), Skill(SkillPopup) }
```

3. 有弹窗时，`↑/↓/Tab/Enter/Esc` 先给弹窗；没有弹窗时才是编辑语义：`Enter` 提交、`Tab` 排队、`↑/↓` 历史、`Ctrl+K/Y` kill/yank……
4. 处理完统一 `sync_popups()`，保证弹窗可见性和文本/光标一致。

**CodeWhale**（`shell_key_routing.rs`）：

1. 先算 `Focus`：`Composer | TerminalPanel | Panel | Modal(kind) | Onboarding | Launch`。
2. 每个绑定声明自己的 `FocusScope`，由一张 `ShellBinding` 注册表统一匹配；footer 提示、帮助目录、实际 handler 共用同一份定义，避免“文档写了但没实现”。
3. 原则：**可打印字符永远归 composer**（源码里叫 TUI-DOG-002），裸 `j/k/g` 不做导航；只有 `Alt-*` / `Ctrl-*` / 功能键才可能是全局快捷键。
4. 内嵌终端聚焦时（`TerminalPanel`），`Enter`、`Tab`、`Ctrl-C` 全部转发给 PTY，`Esc` 才回到 composer。

### 2.4 第四步：渲染

- 两者都是 ratatui 的 Buffer diff 渲染 + 帧率限制器（Codex `frame_rate_limiter.rs`，CodeWhale `display_refresh.rs`）。
- Codex 的 `custom_terminal` 自己维护 `viewport_area`：初始锚定在光标位置（`custom_terminal.rs:197`），底部是 composer，上面是消息区；消息完成后通过 `insert_history_lines()` 写进**真实 scrollback**，所以退出后还能翻历史。
- CodeWhale 默认整屏 alt buffer；额外用 DEC 2026 synchronized output（`\x1b[?2026h` … `\x1b[?2026l`）包住每帧，GPU 终端上避免撕裂/闪烁；还会探测 OSC 11 背景色、kitty graphics、sixel。

### 2.5 第五步：恢复 / 让出终端

这部分是 TUI 最容易翻车的地方，两家都做了多层保护：

| 场景 | Codex | CodeWhale |
|---|---|---|
| 正常退出 | `restore()`：Pop keyboard flags → DisableBracketedPaste → DisableFocusChange → disable_raw_mode → Show cursor（`tui.rs:128`） | `TerminalCleanupGuard` + `leave_alt_screen()` + 逆序恢复各模式 |
| panic | `color-eyre` + panic hook 里 restore | `fatal_signal_guard` + panic hook 里 restore |
| `Ctrl-Z` | `job_control.rs` 挂起/恢复 | `job_control_guard` 在 raw mode 之前安装，保证每个已开启的模式都有 undo |
| 外部编辑器 | leave alt screen → restore modes → **drop EventStream** → `tcflush(STDIN)` → spawn `$EDITOR` → 反向恢复 | `with_suspended_tui` → pause input pump（等 ack）→ 恢复 → resume |
| 子进程读 stdin | 同上 | `pause_terminal_input_for_child()`，失败则拒绝启动子进程 |

---

## 3. Composer（输入框）是怎么实现的

### Codex

- `bottom_pane/textarea.rs`（2.5k 行）：UTF-8 + grapheme 感知的光标/删除/换行，软换行，`Ctrl+K` / `Ctrl+Y` 的单条 kill buffer；支持 `TextElement`（图片、技能等占位符，编辑时作为原子整体移动）；**没有鼠标选区，也没有 undo 栈**。
- `bottom_pane/chat_composer.rs`（8.2k 行）：真正的状态机
  - `ActivePopup` + `sync_popups()`；
  - paste burst 检测（见下）；
  - 历史：会话内完整 payload + `~/.codex/history.jsonl` 的纯文本历史；
  - 提交前 `prepare_submission_text()`：展开粘贴占位符、trim、重映射 element range、裁掉失效图片附件；
  - `input_enabled = false` 的只读态（审批/提问时占位提示）。
- paste burst（`paste_burst.rs`）：终端不支持 bracketed paste 时，把“快速连续的 Char + Enter”缓冲成一次 paste，避免粘贴到一半就发送；对 ASCII 会短暂 hold 第一个字符防闪烁，对非 ASCII/IME **不 hold**（否则中文输入会“掉字”）。

### CodeWhale

- `app/composer.rs`（2.1k 行）+ `composer_ui.rs` / `composer_chrome.rs`：选区、鼠标点击定位、拖拽选择、kill ring、`Ctrl-U` 清空后可 `Ctrl-Z` 恢复、`Ctrl-G` stash、`Alt-R` 历史搜索、grapheme 级移动/删除。
- 粘贴同样有 burst 启发式（`tui/paste.rs`），并且维护一份“可信终端名单”（Ghostty / iTerm / WezTerm / Apple_Terminal 等），只有确认会发 `Event::Paste` 的终端才关掉启发式；Windows 一律保留。
- 输入事件全部来自输入线程，composer 不直接读 stdin。

> 复杂度提示：如果 rsh 只想“能输入一行话”，这些都不需要；但只要想要 Codex 那种多行编辑 + 弹窗 + 粘贴 + 中文，**自研 composer 基本不可避免**。

---

## 4. 用户实际能用哪些方式“输入命令”

### 4.1 Codex

| 方式 | 触发 | 说明 |
|---|---|---|
| 自然语言 | 直接输入，`Enter` | 空闲时发送；有任务在跑时 `Enter` 入队，`Tab` 也是入队/提交 |
| 斜杠命令 | `/` | 弹出内置命令（model / approvals / new / resume / compact / diff / status / mcp / init / review…），`↑↓` 选择，`Tab`/`Enter` 执行 |
| 文件引用 | `@` | `FileSearchPopup`，模糊搜索工作区文件 |
| 技能 | `$` | `SkillPopup`，插入技能名 |
| Shell 命令 | `!cmd` | 不发给模型，直接本地执行；`Tab` 对 `!` 输入不触发提交 |
| 历史 | `↑` / `↓` | 会话内 + 跨会话 `history.jsonl` |
| 外部编辑器 | `Ctrl+G` | `$VISUAL`/`$EDITOR` 编辑长文本 |
| 图片 | `Ctrl+V` | 从剪贴板附加图片 |
| transcript | `Ctrl+T` | 进入 alt screen 查看完整记录 |
| 回溯 | `Esc Esc` | 选一条历史用户消息回退重编 |
| 快捷键帮助 | 空输入时 `?` | 底部弹出 shortcut overlay |

非交互入口：`codex exec "…"`。

Codex **没有**：鼠标捕获、命令面板、内置终端 dock、Hotbar。

### 4.2 CodeWhale

| 方式 | 触发 | 说明 |
|---|---|---|
| 自然语言 | 直接输入，`Enter` | 空闲发送；忙时入队；`Ctrl+Enter` 直接 steer 进当前 turn；忙时 500ms 内二次 `Enter` 也 steer |
| 斜杠命令 | `/` | 92 个内置命令 + 35 个技能；`Tab` 补全，`Ctrl-P/N` 或 `↑↓` 选择 |
| 命令面板 | `Ctrl+K` | 分 Action / Command / Skill / Tool / Mcp 五组搜索并执行 |
| 文件引用 | `@` | 文件路径 + 特殊 mention：`@git`（git status）、`@diff`（working tree diff） |
| 记忆 | `#` | `[memory] enabled` 时，`# foo` + Enter 直接追加到 memory 文件，不发送 turn |
| Shell 命令 | `!cmd` | 走正常审批 / 沙箱 / 输出面板 |
| 内嵌终端 | `/workbar terminal` 或 `Alt-W` | **真 PTY**：直接敲 shell 命令，`Enter/Tab/Ctrl-C` 发给 shell，`Esc` 回 composer，`Ctrl-N` 新 shell，`Alt-↑/↓` 切换，输出可滚动 |
| 帮助 | `F1` / `Ctrl-/` / `/help` | 198 条可搜索帮助目录（命令、快捷键、概念） |
| 外部编辑器 | `F4` / `Ctrl-Shift-O` | `$VISUAL`/`$EDITOR` |
| Hotbar | `Alt-1`…`Alt-8` | 可配置槽位，默认 workflow/goal/auto/plan/agent/operate/palette/side |
| 鼠标 | 拖拽 / 点击 / 右键 | 选区复制、点卡片、右键菜单、滚动 |
| 会话 | `Ctrl-R` | resume picker |

非交互 / 远程入口：`codewhale exec`、`serve`、`app-server`、`web`。

---

## 5. 键位对照（容易踩混的部分）

| 功能 | Codex | CodeWhale |
|---|---|---|
| 发送 | `Enter` | `Enter` |
| 换行 | `Ctrl+J` / `Shift+Enter`（增强键盘时） | `Shift+Enter` / `Alt+Enter` / `Ctrl+J` |
| 排队 | `Tab`（忙时） | `Enter`（忙时） |
| 打断/steer | `Esc` 取消 turn | `Ctrl+Enter` steer |
| 历史 | `↑` / `↓` | `↑` / `↓`，`Alt-R` 搜索 |
| 命令弹窗 | `↑↓` / `Tab` / `Enter` / `Esc` | 同左，另加 `Ctrl-P/N` |
| 外部编辑器 | `Ctrl+G` | `F4` / `Ctrl-Shift-O` |
| transcript | `Ctrl+T` | `Ctrl-Shift-T` |
| `Ctrl+T` | 打开 transcript | **循环 reasoning effort** |
| 退出 | `Ctrl+C`（再按一次）/ `Ctrl+D` | `Ctrl+D`（空输入）/ `Ctrl+C` 取消或清 draft |
| 清屏 | `Ctrl+L` | `/clear` |
| 命令面板 | 无 | `Ctrl+K` |
| 帮助 | `?`（shortcut overlay） | `F1` / `Ctrl-/` |

---

## 6. 两种设计取向的取舍

**Codex：终端原生、克制**

- 优点：不抢鼠标、不抢 scrollback，复制粘贴、滚轮、终端自带的选择都能用；退出后历史还在；输入层代码相对少。
- 缺点：命令发现性弱（要记 `/`、`!`、`@`、`$`）；长会话只能靠 `Ctrl+T` 进 alt screen；没有面板/多任务视图。

**CodeWhale：全屏工作台、功能密度高**

- 优点：命令面板 + 帮助目录让发现性好；鼠标、Hotbar、工作栏、内嵌终端把“agent 运行时还能自己敲命令”变成一等公民。
- 缺点：接管屏幕和鼠标，终端原生选择/滚动被替换；需要处理大量终端兼容矩阵（kitty / sixel / OSC 52 / Windows / tmux / Zellij）；`crates/tui` 10 万行级别，维护成本高。

**共同点**：raw mode + bracketed paste + Kitty 协议 + 自研 composer + 前缀弹窗 + paste burst + 外部编辑器让出 tty。这套组合基本就是“现代终端 agent”的标准答案。

---

## 7. 对 rsh 的落地建议

### 7.1 先定一个产品决策：inline 还是 fullscreen

- 想要“像 codex 一样，退出后历史还在、终端选择可用” → **inline viewport**，不要 `EnableMouseCapture`，alt screen 只给全屏 overlay。
- 想要“像 codewhale 一样，有面板/鼠标/内嵌终端” → **alt screen**，同时准备好 `/inline` 逃生口和一套“终端不支持时降级”的逻辑。

这个决定会影响 80% 的输入层代码，建议先定。

### 7.2 最小架构（按模块）

```
src/
  terminal/
    mod.rs        // enter/restore, panic hook, suspend/resume, 子进程让出 tty
    input.rs      // crossterm EventStream 或 reader 线程 -> AppEvent
  ui/
    composer.rs   // grapheme buffer, cursor, soft wrap, history, kill buffer
    popup.rs      // '/' '@' '#' 的候选列表 + 模糊过滤 + 键盘选择
    render.rs     // ratatui 布局与绘制
  app.rs          // 状态机：Focus / Modal / Popup / Composer
```

事件流建议：

```
终端字节 → crossterm 解析 → KeyEvent/Paste
        → 输入线程/EventStream
        → AppEvent 通道
        → dispatch（全局键 → 弹窗 → composer）
        → 更新状态
        → 标脏 + 帧调度
        → ratatui diff 绘制
```

### 7.3 MVP 顺序

1. raw mode + bracketed paste + 退出恢复 + panic hook（**先做恢复，再做界面**）。
2. 单行 composer：字符、Backspace、Enter 提交、Ctrl-C 退出。
3. 多行/软换行、grapheme、CJK 宽度、`Ctrl-J` 换行。
4. `/` 命令弹窗（先 3 个命令：`/help`、`/clear`、`/quit`）。
5. `!cmd` 本地 shell（审批/确认 + 输出面板）。
6. `@` 文件补全（可以先只做当前目录）。
7. 历史（内存 + `~/.rsh/history`）。
8. 外部编辑器（`Ctrl-G`，记得 drop/暂停输入源 + `tcflush`）。
9. 鼠标/命令面板/内嵌终端——按产品需要再加。

### 7.4 坑清单（按踩坑概率排序）

1. **paste burst**：不支持 bracketed paste 的终端（Windows、部分 SSH、tmux 配置）会把粘贴拆成按键；不做 burst 检测，多行粘贴会在第一个换行直接发送。
2. **IME / 中文**：crossterm 不提供 preedit，输入法组合由终端负责；但对非 ASCII 字符不要做“hold 第一个字符”的 paste 防抖，否则中文会掉字。CJK 宽度用 `unicode-width`，CodeWhale 甚至 patch 了它。
3. **终止/挂起恢复**：panic、`Ctrl-Z`、`kill -9` 之外的信号、SSH 断线，都要保证终端不留在 raw mode。至少装 panic hook + `SIGTERM`/`SIGHUP` guard。
4. **子进程抢 stdin**：`$EDITOR`、`less`、内嵌 shell 启动前必须停止读 stdin，否则按键会被两边分食；Codex 的做法是 drop EventStream，CodeWhale 是 pause + ack。
5. **alt screen 与 scrollback**：进了 alt screen，用户滚轮看不到历史；Zellij 干脆不支持 alt screen 的 scrollback。Codex 因此做了 `auto` 检测 + `--no-alt-screen`。
6. **Windows**：crossterm 的 Kitty flags 在 Windows 是 no-op；非 BMP 字符（emoji）粘贴会丢；`conhost` 的 OSC 8 终止符有问题。CodeWhale 为此打了三个平台补丁。
7. **大粘贴**：几十 KB 的粘贴要限流/截断，否则一帧渲染会卡死。Codex 超过 1000 字符的粘贴会折叠成占位符（`LARGE_PASTE_CHAR_THRESHOLD`），提交时再展开；CodeWhale 把超长粘贴截断预览，用户开始编辑时自动恢复全文。
8. **鼠标**：一旦 `EnableMouseCapture`，终端原生的选择/复制就没了，必须自己做选区 + OSC 52 复制，否则用户会骂。

### 7.5 测试建议

- `ratatui::backend::TestBackend` 做 composer/popup 的快照测试（Codex 有 `snapshots/`，CodeWhale 有 `golden_harness.rs`）。
- 用 `tmux` / `portable-pty` 做端到端：启动 → 输入 → 断言屏幕文本 → 退出后 `stty` 状态正常。
- 专门测：中文输入、emoji、多行粘贴、`Ctrl-C`、`Ctrl-Z`、`$EDITOR` 往返。

### 7.6 如果 rsh 只做 REPL

如果短期只需要“一行输入 + 上下历史 + 补全”，不必上 ratatui：`reedline` / `rustyline` 就能覆盖，成本低一个数量级。但一旦要做 Codex 式的多行 composer、`/` 弹窗、内联图片、流式输出，就应该按上面的 TUI 路线走——这两家产品也都是这么选的。

---

## 8. 证据索引

| 内容 | 位置 |
|---|---|
| Codex 终端模式设置 | `codex-rs/tui/src/tui.rs:63`（`set_modes`）、`:128`（`restore_common`） |
| Codex 事件流 | `codex-rs/tui/src/tui/event_stream.rs`、`app.rs:3831` |
| Codex 键分发 | `app.rs:5838`、`bottom_pane/chat_composer.rs:2463` |
| Codex composer 状态机 | `bottom_pane/chat_composer.rs`、`bottom_pane/paste_burst.rs` |
| Codex inline viewport | `custom_terminal.rs:186`、`tui.rs:412`、`lib.rs:1593` |
| Codex 快捷键表 | `bottom_pane/footer.rs:943` |
| CodeWhale 终端初始化 | `crates/tui/src/tui/ui/terminal.rs:697`、`ui/event_loop.rs:1016` |
| CodeWhale 输入线程 | `crates/tui/src/tui/ui/terminal_input.rs` |
| CodeWhale 键分发 | `crates/tui/src/tui/shell_key_routing.rs:32`（`Focus`） |
| CodeWhale 命令面板 | `crates/tui/src/tui/command_palette.rs` |
| CodeWhale 官方键位文档 | [docs/KEYBINDINGS.md](https://github.com/Hmbown/CodeWhale/blob/main/docs/KEYBINDINGS.md) |
| CodeWhale 官网 | <https://codewhale.net> |
| ratatui 官方菜谱（spawn vim / 事件处理） | <https://ratatui.rs/recipes/> |

> 说明：CodeWhale 源码通过稀疏 clone 到 `.research/cw` 后核对，行号对应 `main` 分支；Codex 行号对应本地 `cca36c568`。版本升级后行号可能偏移，以符号名为准。
