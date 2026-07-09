export default {
  name: "service_desk_probe",
  description:
    "Move through likely service-counter tiles, press A from a chosen facing at each stop, then tap B a few times to clear text safely. Useful for Nurse Joy / shop-counter scanning without accidental long A-mash loops.",
  parameters: {
    type: "object",
    properties: {
      moves: {
        type: "array",
        items: { type: "string", enum: ["up", "down", "left", "right"] },
        description:
          "Movement buttons to press one at a time before each interaction test.",
      },
      interact_facing: {
        type: "string",
        enum: ["up", "down", "left", "right"],
        description: "Direction to face immediately before pressing A.",
        default: "up",
      },
      hold_frames: {
        type: "number",
        default: 10,
      },
      wait_frames: {
        type: "number",
        default: 30,
      },
      clear_b_presses: {
        type: "number",
        default: 3,
        description:
          "How many single B presses to use after each A interaction attempt.",
      },
    },
    required: ["moves"],
  },
  async execute(args, ctx) {
    const moves = args.moves || [];
    const interactFacing = args.interact_facing || "up";
    const hold = args.hold_frames ?? 10;
    const wait = args.wait_frames ?? 30;
    const clearB = args.clear_b_presses ?? 3;
    const log = [];

    const snap = async (label) => {
      const res = await ctx.emulator.frame();
      const s = res.state || {};
      log.push({
        label,
        map: s.map_name || s.map || null,
        x: s.x ?? null,
        y: s.y ?? null,
        facing: s.facing ?? null,
        battle: s.battle_flag ?? s.in_battle ?? null,
        screen_hash: res.screen_hash || null,
      });
    };

    await snap("start");
    for (let i = 0; i < moves.length; i++) {
      const mv = moves[i];
      await ctx.emulator.press([mv], hold, wait);
      await snap(`move_${i + 1}_${mv}`);
      if (interactFacing !== mv) {
        await ctx.emulator.press([interactFacing], hold, wait);
        await snap(`face_${i + 1}_${interactFacing}`);
      }
      await ctx.emulator.press(["a"], 6, 20);
      await snap(`a_${i + 1}`);
      for (let j = 0; j < clearB; j++) {
        await ctx.emulator.press(["b"], 6, 20);
        await snap(`b_${i + 1}_${j + 1}`);
      }
    }
    ctx.log(`service_desk_probe completed ${moves.length} probes`);
    return { ok: true, probes: moves.length, log };
  },
};
