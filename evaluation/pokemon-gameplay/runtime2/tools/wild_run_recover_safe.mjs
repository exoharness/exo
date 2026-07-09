export default {
  name: 'wild_run_recover_safe',
  description: 'More conservative wild-battle escape recovery: backs out with B, patiently advances send-out / transition text, then explicitly selects RUN and clears escape text. Useful when the battle may be stuck in ITEM or move submenus.',
  parameters: {
    type: 'object',
    properties: {
      attempts: { type: 'integer', minimum: 1, maximum: 8, default: 4 },
      intro_wait_frames: { type: 'integer', minimum: 1, maximum: 240, default: 72 },
      hold_frames: { type: 'integer', minimum: 1, maximum: 60, default: 6 },
      wait_frames: { type: 'integer', minimum: 0, maximum: 240, default: 18 },
      clear_presses: { type: 'integer', minimum: 1, maximum: 12, default: 4 }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const attempts = args.attempts ?? 4;
    const introWait = args.intro_wait_frames ?? 72;
    const hold = args.hold_frames ?? 6;
    const wait = args.wait_frames ?? 18;
    const clear = args.clear_presses ?? 4;

    let last = await ctx.emulator.tick(1);
    const log = [];

    for (let i = 0; i < attempts; i++) {
      const s = last.state || {};
      log.push({ phase: 'start_attempt', i, battle: s.battle, map: s.map_name, x: s.x, y: s.y });
      if (s.battle !== 'wild') {
        return { ok: true, finished: true, attempts_used: i, reason: 'battle_already_over', log, state: s };
      }

      // First, back out of any submenu state conservatively.
      last = await ctx.emulator.press(['b', 'b'], hold, wait);
      log.push({ phase: 'after_bb', i, battle: last.state?.battle, x: last.state?.x, y: last.state?.y });
      if (last.state?.battle !== 'wild') {
        return { ok: true, finished: true, attempts_used: i + 1, reason: 'battle_cleared_after_bb', log, state: last.state };
      }

      // Wait through send-out / transition lag, then single-A progression.
      last = await ctx.emulator.tick(introWait);
      log.push({ phase: 'after_wait', i, battle: last.state?.battle });
      if (last.state?.battle !== 'wild') {
        return { ok: true, finished: true, attempts_used: i + 1, reason: 'battle_cleared_during_wait', log, state: last.state };
      }

      last = await ctx.emulator.press(['a'], hold, wait);
      log.push({ phase: 'after_single_a', i, battle: last.state?.battle });
      if (last.state?.battle !== 'wild') {
        return { ok: true, finished: true, attempts_used: i + 1, reason: 'battle_cleared_after_single_a', log, state: last.state };
      }

      // Explicit RUN selections from common possible menu positions.
      const runTries = [
        ['down', 'right', 'a'],
        ['right', 'a'],
        ['down', 'a']
      ];
      for (const seq of runTries) {
        last = await ctx.emulator.press(seq, hold, wait);
        log.push({ phase: 'after_run_try', i, seq, battle: last.state?.battle });
        if (last.state?.battle !== 'wild') {
          return { ok: true, finished: true, attempts_used: i + 1, reason: 'battle_cleared_after_run_input', log, state: last.state };
        }

        // Clear possible 'Got away safely!' or failure text.
        for (let j = 0; j < clear; j++) {
          last = await ctx.emulator.press(['a'], hold, wait);
          if (last.state?.battle !== 'wild') {
            log.push({ phase: 'cleared_after_a', i, j, battle: last.state?.battle });
            return { ok: true, finished: true, attempts_used: i + 1, reason: 'battle_cleared_during_text_clear', log, state: last.state };
          }
        }

        last = await ctx.emulator.tick(48);
        if (last.state?.battle !== 'wild') {
          log.push({ phase: 'cleared_after_post_wait', i, battle: last.state?.battle });
          return { ok: true, finished: true, attempts_used: i + 1, reason: 'battle_cleared_after_post_wait', log, state: last.state };
        }
      }
    }

    return { ok: false, finished: false, attempts_used: attempts, reason: 'battle_still_active', log, state: last.state };
  }
};