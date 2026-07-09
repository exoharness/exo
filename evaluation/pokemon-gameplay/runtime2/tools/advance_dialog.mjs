export default {
  name: "advance_dialog",
  description:
    "Advance story dialog by pressing A repeatedly in batches, optionally with a final B press. Useful for Oak speeches, intro text, and routine NPC conversations.",
  parameters: {
    type: "object",
    properties: {
      presses: { type: "integer", minimum: 1, maximum: 40, default: 8 },
      hold_frames: { type: "integer", minimum: 1, maximum: 120, default: 6 },
      wait_frames: { type: "integer", minimum: 0, maximum: 600, default: 20 },
      use_b_last: { type: "boolean", default: false },
    },
    required: [],
  },
  async execute(args, ctx) {
    const presses = args.presses ?? 8;
    const holdFrames = args.hold_frames ?? 6;
    const waitFrames = args.wait_frames ?? 20;
    const useBLast = args.use_b_last ?? false;

    const log = [];
    for (let i = 0; i < presses; i++) {
      const button = useBLast && i === presses - 1 ? "b" : "a";
      const res = await ctx.emulator.press([button], holdFrames, waitFrames);
      const s = res.state || {};
      log.push({
        step: i + 1,
        button,
        map: s.map_name,
        map_id: s.map_id,
        x: s.x,
        y: s.y,
        battle: s.battle_type || s.in_battle || false,
        party_count: s.party?.length,
      });
      ctx.log(
        `step ${i + 1}: ${button} -> ${s.map_name || s.map_id} (${s.x},${s.y}) battle=${s.battle_type || s.in_battle || false}`,
      );
    }
    return { log };
  },
};
