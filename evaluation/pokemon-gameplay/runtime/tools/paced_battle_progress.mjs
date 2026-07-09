export default {
  name: 'paced_battle_progress',
  description: 'Advance slow early-game battle text with a cautious cadence: wait, then single A presses, stopping if the battle ends or a command/menu state appears to have returned.',
  parameters: {
    type: 'object',
    properties: {
      cycles: { type: 'integer', minimum: 1, maximum: 20, default: 6 },
      wait_frames: { type: 'integer', minimum: 1, maximum: 240, default: 90 },
      hold_frames: { type: 'integer', minimum: 1, maximum: 60, default: 6 },
      post_wait_frames: { type: 'integer', minimum: 0, maximum: 240, default: 24 }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const cycles = args.cycles ?? 6;
    const waitFrames = args.wait_frames ?? 90;
    const holdFrames = args.hold_frames ?? 6;
    const postWaitFrames = args.post_wait_frames ?? 24;

    const log = [];
    let last = await ctx.emulator.frame();
    log.push({ step: 'start', battle: last.state?.battle });

    for (let i = 0; i < cycles; i++) {
      if (last.state?.battle === 'none') {
        log.push({ step: 'battle_cleared_before_cycle', cycle: i });
        break;
      }

      last = await ctx.emulator.tick(waitFrames);
      log.push({ step: 'wait', cycle: i + 1, battle: last.state?.battle, text: last.state?.textbox_id ?? null });
      if (last.state?.battle === 'none') {
        log.push({ step: 'battle_cleared_after_wait', cycle: i + 1 });
        break;
      }

      last = await ctx.emulator.press(['a'], holdFrames, postWaitFrames);
      log.push({ step: 'a_press', cycle: i + 1, battle: last.state?.battle, text: last.state?.textbox_id ?? null });
      if (last.state?.battle === 'none') {
        log.push({ step: 'battle_cleared_after_a', cycle: i + 1 });
        break;
      }
    }

    return { state: last.state, screen_hash: last.screen_hash, log };
  }
};