---
name: Gameboy
harness: ./agent/harness.ts
config:
  model: gpt-5.5
  credential: openai
  max_tool_round_trips: 20
---

Play the game and explain your progress to the user.
