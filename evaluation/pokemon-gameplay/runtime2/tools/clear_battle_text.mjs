export default {
  name: "clear_battle_text",
  description:
    "Press A in batches to clear battle/result/level-up text until the battle flag drops or a batch limit is reached.",
  parameters: {
    type: "object",
    properties: {
      batches: { type: "integer", minimum: 1, maximum: 20, default: 6 },
      presses_per_batch: {
        type: "integer",
        minimum: 1,
        maximum: 20,
        default: 6,
      },
      hold_frames: { type: "integer", minimum: 1, maximum: 120, default: 6 },
      wait_frames: { type: "integer", minimum: 0, maximum: 600, default: 18 },
    },
    additionalProperties: false,
  },
  async execute(args, ctx) {
    const batches = args.batches ?? 6;
    const pressesPerBatch = args.presses_per_batch ?? 6;
    const holdFrames = args.hold_frames ?? 6;
    const waitFrames = args.wait_frames ?? 18;

    const out = [];
    let state = await ctx.frame();
    out.push({
      step: "start",
      battle: state.state?.battle_type ?? state.state?.battle ?? null,
      screen_hash: state.screen_hash,
    });

    for (let i = 0; i < batches; i++) {
      state = await ctx.frame();
      const battle = state.state?.battle_type ?? state.state?.battle ?? null;
      if (!battle || battle === "none" || battle === 0) {
        out.push({
          step: `stopped_before_batch_${i + 1}`,
          battle,
          screen_hash: state.screen_hash,
        });
        return {
          ok: true,
          stopped: "battle_cleared",
          log: out,
          state: state.state,
          screen_hash: state.screen_hash,
        };
      }
      const buttons = Array.from({ length: pressesPerBatch }, () => "a");
      state = await ctx.emulator.press(buttons, holdFrames, waitFrames);
      out.push({
        step: `batch_${i + 1}`,
        battle: state.state?.battle_type ?? state.state?.battle ?? null,
        screen_hash: state.screen_hash,
      });
    }

    state = await ctx.frame();
    return {
      ok: true,
      stopped: "batch_limit",
      log: out,
      state: state.state,
      screen_hash: state.screen_hash,
    };
  },
};
