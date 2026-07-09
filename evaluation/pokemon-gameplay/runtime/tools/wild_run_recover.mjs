export default {
  name: 'wild_run_recover',
  description: 'Try to recover from messy wild-battle substates by backing out with B, then attempting common RUN selections and clearing the escape text until battle ends or attempts are exhausted.',
  parameters: {
    type: 'object',
    properties: {
      attempts: { type: 'integer', minimum: 1, maximum: 8, default: 4 },
      hold_frames: { type: 'integer', minimum: 1, maximum: 120, default: 6 },
      wait_frames: { type: 'integer', minimum: 0, maximum: 600, default: 18 },
      clear_presses: { type: 'integer', minimum: 1, maximum: 20, default: 4 }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const attempts = args.attempts ?? 4;
    const hold = args.hold_frames ?? 6;
    const wait = args.wait_frames ?? 18;
    const clearPresses = args.clear_presses ?? 4;

    const log = [];
    const patterns = [
      ['b', 'down', 'a'],
      ['b', 'right', 'a'],
      ['b', 'down', 'right', 'a'],
      ['b', 'b', 'down', 'a'],
      ['b', 'b', 'right', 'a'],
      ['b', 'b', 'down', 'right', 'a']
    ];

    let last;
    for (let i = 0; i < attempts; i++) {
      const pattern = patterns[i % patterns.length];
      last = await ctx.emulator.press(pattern, hold, wait);
      log.push({ phase: 'attempt', i: i + 1, pattern, battle: last.state?.battle });
      if (!last.state?.battle) return { ok: true, escaped: true, log, state: last.state, screen_hash: last.screen_hash };

      last = await ctx.emulator.press(Array(clearPresses).fill('a'), hold, wait);
      log.push({ phase: 'clear', i: i + 1, presses: clearPresses, battle: last.state?.battle });
      if (!last.state?.battle) return { ok: true, escaped: true, log, state: last.state, screen_hash: last.screen_hash };
    }

    return { ok: false, escaped: false, log, state: last?.state, screen_hash: last?.screen_hash };
  }
};