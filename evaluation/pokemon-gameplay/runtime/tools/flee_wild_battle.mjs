export default {
  name: 'flee_wild_battle',
  description: 'Attempt to run from a simple wild battle by opening RUN (down, right, A), then mash through the result text until battle clears or batch limit is reached.',
  parameters: {
    type: 'object',
    properties: {
      attempts: { type: 'integer', minimum: 1, maximum: 8, default: 3 },
      clear_presses: { type: 'integer', minimum: 1, maximum: 20, default: 6 },
      hold_frames: { type: 'integer', minimum: 1, maximum: 120, default: 6 },
      wait_frames: { type: 'integer', minimum: 0, maximum: 600, default: 18 }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const attempts = args?.attempts ?? 3;
    const clearPresses = args?.clear_presses ?? 6;
    const hold = args?.hold_frames ?? 6;
    const wait = args?.wait_frames ?? 18;

    const log = [];
    let last = await ctx.emulator.frame();
    log.push({ step: 'start', battle: last.state?.battle?.in_battle ?? last.state?.in_battle ?? null, screen_hash: last.screen_hash });

    for (let i = 0; i < attempts; i++) {
      last = await ctx.emulator.press(['down', 'right', 'a'], hold, wait);
      const inBattleAfterRunInput = last.state?.battle?.in_battle ?? last.state?.in_battle ?? null;
      log.push({ step: `run_input_${i + 1}`, battle: inBattleAfterRunInput, screen_hash: last.screen_hash });

      for (let j = 0; j < clearPresses; j++) {
        last = await ctx.emulator.press(['a'], hold, wait);
        const inBattle = last.state?.battle?.in_battle ?? last.state?.in_battle ?? null;
        log.push({ step: `clear_${i + 1}_${j + 1}`, battle: inBattle, screen_hash: last.screen_hash });
        if (inBattle === false) {
          return { ok: true, escaped: true, log, state: last.state, screen_hash: last.screen_hash };
        }
      }
    }

    return {
      ok: true,
      escaped: (last.state?.battle?.in_battle ?? last.state?.in_battle ?? null) === false,
      log,
      state: last.state,
      screen_hash: last.screen_hash
    };
  }
};