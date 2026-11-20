# rsh 代理指南

## 项目说明

rsh 是一个使用 Rust 2024 编写的终端 coding agent，提供自研模型/工具循环、inline 终端 UI、JSONL 会话持久化、Skills，以及 native/dsh 两种子代理后端。异步运行时采用 Tokio；模型客户端使用 OpenAI 兼容的 Chat Completions 协议。

引入或重命名领域概念前，先阅读 `CONTEXT.md`。修改 viewport、会话格式、权限模型或子代理架构前，先阅读 `docs/adr/` 中相关的 ADR。

## 构建与验证

```bash
cargo build
cargo fmt --check
cargo test
```

在开发机上，如果 Cargo 需要访问网络，先运行 `source scripts/dev-env.sh`；该脚本会配置本地 IPv4 代理，以及工作区内的 Cargo 和临时文件目录。

常规测试必须保持确定性：不得依赖真实模型、API Key、网络服务或人工输入。live 测试会消耗模型 Token，仅在用户明确要求时运行：

```bash
RSH_LIVE=1 cargo test --test e2e_project -- --ignored
RSH_LIVE=1 cargo test --test e2e_grill_me -- --nocapture
```

测试临时数据放在 `target/test-tmp` 下。删除本次运行产生、但不属于任务交付内容的文件。

## 模块地图

- `src/agent/`：Turn/Step 循环、工具结果反馈、用量统计和上下文压缩。
- `src/llm/`：OpenAI 兼容请求类型、SSE 流式响应、重试和取消。
- `src/tools/`：Tool trait、注册表、权限、审批和内置工具。
- `src/session/`：版本化 JSONL 事件、会话回放和会话查找。
- `src/skills.rs`：全局/项目技能发现和 frontmatter 解析。
- `src/subagent/`：native 执行器、外部 dsh 执行器和 ACP 客户端。
- `src/tui/`：终端生命周期、输入状态、渲染、用户提问和审批。
- `src/config.rs`、`src/prompt.rs`：运行时配置和系统提示词组装。
- `tests/`：闭集契约、工具语义、PTY 测试和显式启用的 live 测试。

## Rust 生产代码要求

- 非测试生产代码中禁止使用 `unwrap()`、`expect()` 或 `panic!()`。返回结构化错误，并在应用和传输边界保留错误上下文。
- 通过现有工具错误/结果路径返回工具失败，使模型能够恢复；只有传输错误和循环错误才应中止 Turn。
- 保留取消和超时行为。Unix Shell 执行必须保留进程组清理，确保被取消或超时命令的后代进程不会继续运行。
- 遵循邻近代码的命名、错误处理和模块边界。
- 修改代码逻辑时，注意更新函数或者模块注释
- 模块间通信优先使用 flume channel 的异步接口

## 行为约束

- 主 Agent 固定暴露九个工具；native 子代理固定暴露七个工具。子代理不得递归创建子代理，也不得直接向用户提问。如果有意修改工具集，必须同步更新注册表、提示词、契约测试和用户文档。
- 默认权限模式有意设为 `danger-full-access`。`ask` 需要交互式审批界面，`read-only` 拒绝写操作。所有模式都拒绝提权命令。`!cmd` 快捷方式必须与模型请求的 Bash 调用经过相同的权限检查。
- 工具层权限不是操作系统级沙箱，不得将其描述为沙箱。
- 会话事件采用 append-only 方式写入，并支持回放。保持与现有 JSONL 日志兼容，包括上下文压缩和未知事件处理。
- inline viewport 必须保留不使用 alternate screen 的全屏降级方案。所有退出路径都必须恢复终端状态。
- 默认配置路径为 `~/.rsh/config.toml`，可由 `RSH_CONFIG` 覆盖。会话默认保存在 `~/.rsh/sessions/` 下，可由 `RSH_HOME` 覆盖根目录。

## 测试要求

- 断言返回内容，而不是只断言结果数量。仅有数量无法证明返回了正确内容。
- 过滤测试使用互斥的匹配与不匹配数据，既证明匹配内容被返回，也证明不匹配内容确实没有返回。
- 对预期结果使用严格相等断言，不得用 `>=` 或 `<=` 弱化精确断言。
- 在正向用例之外补充反向用例：非法输入、歧义编辑、拒绝操作、未知名称和不匹配搜索，都必须产生规定的错误或确认结果不存在。
- 尽可能验证真实行为：工具测试使用真实临时文件，HTTP/PTY 流程使用本地假模型；默认测试套件不得访问真实 API。
- 单元测试放在对应代码旁，跨模块契约测试放在 `tests/`。行为有意变化时更新现有测试，不得保留过时预期。

## 文档要求

用户可见的命令、配置或行为发生变化时，更新 `README.md`。领域语言变化时更新 `CONTEXT.md`；架构决策变化时新增或修订 ADR。保持开发计划和设计规格与实际实现一致。
