export default {
  name: 'clear_battle_text_safe',
  description: 'Press A in batches to clear battle/result/level-up text until battle ends or a batch limit is reached. Avoids frame APIs and relies only on returned state.',
  parameters: {
    type: 'object',
    properties: {
      batches: { type: 'integer', minimum: 1, maximum: 30, default: 8 },
      presses_per_batch: { type: 'integer', minimum: 1, maximum: 20, default: 6 },
      hold_frames: { type: 'integer', minimum: 1, maximum: 120, default: 6 },
      wait_frames: { type: 'integer', minimum: 0, maximum: 600, default: 18 }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const batches = args.batches ?? 8;
    const pressesPerBatch = args.presses_per_batch ?? 6;
    const holdFrames = args.hold_frames ?? 6;
    const waitFrames = args.wait_frames ?? 18;

    const logs = [];
    let last = null;

    for (let i = 0; i < batches; i++) {
      last = await ctx.emulator.press(Array(pressesPerBatch).fill('a'), holdFrames, waitFrames);
      const state = last?.state ?? {};
      const battle = state?.battle ?? null;
      const loc = state?.location ?? null;
      const x = state?.x ?? state?.coords?.x ?? null;
      const y = state?.y ?? state?.coords?.y ?? null;
      logs.push({ batch: i + 1, battle, location: loc, x, y, screen_hash: last?.screen_hash ?? null });
      ctx.log(`batch ${i + 1}: battle=${battle} loc=${loc} x=${x} y=${y}`);
      if (battle === 'none' || battle === 0 || battle === false || battle == null) {
        return { ok: true, stopped_reason: 'battle_cleared_or_not_in_battle', logs, last_state: state };
      }
    }

    return { ok: true, stopped_reason: 'batch_limit_reached', logs, last_state: last?.state ?? null };
  }
};