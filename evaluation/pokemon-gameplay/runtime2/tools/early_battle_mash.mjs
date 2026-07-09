export default {
  name: "early_battle_mash",
  description:
    "Mash A through simple early-game wild/trainer battles and post-battle text, stopping when battle flag clears or batch limit is reached.",
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

    const log = [];
    let last;
    for (let i = 0; i < batches; i++) {
      const buttons = Array.from({ length: pressesPerBatch }, () => "a");
      last = await ctx.emulator.press(buttons, holdFrames, waitFrames);
      const s = last.state || {};
      const party = s.party || [];
      log.push({
        batch: i + 1,
        battle: !!s.battle,
        map: s.map_name || s.location || s.map || null,
        coords: s.position
          ? [s.position.x, s.position.y]
          : typeof s.x === "number" && typeof s.y === "number"
            ? [s.x, s.y]
            : null,
        top_party: party[0]
          ? {
              name: party[0].species || party[0].name,
              level: party[0].level,
              hp: party[0].hp,
              max_hp: party[0].max_hp,
            }
          : null,
      });
      if (!s.battle) break;
    }
    return { ...last, log };
  },
};
