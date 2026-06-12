# Discord Voice (Design)

Join a Discord voice channel and hold a spoken conversation with Exoclaw — speak
to it, hear it reply — with one config knob and no new API keys. Implementation
lives in `examples/exoclaw/adapters/discord/voice.ts`; operator setup is in that
adapter's `README.md`.

## Principle

Exoclaw adapters are transports; the agent is the brain. Voice follows the same
rule: it is a **microphone and speaker bolted onto the existing text-message
pipe**, not a second brain. A spoken turn becomes a normal inbound `message`
event; a spoken reply is a normal outbound `send_adapter_message`.

The consequence is the load-bearing design decision: **all audio lives in the
Discord worker.** The Rust adapter runtime, the worker protocol, the agent
tools, and the turn loop are unchanged — audio never reaches Rust. Voice reuses
the agent's tools, identity, and history for free because, by the time Rust sees
it, a voice turn is indistinguishable from a typed one.

This is the pipeline approach (STT → agent → TTS). A realtime speech-to-speech
bridge is deliberately out of scope: it would be a separate brain that does not
reuse Exoclaw's tools, identity, or history without significant extra plumbing.
It can be added later as an isolated mode without touching this design.

## Data path

One spoken turn:

```
User speaks
  -> Worker: per-speaker Opus receive (@discordjs/voice)
  -> segment on trailing silence (drop sub-0.4s blips)
  -> decode to PCM, wrap as WAV
  -> OpenAI STT
  -> emit `message` event { target = voiceChannelId, metadata.source = "voice" }
  -> Rust runtime wakes the conversation (identical to a typed message)
  -> Exoclaw agent turn (tools, identity, history)
  -> send_adapter_message { target = voiceChannelId }
  -> Outbox -> Worker sees target is an active voice session
  -> OpenAI TTS -> play into the voice channel (+ post text as transcript)
```

**Inbound.** The worker joins voice with `@discordjs/voice`, captures per-speaker
Opus, and segments utterances on trailing silence (`receiver.speaking` plus an
`AfterSilence` end-behavior; sub-0.4s utterances are dropped as coughs/blips).
Each utterance is decoded to PCM, wrapped as WAV in-process, transcribed by
OpenAI STT, and emitted as a normal `message` event whose `target` is the
**voice channel id** and whose metadata carries `source: "voice"`. Rust handles
it exactly like a typed message.

**Outbound.** The worker maps `voiceChannelId -> active voice session`. When a
`send_adapter_message` arrives whose `target` is an active voice channel, the
worker synthesizes it with OpenAI TTS and plays it into the connection. The reply
text is **also** posted to the channel, so every voice turn has an inspectable
text transcript. If the user starts speaking during playback, playback stops
(barge-in).

Because the inbound event's `target` is the voice channel id and the agent is
told (in `prompts/me.md`) to reply to the inbound target, replies route back to
voice with **no new tool and no protocol change**. Voice composes with
conversation target-scoping: a voice channel is just another target, so with
`conversationScope: "target"` a voice channel gets its own conversation.

## Control

- `/voice join` — join the caller's current voice channel.
- `/voice leave` — leave the voice channel.
- The bot auto-leaves when the channel empties (no humans left).

Slash commands are handled entirely in the worker and never involve the model.
They require the `applications.commands` scope and `Connect`/`Speak` permissions.

## Models and keys

STT and TTS both use the existing `openai` secret, bound into the worker as
`OPENAI_API_KEY` — no new key. The Rust adapter adds that secret binding to the
worker environment only when `voice` is enabled, so text-only adapters need no
OpenAI key. Defaults (not configurable): `gpt-4o-mini-transcribe` for STT,
`gpt-4o-mini-tts` with the `alloy` voice for TTS.

## Configuration

Two adapter-config fields, both surfaced on `create_adapter`:

- `voice` (boolean, default `false`) — enable voice.
- `openaiSecretId` (string, default `"openai"` when voice is on) — the secret
  holding the OpenAI key for STT/TTS.

Everything else — models, voice, silence window — is hardcoded to sane defaults.
Voice is meant to be one knob, not a panel.

> **Schema note.** `voice` and `openaiSecretId` are listed in the
> `create_adapter` Discord schema's `required[]`. OpenAI strict function-calling
> rejects any tool whose `required` omits a declared property; a property added
> to `properties` but not `required` 400s **every** model turn (not just
> adapter creation). `openaiSecretId` is nullable so the model can pass `null`
> when `voice` is false.

Additional requirements vs. the text-only adapter:

- The `GuildVoiceStates` gateway intent (non-privileged; added automatically
  when `voice` is enabled).
- `Connect` and `Speak` bot permissions, and the `applications.commands` scope.
- An OpenAI secret (`exo secret set openai --env OPENAI_API_KEY`).

## Non-goals (v1)

- **Realtime speech-to-speech.** The pipeline is assistant-grade latency
  (~seconds per turn, more when tools run), not phone-call-grade. Lower latency
  and mid-sentence interruption would need a separate realtime bridge.
- **Ambient "thinking" audio / multi-source mixing.** Not needed for a working
  conversation.
- **Wake-word gating.** While joined, every utterance is a turn.

## Dependencies

`@discordjs/voice`, `prism-media`, `opusscript` (pure-JS Opus codec — no native
build step), and `sodium-native` (voice encryption, prebuilt binary). No ffmpeg:
inbound Opus is decoded to PCM and wrapped as WAV in-process, and TTS is fetched
as OGG/Opus and played directly via `StreamType.OggOpus`.
