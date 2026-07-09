export default {
  name: 'early_wild_run_opening',
  description: 'Cautiously escape an opening wild battle by advancing intro text with waits, then selecting RUN explicitly; useful for slow Route 2 Weedle/Kakuna/Pidgey starts.',
  parameters: {
    type: 'object',
    properties: {
      cycles: { type: 'integer', minimum: 1, maximum: 12, default: 6 },
      intro_wait_frames: { type: 'integer', minimum: 1, maximum: 240, default: 48 },
      hold_frames: { type: 'integer', minimum: 1, maximum: 60, default: 6 },
      wait_frames: { type: 'integer', minimum: 0, maximum: 180, default: 18 },
      post_escape_wait_frames: { type: 'integer', minimum: 0, maximum: 240, default: 72 }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const cycles = args.cycles ?? 6;
    const introWait = args.intro_wait_frames ?? 48;
    const hold = args.hold_frames ?? 6;
    const wait = args.wait_frames ?? 18;
    const postEscape = args.post_escape_wait_frames ?? 72;

    const out = [];
    const battleOn = (s) => {
      const b = s?.battle;
      return !(b === 'none' || b === null || b === undefined || b === false);
    };

    let res = await ctx.emulator.tick(introWait);
    out.push({ step: 'initial_wait', battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
    if (!battleOn(res.state)) return { ok: true, escaped: true, log: out, state: res.state };

    for (let i = 0; i < cycles; i++) {
      res = await ctx.emulator.press(['a'], hold, wait);
      out.push({ step: `advance_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
      if (!battleOn(res.state)) {
        await ctx.emulator.tick(postEscape);
        return { ok: true, escaped: true, log: out, state: res.state };
      }

      res = await ctx.emulator.tick(introWait);
      out.push({ step: `settle_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
      if (!battleOn(res.state)) return { ok: true, escaped: true, log: out, state: res.state };

      res = await ctx.emulator.press(['down','right','a'], hold, wait);
      out.push({ step: `run_attempt_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
      if (!battleOn(res.state)) {
        res = await ctx.emulator.tick(postEscape);
        out.push({ step: `post_escape_wait_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
        return { ok: true, escaped: !battleOn(res.state), log: out, state: res.state };
      }

      res = await ctx.emulator.press(['a'], hold, wait);
      out.push({ step: `clear_after_run_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
      if (!battleOn(res.state)) {
        res = await ctx.emulator.tick(postEscape);
        out.push({ step: `post_clear_wait_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
        return { ok: true, escaped: !battleOn(res.state), log: out, state: res.state };
      }

      res = await ctx.emulator.press(['b'], hold, wait);
      out.push({ step: `backout_${i+1}`, battle: res.state?.battle, map: res.state?.map, x: res.state?.x, y: res.state?.y });
      if (!battleOn(res.state)) return { ok: true, escaped: true, log: out, state: res.state };
    }

    return { ok: false, escaped: false, log: out, state: res.state };
  }
};