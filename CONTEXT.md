# rsh

rsh is a terminal coding agent. It talks to the user in a terminal UI, runs its own model/tool loop against a workspace, and can delegate isolated work to subagents, including an external DeepSeek Harness runtime.

## Language

**Agent**:
The rsh process that converses with the user and drives the tool loop.
_Avoid_: bot, assistant, model

**Turn**:
One user request and everything the agent does in response, until it stops or is cancelled.
_Avoid_: request, message, round

**Step**:
One model call plus the tool calls it requests.
_Avoid_: iteration, loop

**Tool Call**:
A model-requested invocation of a tool with JSON arguments.
_Avoid_: function call, action

**Tool Result**:
The output of a tool call, returned to the model as the next message.
_Avoid_: observation, output

**Session**:
A persisted conversation with an identity, replayable from its event log.
_Avoid_: conversation, chat, thread

**Compaction**:
Replacing older messages with a summary while keeping the recent turns intact.
_Avoid_: summarization, truncation

**Skill**:
A directory containing `SKILL.md` instructions that the agent can load on demand, together with its resource files.
_Avoid_: prompt, plugin

**Subagent**:
An isolated agent instance that receives one task and returns one result; it never talks to the user directly.
_Avoid_: child agent, worker, task

**Backend**:
The implementation used to run a subagent: in-process, or the external DeepSeek Harness reached over a protocol.
_Avoid_: provider, runner

**Workspace**:
The directory tree the agent is allowed to read and modify.
_Avoid_: project, cwd, repo

**Permission Mode**:
The policy that decides whether a tool action runs, asks the user, or is denied.
_Avoid_: sandbox, approval

**Composer**:
The multi-line input area at the bottom of the terminal UI.
_Avoid_: input box, prompt

**Transcript**:
The rendered record of the session in the terminal, scrolled into terminal history as it grows.
_Avoid_: history, log, output

**ACP**:
Agent Client Protocol, the JSON-RPC protocol used to drive the external DeepSeek Harness as a subagent backend.
_Avoid_: agent protocol, rpc
