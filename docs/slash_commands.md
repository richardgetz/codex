# Slash commands

For an overview of Codex CLI slash commands, see [this documentation](https://developers.openai.com/codex/cli/slash-commands).

## Fork-only voice commands

This fork adds `/mic` and `/voice` for its native GPT-Live voice mode. See the
[fork differences](./fork-differences.md#gpt-live-voice-in-the-native-tui)
page for the complete command and `config.toml` reference. Use `/mic help` or
`/voice help` inside the TUI to print the available controls; `/voice debug` is
an opt-in, session-local handoff-effort diagnostic and is off by default.
