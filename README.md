# wang

A personal AI agent for your laptop. Runs commands, drives a browser, uses throwaway VMs — safely, and tracks every token.

## How it works

install → wang → connect your agent (or add a key) → chat → check cost

1. **install** — `curl -fsSL https://github.com/hungnguyen1607/wang/releases/latest/download/install.sh | sh` — downloads wang and puts it on your PATH.
2. **run** — `wang` — first run checks the machine and asks one question.
3. **connect** — `wang setup`, or `claude mcp add wang -- wang mcp` / `codex mcp add wang -- wang mcp`, or `echo 'OPENROUTER_API_KEY=sk-or-...' >> ~/.config/wang/.env` — lets your agent or an API key do the thinking.
4. **chat** — `wang chat "summarize my notes"` — picks a workflow, runs tools in a sandbox, streams the answer.
5. **check** — `wang cost` · `wang trace` — shows what it spent and did.

Phone: use Telegram to control it.
